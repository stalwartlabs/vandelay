/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::thread;

use indexmap::IndexSet;
use serde::de::DeserializeOwned;
use serde::ser::{Serialize, SerializeMap, Serializer};
use serde_json::{Map, Value};

use crate::jmap::connect::JmapAccount;
use crate::jmap::error::JmapError;
use crate::jmap::http::JmapClient;
use crate::jmap::session::Limits;
use crate::jmap::wire::JmapId;
use crate::net::retry::{Disposition, RetryWait};

pub const URN_CORE: &str = "urn:ietf:params:jmap:core";

pub const URN_VACATION_RESPONSE: &str = "urn:ietf:params:jmap:vacationresponse";

const ANCHOR_RESTARTS: u32 = 2;

type CapabilityUrn = &'static str;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodCallKind {
    Read,
    SingleObjectWrite,
}

impl MethodCallKind {
    pub fn disposition(self, error_type: &str) -> Disposition {
        match (error_type, self) {
            ("serverUnavailable", _) | ("serverPartialFail", MethodCallKind::Read) => {
                Disposition::Retry
            }
            _ => Disposition::Fatal,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MethodCall {
    pub name: String,
    pub args: Value,
    pub call_id: String,
}

impl Serialize for MethodCall {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (&self.name, &self.args, &self.call_id).serialize(serializer)
    }
}

impl MethodCall {
    pub fn check_error(&self) -> Result<(), JmapError> {
        if self.name != "error" {
            return Ok(());
        }
        let error_type = self
            .args
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if let Some(known) = hashify::map!(
            error_type.as_bytes(),
            KnownMethodError,
            "anchorNotFound" => KnownMethodError::AnchorNotFound,
            "cannotCalculateChanges" => KnownMethodError::CannotCalculateChanges,
            "unknownMethod" => KnownMethodError::UnknownMethod,
        ) {
            return Err(known.error());
        }
        Err(JmapError::Method {
            call_id: self.call_id.clone(),
            error_type: error_type.to_owned(),
            description: self
                .args
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }

    fn decode_get<T: DeserializeOwned>(&self, out: &mut GetResult<T>) -> Result<(), JmapError> {
        let list = self
            .args
            .get("list")
            .and_then(Value::as_array)
            .ok_or_else(|| JmapError::malformed("get response has no list array"))?;
        out.list.reserve(list.len());
        for item in list {
            out.list.push(T::deserialize(item)?);
        }
        if let Some(not_found) = self.args.get("notFound").and_then(Value::as_array) {
            out.not_found.extend(
                not_found
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|s| JmapId(s.to_owned())),
            );
        }
        if let Some(state) = self.args.get("state").and_then(Value::as_str) {
            out.state = Some(state.to_owned());
        }
        Ok(())
    }

    fn decode_set(&self) -> SetOutcome {
        let pairs = |key: &str| -> Vec<(String, Value)> {
            self.args
                .get(key)
                .and_then(Value::as_object)
                .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default()
        };
        SetOutcome {
            created: pairs("created"),
            updated: self
                .args
                .get("updated")
                .and_then(Value::as_object)
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default(),
            destroyed: self
                .args
                .get("destroyed")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            not_created: pairs("notCreated"),
            not_updated: pairs("notUpdated"),
            not_destroyed: pairs("notDestroyed"),
        }
    }

    fn append_ids(&self, key: &str, out: &mut Vec<JmapId>) {
        if let Some(arr) = self.args.get(key).and_then(Value::as_array) {
            out.extend(
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(|s| JmapId(s.to_owned())),
            );
        }
    }
}

#[derive(Clone, Copy)]
enum KnownMethodError {
    AnchorNotFound,
    CannotCalculateChanges,
    UnknownMethod,
}

impl KnownMethodError {
    fn error(self) -> JmapError {
        match self {
            KnownMethodError::AnchorNotFound => JmapError::AnchorNotFound,
            KnownMethodError::CannotCalculateChanges => JmapError::CannotCalculateChanges,
            KnownMethodError::UnknownMethod => JmapError::UnknownMethod,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Request {
    calls: Vec<MethodCall>,
}

impl Serialize for Request {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("methodCalls", &self.calls)?;
        map.serialize_entry("using", &self.using())?;
        map.end()
    }
}

impl Request {
    pub fn new() -> Request {
        Request { calls: Vec::new() }
    }

    pub fn call(
        &mut self,
        name: impl Into<String>,
        args: Value,
        call_id: impl Into<String>,
    ) -> &mut Request {
        self.calls.push(MethodCall {
            name: name.into(),
            args,
            call_id: call_id.into(),
        });
        self
    }

    pub fn len(&self) -> usize {
        self.calls.len()
    }

    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    pub fn using(&self) -> IndexSet<&'static str> {
        let mut set = IndexSet::with_capacity(self.calls.len() + 1);
        set.insert(URN_CORE);
        for call in &self.calls {
            set.insert(Request::capability_of(&call.name));
        }
        set
    }

    pub fn capability_of(method: &str) -> &'static str {
        let prefix = method.split_once('/').map_or(method, |(prefix, _)| prefix);
        hashify::map!(
            prefix.as_bytes(),
            CapabilityUrn,
            "Mailbox" => "urn:ietf:params:jmap:mail",
            "Email" => "urn:ietf:params:jmap:mail",
            "Identity" => "urn:ietf:params:jmap:submission",
            "SieveScript" => "urn:ietf:params:jmap:sieve",
            "AddressBook" => "urn:ietf:params:jmap:contacts",
            "ContactCard" => "urn:ietf:params:jmap:contacts",
            "Calendar" => "urn:ietf:params:jmap:calendars",
            "CalendarEvent" => "urn:ietf:params:jmap:calendars",
            "ParticipantIdentity" => "urn:ietf:params:jmap:calendars",
            "FileNode" => "urn:ietf:params:jmap:filenode",
            "Principal" => "urn:ietf:params:jmap:principals",
            "VacationResponse" => URN_VACATION_RESPONSE,
            "x:Account" => "urn:stalwart:jmap",
            "x:Domain" => "urn:stalwart:jmap",
        )
        .copied()
        .unwrap_or(URN_CORE)
    }

    fn encode(&self, limits: Option<&Limits>) -> Result<Vec<u8>, JmapError> {
        if let Some(limits) = limits
            && u64::try_from(self.calls.len()).unwrap_or(u64::MAX) > limits.max_calls_in_request
        {
            return Err(JmapError::RequestTooLarge);
        }
        let payload = serde_json::to_vec(self)?;
        if let Some(limits) = limits
            && u64::try_from(payload.len()).unwrap_or(u64::MAX) > limits.max_size_request
        {
            return Err(JmapError::RequestTooLarge);
        }
        Ok(payload)
    }

    pub fn fits(&self, limits: &Limits) -> Result<(), JmapError> {
        self.encode(Some(limits)).map(drop)
    }

    pub fn send(&self, client: &JmapClient, api_url: &str) -> Result<Response, JmapError> {
        let payload = self.encode(None)?;
        Response::parse(client.post_payload(api_url, &payload)?)
    }

    fn send_within(&self, account: &JmapAccount, limits: &Limits) -> Result<Response, JmapError> {
        let payload = self.encode(Some(limits))?;
        Response::parse(account.client().post_payload(account.api_url(), &payload)?)
    }
}

#[derive(Debug)]
pub struct Response {
    pub method_responses: Vec<MethodCall>,
}

impl Response {
    fn parse(mut value: Value) -> Result<Response, JmapError> {
        let Some(Value::Array(entries)) = value.get_mut("methodResponses").map(Value::take) else {
            return Err(JmapError::malformed(
                "response has no methodResponses array",
            ));
        };
        let mut method_responses = Vec::with_capacity(entries.len());
        for entry in entries {
            let Ok([name, args, call_id]) = <[Value; 3]>::try_from(match entry {
                Value::Array(triple) => triple,
                _ => Vec::new(),
            }) else {
                return Err(JmapError::malformed(
                    "methodResponse is not a [name,args,id] triple",
                ));
            };
            let Value::String(name) = name else {
                return Err(JmapError::malformed("methodResponse name is not a string"));
            };
            let Value::String(call_id) = call_id else {
                return Err(JmapError::malformed(
                    "methodResponse callId is not a string",
                ));
            };
            method_responses.push(MethodCall {
                name,
                args,
                call_id,
            });
        }
        Ok(Response { method_responses })
    }

    pub fn first(&self) -> Result<&MethodCall, JmapError> {
        self.method_responses
            .first()
            .ok_or_else(|| JmapError::malformed("empty methodResponses"))
    }

    pub fn by_call_id(&self, call_id: &str) -> Result<&MethodCall, JmapError> {
        self.method_responses
            .iter()
            .find(|m| m.call_id == call_id)
            .ok_or_else(|| JmapError::malformed(format!("no response for callId {call_id}")))
    }
}

impl JmapClient {
    pub fn retry_method<T>(
        &self,
        kind: MethodCallKind,
        mut attempt: impl FnMut() -> Result<T, JmapError>,
    ) -> Result<T, JmapError> {
        let policy = *self.retry();
        let mut tries = 0u32;
        loop {
            let err = match attempt() {
                Ok(value) => return Ok(value),
                Err(e) => e,
            };
            let retryable = matches!(
                &err,
                JmapError::Method { error_type, .. }
                    if kind.disposition(error_type) == Disposition::Retry
            );
            if tries >= policy.max_retries || !retryable {
                return Err(err);
            }
            tries += 1;
            let delay = self.backoff(tries);
            self.rate_limit().cooldown().arm(delay);
            self.logger().warn(format_args!(
                "{err}; retrying in {} ({tries}/{})",
                RetryWait(delay),
                policy.max_retries
            ));
            thread::sleep(delay);
        }
    }
}

#[derive(Debug)]
pub struct GetResult<T> {
    pub list: Vec<T>,
    pub not_found: Vec<JmapId>,
    pub state: Option<String>,
}

impl<T> Default for GetResult<T> {
    fn default() -> Self {
        GetResult {
            list: Vec::new(),
            not_found: Vec::new(),
            state: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct ChangesResult {
    pub created: Vec<JmapId>,
    pub updated: Vec<JmapId>,
    pub destroyed: Vec<JmapId>,
    pub new_state: String,
}

impl ChangesResult {
    fn dedup(&mut self) {
        for ids in [&mut self.created, &mut self.updated, &mut self.destroyed] {
            let mut seen = IndexSet::with_capacity(ids.len());
            ids.retain(|id| seen.insert(id.0.clone()));
        }
    }
}

#[derive(Debug, Default)]
pub struct SetOutcome {
    pub created: Vec<(String, Value)>,
    pub updated: Vec<String>,
    pub destroyed: Vec<String>,
    pub not_created: Vec<(String, Value)>,
    pub not_updated: Vec<(String, Value)>,
    pub not_destroyed: Vec<(String, Value)>,
}

impl SetOutcome {
    fn absorb(&mut self, mut other: SetOutcome) {
        self.created.append(&mut other.created);
        self.updated.append(&mut other.updated);
        self.destroyed.append(&mut other.destroyed);
        self.not_created.append(&mut other.not_created);
        self.not_updated.append(&mut other.not_updated);
        self.not_destroyed.append(&mut other.not_destroyed);
    }
}

#[derive(Debug, Default)]
pub struct SetRequest<'a> {
    pub create: Option<Value>,
    pub update: Option<Value>,
    pub destroy: Option<Value>,
    pub extra_args: &'a [(&'a str, Value)],
}

#[derive(Debug, Clone, Copy)]
enum Section {
    Create,
    Update,
    Destroy,
}

struct SetItem {
    section: Section,
    key: String,
    value: Value,
}

impl SetItem {
    fn collect(value: Option<Value>, section: Section, out: &mut Vec<SetItem>) {
        match (section, value) {
            (Section::Destroy, Some(Value::Array(ids))) => {
                out.extend(ids.into_iter().filter_map(|v| match v {
                    Value::String(key) => Some(SetItem {
                        section,
                        key,
                        value: Value::Null,
                    }),
                    _ => None,
                }));
            }
            (_, Some(Value::Object(m))) => out.extend(m.into_iter().map(|(key, value)| SetItem {
                section,
                key,
                value,
            })),
            _ => {}
        }
    }
}

impl JmapAccount {
    pub fn send(&self, req: &Request) -> Result<Response, JmapError> {
        req.send(self.client(), self.api_url())
    }

    fn single_call(&self, req: &Request) -> Result<Response, JmapError> {
        let resp = self.send(req)?;
        resp.first()?.check_error()?;
        Ok(resp)
    }

    fn base_args(&self) -> Map<String, Value> {
        let mut args = Map::new();
        args.insert(
            "accountId".to_owned(),
            Value::String(self.account_id().to_owned()),
        );
        args
    }

    pub fn query_all_ids(&self, type_name: &str) -> Result<Vec<JmapId>, JmapError> {
        let mut restarts = 0u32;
        loop {
            match self.query_pages(type_name) {
                Err(JmapError::AnchorNotFound) if restarts < ANCHOR_RESTARTS => restarts += 1,
                other => return other,
            }
        }
    }

    fn query_pages(&self, type_name: &str) -> Result<Vec<JmapId>, JmapError> {
        let limit = self.limits().max_objects_in_get.max(1);
        let method = format!("{type_name}/query");
        let mut collected: Vec<JmapId> = Vec::new();
        let mut page_size: Option<usize> = None;
        let mut anchor: Option<String> = None;

        loop {
            let mut args = self.base_args();
            args.insert("limit".to_owned(), Value::from(limit));
            if let Some(a) = anchor.take() {
                args.insert("anchor".to_owned(), Value::String(a));
                args.insert("anchorOffset".to_owned(), Value::from(1));
            }
            let mut req = Request::new();
            req.call(method.as_str(), Value::Object(args), "q");
            let resp = self.single_call(&req)?;
            let page = resp.first()?;
            let ids = page
                .args
                .get("ids")
                .and_then(Value::as_array)
                .ok_or_else(|| JmapError::malformed("query response has no ids array"))?;
            let this_len = ids.len();
            collected.reserve(this_len);
            for v in ids {
                let s = v
                    .as_str()
                    .ok_or_else(|| JmapError::malformed("query id is not a string"))?;
                collected.push(JmapId(s.to_owned()));
            }
            match page_size {
                None if this_len == 0 => break,
                None => page_size = Some(this_len),
                Some(full) if this_len == 0 || this_len < full => break,
                Some(_) => {}
            }
            let last = collected
                .last()
                .map(|i| i.0.clone())
                .ok_or_else(|| JmapError::malformed("non-empty page yielded no anchor"))?;
            anchor = Some(last);
        }
        Ok(collected)
    }

    pub fn get<T: DeserializeOwned>(
        &self,
        type_name: &str,
        ids: &[JmapId],
        properties: Option<&[&str]>,
    ) -> Result<GetResult<T>, JmapError> {
        let mut out = GetResult::default();
        let chunk = usize::try_from(self.limits().max_objects_in_get.max(1)).unwrap_or(usize::MAX);
        for part in ids.chunks(chunk) {
            self.get_chunk(type_name, part, properties, &mut out)?;
        }
        Ok(out)
    }

    fn get_chunk<T: DeserializeOwned>(
        &self,
        type_name: &str,
        ids: &[JmapId],
        properties: Option<&[&str]>,
        out: &mut GetResult<T>,
    ) -> Result<(), JmapError> {
        let mut args = self.base_args();
        args.insert(
            "ids".to_owned(),
            Value::Array(ids.iter().map(|i| Value::String(i.0.clone())).collect()),
        );
        if let Some(props) = properties {
            args.insert(
                "properties".to_owned(),
                Value::Array(
                    props
                        .iter()
                        .chain(&["id"])
                        .map(|s| Value::String((*s).to_owned()))
                        .collect(),
                ),
            );
        }
        let mut req = Request::new();
        req.call(format!("{type_name}/get"), Value::Object(args), "g");
        let sent = req.send_within(self, self.limits()).and_then(|resp| {
            let mr = resp.first()?;
            mr.check_error()?;
            mr.decode_get(out)
        });
        match sent {
            Err(JmapError::RequestTooLarge) if ids.len() <= 1 => {
                Err(JmapError::SingleObjectTooLarge(format!(
                    "{type_name}/get of a single id exceeds maxSizeRequest"
                )))
            }
            Err(JmapError::RequestTooLarge) => {
                let (head, tail) = ids.split_at(ids.len() / 2);
                self.get_chunk(type_name, head, properties, out)?;
                self.get_chunk(type_name, tail, properties, out)
            }
            other => other,
        }
    }

    pub fn get_all<T: DeserializeOwned>(&self, type_name: &str) -> Result<GetResult<T>, JmapError> {
        let mut args = self.base_args();
        args.insert("ids".to_owned(), Value::Null);
        let mut req = Request::new();
        req.call(format!("{type_name}/get"), Value::Object(args), "g");
        let resp = self.single_call(&req)?;
        let mut out = GetResult::default();
        resp.first()?.decode_get(&mut out)?;
        Ok(out)
    }

    pub fn state(&self, type_name: &str) -> Result<Option<String>, JmapError> {
        let mut args = self.base_args();
        args.insert("ids".to_owned(), Value::Array(Vec::new()));
        let mut req = Request::new();
        req.call(format!("{type_name}/get"), Value::Object(args), "g");
        let resp = self.single_call(&req)?;
        let mut out: GetResult<Value> = GetResult::default();
        resp.first()?.decode_get(&mut out)?;
        Ok(out.state)
    }

    pub fn changes(&self, type_name: &str, since_state: &str) -> Result<ChangesResult, JmapError> {
        let method = format!("{type_name}/changes");
        let max_changes = self.limits().max_objects_in_get.max(1);
        let mut out = ChangesResult::default();
        let mut since = since_state.to_owned();
        loop {
            let mut args = self.base_args();
            args.insert("sinceState".to_owned(), Value::String(since.clone()));
            args.insert("maxChanges".to_owned(), Value::from(max_changes));
            let mut req = Request::new();
            req.call(method.as_str(), Value::Object(args), "c");
            let resp = self.single_call(&req)?;
            let mr = resp.first()?;
            let new_state = mr
                .args
                .get("newState")
                .and_then(Value::as_str)
                .ok_or_else(|| JmapError::malformed("changes response has no newState"))?;
            mr.append_ids("created", &mut out.created);
            mr.append_ids("updated", &mut out.updated);
            mr.append_ids("destroyed", &mut out.destroyed);
            let has_more = mr
                .args
                .get("hasMoreChanges")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            out.new_state.clear();
            out.new_state.push_str(new_state);
            if !has_more || since == new_state {
                break;
            }
            since.clear();
            since.push_str(new_state);
        }
        out.dedup();
        Ok(out)
    }

    pub fn set(&self, type_name: &str, body: SetRequest<'_>) -> Result<SetOutcome, JmapError> {
        let mut items = Vec::new();
        SetItem::collect(body.create, Section::Create, &mut items);
        SetItem::collect(body.update, Section::Update, &mut items);
        SetItem::collect(body.destroy, Section::Destroy, &mut items);

        let method = format!("{type_name}/set");
        if items.is_empty() {
            return self.set_send(type_name, &method, &[], body.extra_args);
        }

        let chunk = usize::try_from(self.limits().max_objects_in_set.max(1)).unwrap_or(usize::MAX);
        let mut outcome = SetOutcome::default();
        for part in items.chunks(chunk) {
            outcome.absorb(self.set_send(type_name, &method, part, body.extra_args)?);
        }
        Ok(outcome)
    }

    fn set_send(
        &self,
        type_name: &str,
        method: &str,
        items: &[SetItem],
        extra_args: &[(&str, Value)],
    ) -> Result<SetOutcome, JmapError> {
        let mut args = self.base_args();
        let mut create = Map::new();
        let mut update = Map::new();
        let mut destroy: Vec<Value> = Vec::new();
        for item in items {
            match item.section {
                Section::Create => {
                    create.insert(item.key.clone(), item.value.clone());
                }
                Section::Update => {
                    update.insert(item.key.clone(), item.value.clone());
                }
                Section::Destroy => destroy.push(Value::String(item.key.clone())),
            }
        }
        if !create.is_empty() {
            args.insert("create".to_owned(), Value::Object(create));
        }
        if !update.is_empty() {
            args.insert("update".to_owned(), Value::Object(update));
        }
        if !destroy.is_empty() {
            args.insert("destroy".to_owned(), Value::Array(destroy));
        }
        for (k, v) in extra_args {
            args.insert((*k).to_owned(), v.clone());
        }
        let mut req = Request::new();
        req.call(method, Value::Object(args), "s");

        let sent = req.send_within(self, self.limits()).and_then(|resp| {
            let mr = resp.first()?;
            mr.check_error()?;
            Ok(mr.decode_set())
        });
        match sent {
            Err(JmapError::RequestTooLarge) if items.len() <= 1 => {
                Err(JmapError::SingleObjectTooLarge(format!(
                    "{type_name}/set of a single object exceeds maxSizeRequest"
                )))
            }
            Err(JmapError::RequestTooLarge) => {
                let (head, tail) = items.split_at(items.len() / 2);
                let mut left = self.set_send(type_name, method, head, extra_args)?;
                left.absorb(self.set_send(type_name, method, tail, extra_args)?);
                Ok(left)
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::logging::Level;
    use serde_json::json;
    use std::cell::Cell;

    use super::*;
    use crate::error::Error;
    use crate::logging::Logger;
    use crate::net::auth::Auth;
    use crate::net::http::{ClientOptions, TlsPolicy};
    use crate::net::retry::RetryPolicy;

    fn test_client(max_retries: u32) -> JmapClient {
        JmapClient::new(
            Auth::Bearer {
                token: "t".to_owned(),
            },
            ClientOptions {
                retry: RetryPolicy::new(max_retries),
                tls: TlsPolicy::Verify,
                logger: Logger::new(Level::Quiet),
            },
        )
    }

    fn method_error(error_type: &str) -> JmapError {
        JmapError::Method {
            call_id: "i".to_owned(),
            error_type: error_type.to_owned(),
            description: None,
        }
    }

    fn drive(
        max_retries: u32,
        outcome: impl Fn(u32) -> Result<&'static str, JmapError>,
    ) -> (Result<&'static str, JmapError>, u32) {
        let client = test_client(max_retries);
        let calls = Cell::new(0u32);
        let result = client.retry_method(MethodCallKind::SingleObjectWrite, || {
            calls.set(calls.get() + 1);
            outcome(calls.get())
        });
        (result, calls.get())
    }

    #[test]
    fn transient_method_error_is_retried_until_it_succeeds() {
        let (result, calls) = drive(3, |n| {
            if n < 3 {
                Err(method_error("serverUnavailable"))
            } else {
                Ok("created")
            }
        });
        assert_eq!(result.unwrap(), "created");
        assert_eq!(calls, 3);
    }

    #[test]
    fn exhausting_the_budget_returns_the_original_method_error() {
        let (result, calls) = drive(2, |_| Err(method_error("serverUnavailable")));
        assert_eq!(calls, 3, "one initial attempt plus max_retries");
        let err = result.unwrap_err();
        assert!(
            matches!(&err, JmapError::Method { error_type, .. } if error_type == "serverUnavailable"),
            "the caller must still see the method error, not RetriesExhausted: {err}"
        );
        assert!(
            !Error::from(err).aborts_run(),
            "an exhausted method retry stays a per-unit failure"
        );
    }

    #[test]
    fn permanent_method_error_consumes_no_attempt() {
        let (result, calls) = drive(5, |_| Err(method_error("invalidArguments")));
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }

    #[test]
    fn partial_fail_is_not_retried_for_a_write() {
        let (result, calls) = drive(5, |_| Err(method_error("serverPartialFail")));
        assert!(result.is_err());
        assert_eq!(calls, 1, "retrying a partial write could duplicate it");
    }

    #[test]
    fn non_method_errors_are_left_to_the_http_layer() {
        let (result, calls) = drive(5, |_| Err(JmapError::Transport("reset".to_owned())));
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }

    #[test]
    fn using_union_includes_core_and_dedups() {
        let mut r = Request::new();
        r.call("Mailbox/get", json!({}), "a");
        r.call("Email/query", json!({}), "b");
        let u = r.using();
        assert!(u.contains(URN_CORE));
        assert!(u.contains("urn:ietf:params:jmap:mail"));
        assert_eq!(u.iter().filter(|x| **x == URN_CORE).count(), 1);
    }

    #[test]
    fn using_maps_principal_and_stalwart() {
        assert_eq!(
            Request::capability_of("Principal/query"),
            "urn:ietf:params:jmap:principals"
        );
        assert_eq!(Request::capability_of("x:Account/set"), "urn:stalwart:jmap");
        assert_eq!(
            Request::capability_of("CalendarEvent/set"),
            "urn:ietf:params:jmap:calendars"
        );
        assert_eq!(
            Request::capability_of("Identity/get"),
            "urn:ietf:params:jmap:submission"
        );
    }

    #[test]
    fn fits_rejects_too_many_calls() {
        let limits = Limits {
            max_objects_in_get: 10,
            max_objects_in_set: 10,
            max_calls_in_request: 2,
            max_concurrent_requests: 4,
            max_concurrent_upload: 4,
            max_size_request: 10_000_000,
            max_size_upload: 10_000_000,
        };
        let mut r = Request::new();
        r.call("Mailbox/get", json!({}), "a");
        r.call("Mailbox/get", json!({}), "b");
        r.call("Mailbox/get", json!({}), "c");
        assert!(matches!(r.fits(&limits), Err(JmapError::RequestTooLarge)));
    }

    #[test]
    fn parse_response_and_method_error() {
        let v = json!({
            "methodResponses": [
                ["Mailbox/get", {"list": [], "notFound": []}, "g"],
                ["error", {"type": "invalidArguments"}, "x"]
            ]
        });
        let r = Response::parse(v).unwrap();
        assert!(r.by_call_id("g").unwrap().check_error().is_ok());
        let err = r.by_call_id("x").unwrap().check_error().unwrap_err();
        assert!(matches!(err, JmapError::Method { .. }));
    }

    #[test]
    fn anchor_not_found_is_classified() {
        let v = json!({ "methodResponses": [["error", {"type": "anchorNotFound"}, "q"]] });
        let r = Response::parse(v).unwrap();
        let err = r.first().unwrap().check_error().unwrap_err();
        assert!(matches!(err, JmapError::AnchorNotFound));
    }

    #[test]
    fn method_dispositions_depend_on_the_call_kind() {
        assert_eq!(
            MethodCallKind::Read.disposition("serverPartialFail"),
            Disposition::Retry
        );
        assert_eq!(
            MethodCallKind::SingleObjectWrite.disposition("serverPartialFail"),
            Disposition::Fatal
        );
        assert_eq!(
            MethodCallKind::SingleObjectWrite.disposition("serverUnavailable"),
            Disposition::Retry
        );
        assert_eq!(
            MethodCallKind::Read.disposition("forbidden"),
            Disposition::Fatal
        );
    }
}
