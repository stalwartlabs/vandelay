/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use indexmap::IndexSet;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::jmap::error::JmapError;
use crate::jmap::http::HttpClient;
use crate::jmap::session::Limits;
use crate::jmap::wire::JmapId;

pub const URN_CORE: &str = "urn:ietf:params:jmap:core";
/// RFC 9404 blob management. Lets many blobs be created in one method call
/// instead of one HTTP upload each.
pub const URN_BLOB: &str = "urn:ietf:params:jmap:blob";

pub fn using_urn(method: &str) -> &'static str {
    let prefix = method.split('/').next().unwrap_or(method);
    match prefix {
        "Mailbox" | "Email" => "urn:ietf:params:jmap:mail",
        "Identity" => "urn:ietf:params:jmap:submission",
        "SieveScript" => "urn:ietf:params:jmap:sieve",
        "AddressBook" | "ContactCard" => "urn:ietf:params:jmap:contacts",
        "Calendar" | "CalendarEvent" | "ParticipantIdentity" => "urn:ietf:params:jmap:calendars",
        "FileNode" => "urn:ietf:params:jmap:filenode",
        "Principal" => "urn:ietf:params:jmap:principals",
        "Blob" => URN_BLOB,
        "x:Account" | "x:Domain" => "urn:stalwart:jmap",
        _ => URN_CORE,
    }
}

#[derive(Debug, Clone)]
pub struct MethodCall {
    pub name: String,
    pub args: Value,
    pub call_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct Request {
    calls: Vec<MethodCall>,
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

    pub fn using(&self) -> Vec<String> {
        let mut set: IndexSet<String> = IndexSet::new();
        set.insert(URN_CORE.to_owned());
        for c in &self.calls {
            set.insert(using_urn(&c.name).to_owned());
        }
        set.into_iter().collect()
    }

    fn envelope(&self) -> Value {
        let method_calls: Vec<Value> = self
            .calls
            .iter()
            .map(|c| json!([c.name, c.args, c.call_id]))
            .collect();
        json!({ "using": self.using(), "methodCalls": method_calls })
    }

    pub fn fits(&self, limits: &Limits) -> Result<(), JmapError> {
        if self.calls.len() as u64 > limits.max_calls_in_request {
            return Err(JmapError::RequestTooLarge);
        }
        let size = serde_json::to_vec(&self.envelope())?.len() as u64;
        if size > limits.max_size_request {
            return Err(JmapError::RequestTooLarge);
        }
        Ok(())
    }

    pub fn send(&self, client: &HttpClient, api_url: &str) -> Result<Response, JmapError> {
        let value = client.post_json(api_url, &self.envelope())?;
        Response::parse(value)
    }
}

#[derive(Debug)]
pub struct Response {
    pub method_responses: Vec<MethodCall>,
}

impl Response {
    fn parse(value: Value) -> Result<Response, JmapError> {
        let arr = value
            .get("methodResponses")
            .and_then(Value::as_array)
            .ok_or_else(|| JmapError::malformed("response has no methodResponses array"))?;
        let mut out = Vec::with_capacity(arr.len());
        for entry in arr {
            let triple = entry.as_array().filter(|t| t.len() == 3).ok_or_else(|| {
                JmapError::malformed("methodResponse is not a [name,args,id] triple")
            })?;
            let name = triple[0]
                .as_str()
                .ok_or_else(|| JmapError::malformed("methodResponse name is not a string"))?
                .to_owned();
            let call_id = triple[2]
                .as_str()
                .ok_or_else(|| JmapError::malformed("methodResponse callId is not a string"))?
                .to_owned();
            out.push(MethodCall {
                name,
                args: triple[1].clone(),
                call_id,
            });
        }
        Ok(Response {
            method_responses: out,
        })
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

pub fn check_method_error(mr: &MethodCall) -> Result<(), JmapError> {
    if mr.name != "error" {
        return Ok(());
    }
    let error_type = mr
        .args
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    if error_type == "anchorNotFound" {
        return Err(JmapError::AnchorNotFound);
    }
    if error_type == "cannotCalculateChanges" {
        return Err(JmapError::CannotCalculateChanges);
    }
    if error_type == "unknownMethod" {
        return Err(JmapError::UnknownMethod);
    }
    let description = mr
        .args
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Err(JmapError::Method {
        call_id: mr.call_id.clone(),
        error_type,
        description,
    })
}

fn ids_array(ids: &[JmapId]) -> Value {
    Value::Array(ids.iter().map(|i| Value::String(i.0.clone())).collect())
}

pub fn query_all_ids(
    client: &HttpClient,
    api_url: &str,
    account_id: &str,
    type_name: &str,
    limits: &Limits,
) -> Result<Vec<JmapId>, JmapError> {
    let mut restarts = 0u32;
    loop {
        match query_pages(client, api_url, account_id, type_name, limits) {
            Ok(ids) => return Ok(ids),
            Err(JmapError::AnchorNotFound) if restarts < 2 => {
                restarts += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

fn query_pages(
    client: &HttpClient,
    api_url: &str,
    account_id: &str,
    type_name: &str,
    limits: &Limits,
) -> Result<Vec<JmapId>, JmapError> {
    let limit = limits.max_objects_in_get.max(1);
    let mut collected: Vec<JmapId> = Vec::new();
    let mut page_size: Option<usize> = None;
    let mut anchor: Option<String> = None;

    loop {
        let mut args = Map::new();
        args.insert("accountId".to_owned(), Value::String(account_id.to_owned()));
        args.insert("limit".to_owned(), Value::from(limit));
        if let Some(a) = &anchor {
            args.insert("anchor".to_owned(), Value::String(a.clone()));
            args.insert("anchorOffset".to_owned(), Value::from(1));
        }
        let mut req = Request::new();
        req.call(format!("{type_name}/query"), Value::Object(args), "q");
        let resp = req.send(client, api_url)?;
        let mr = resp.first()?;
        check_method_error(mr)?;
        let ids = mr
            .args
            .get("ids")
            .and_then(Value::as_array)
            .ok_or_else(|| JmapError::malformed("query response has no ids array"))?;
        let this_len = ids.len();
        for v in ids {
            let s = v
                .as_str()
                .ok_or_else(|| JmapError::malformed("query id is not a string"))?;
            collected.push(JmapId(s.to_owned()));
        }
        match page_size {
            None => {
                if this_len == 0 {
                    break;
                }
                page_size = Some(this_len);
            }
            Some(full) => {
                if this_len == 0 || this_len < full {
                    break;
                }
            }
        }
        let last = collected
            .last()
            .map(|i| i.0.clone())
            .ok_or_else(|| JmapError::malformed("non-empty page yielded no anchor"))?;
        anchor = Some(last);
    }
    Ok(collected)
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

struct GetCtx<'a> {
    client: &'a HttpClient,
    api_url: &'a str,
    account_id: &'a str,
    type_name: &'a str,
    properties: Option<&'a [&'a str]>,
    limits: &'a Limits,
}

pub fn get_objects<T: DeserializeOwned>(
    client: &HttpClient,
    api_url: &str,
    account_id: &str,
    type_name: &str,
    ids: &[JmapId],
    properties: Option<&[&str]>,
    limits: &Limits,
) -> Result<GetResult<T>, JmapError> {
    let ctx = GetCtx {
        client,
        api_url,
        account_id,
        type_name,
        properties,
        limits,
    };
    let mut out = GetResult::default();
    let chunk = limits.max_objects_in_get.max(1) as usize;
    let mut start = 0;
    while start < ids.len() {
        let end = (start + chunk).min(ids.len());
        get_chunk(&ctx, &ids[start..end], &mut out)?;
        start = end;
    }
    Ok(out)
}

fn get_chunk<T: DeserializeOwned>(
    ctx: &GetCtx<'_>,
    ids: &[JmapId],
    out: &mut GetResult<T>,
) -> Result<(), JmapError> {
    let mut args = Map::new();
    args.insert(
        "accountId".to_owned(),
        Value::String(ctx.account_id.to_owned()),
    );
    args.insert("ids".to_owned(), ids_array(ids));
    if let Some(props) = ctx.properties {
        let mut p: Vec<Value> = props
            .iter()
            .map(|s| Value::String((*s).to_owned()))
            .collect();
        p.push(Value::String("id".to_owned()));
        args.insert("properties".to_owned(), Value::Array(p));
    }
    let mut req = Request::new();
    req.call(format!("{}/get", ctx.type_name), Value::Object(args), "g");
    let send_result: Result<(), JmapError> = if req.fits(ctx.limits).is_err() {
        Err(JmapError::RequestTooLarge)
    } else {
        match req.send(ctx.client, ctx.api_url) {
            Ok(resp) => {
                let mr = resp.first()?;
                check_method_error(mr)?;
                decode_get(mr, out)
            }
            Err(e) => Err(e),
        }
    };
    match send_result {
        Ok(()) => Ok(()),
        Err(JmapError::RequestTooLarge) => {
            if ids.len() <= 1 {
                return Err(JmapError::SingleObjectTooLarge(format!(
                    "{}/get of a single id exceeds maxSizeRequest",
                    ctx.type_name
                )));
            }
            let mid = ids.len() / 2;
            get_chunk(ctx, &ids[..mid], out)?;
            get_chunk(ctx, &ids[mid..], out)
        }
        Err(e) => Err(e),
    }
}

pub fn get_all<T: DeserializeOwned>(
    client: &HttpClient,
    api_url: &str,
    account_id: &str,
    type_name: &str,
) -> Result<GetResult<T>, JmapError> {
    let mut args = Map::new();
    args.insert("accountId".to_owned(), Value::String(account_id.to_owned()));
    args.insert("ids".to_owned(), Value::Null);
    let mut req = Request::new();
    req.call(format!("{type_name}/get"), Value::Object(args), "g");
    let resp = req.send(client, api_url)?;
    let mr = resp.first()?;
    check_method_error(mr)?;
    let mut out = GetResult::default();
    decode_get(mr, &mut out)?;
    Ok(out)
}

pub fn get_state(
    client: &HttpClient,
    api_url: &str,
    account_id: &str,
    type_name: &str,
) -> Result<Option<String>, JmapError> {
    let mut args = Map::new();
    args.insert("accountId".to_owned(), Value::String(account_id.to_owned()));
    args.insert("ids".to_owned(), Value::Array(Vec::new()));
    let mut req = Request::new();
    req.call(format!("{type_name}/get"), Value::Object(args), "g");
    let resp = req.send(client, api_url)?;
    let mr = resp.first()?;
    check_method_error(mr)?;
    let mut out: GetResult<Value> = GetResult::default();
    decode_get(mr, &mut out)?;
    Ok(out.state)
}

#[derive(Debug, Default)]
pub struct ChangesResult {
    pub created: Vec<JmapId>,
    pub updated: Vec<JmapId>,
    pub destroyed: Vec<JmapId>,
    pub new_state: String,
}

pub fn get_changes(
    client: &HttpClient,
    api_url: &str,
    account_id: &str,
    type_name: &str,
    since_state: &str,
    limits: &Limits,
) -> Result<ChangesResult, JmapError> {
    let mut out = ChangesResult::default();
    let mut since = since_state.to_owned();
    loop {
        let mut args = Map::new();
        args.insert("accountId".to_owned(), Value::String(account_id.to_owned()));
        args.insert("sinceState".to_owned(), Value::String(since.clone()));
        args.insert(
            "maxChanges".to_owned(),
            Value::from(limits.max_objects_in_get.max(1)),
        );
        let mut req = Request::new();
        req.call(format!("{type_name}/changes"), Value::Object(args), "c");
        let resp = req.send(client, api_url)?;
        let mr = resp.first()?;
        check_method_error(mr)?;
        let new_state = mr
            .args
            .get("newState")
            .and_then(Value::as_str)
            .ok_or_else(|| JmapError::malformed("changes response has no newState"))?
            .to_owned();
        append_id_array(&mr.args, "created", &mut out.created);
        append_id_array(&mr.args, "updated", &mut out.updated);
        append_id_array(&mr.args, "destroyed", &mut out.destroyed);
        let has_more = mr
            .args
            .get("hasMoreChanges")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        out.new_state = new_state.clone();
        if !has_more || since == new_state {
            break;
        }
        since = new_state;
    }
    dedup_ids(&mut out.created);
    dedup_ids(&mut out.updated);
    dedup_ids(&mut out.destroyed);
    Ok(out)
}

fn append_id_array(args: &Value, key: &str, out: &mut Vec<JmapId>) {
    if let Some(arr) = args.get(key).and_then(Value::as_array) {
        for v in arr {
            if let Some(s) = v.as_str() {
                out.push(JmapId(s.to_owned()));
            }
        }
    }
}

fn dedup_ids(ids: &mut Vec<JmapId>) {
    let mut seen = IndexSet::new();
    ids.retain(|id| seen.insert(id.0.clone()));
}

fn decode_get<T: DeserializeOwned>(
    mr: &MethodCall,
    out: &mut GetResult<T>,
) -> Result<(), JmapError> {
    let list = mr
        .args
        .get("list")
        .and_then(Value::as_array)
        .ok_or_else(|| JmapError::malformed("get response has no list array"))?;
    for item in list {
        out.list.push(serde_json::from_value(item.clone())?);
    }
    if let Some(nf) = mr.args.get("notFound").and_then(Value::as_array) {
        for v in nf {
            if let Some(s) = v.as_str() {
                out.not_found.push(JmapId(s.to_owned()));
            }
        }
    }
    if let Some(s) = mr.args.get("state").and_then(Value::as_str) {
        out.state = Some(s.to_owned());
    }
    Ok(())
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

fn collect_items(value: Option<&Value>, section: Section) -> Vec<(Section, String, Value)> {
    match (section, value) {
        (Section::Destroy, Some(Value::Array(ids))) => ids
            .iter()
            .filter_map(|v| v.as_str().map(|s| (section, s.to_owned(), Value::Null)))
            .collect(),
        (_, Some(Value::Object(m))) => m
            .iter()
            .map(|(k, v)| (section, k.clone(), v.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

pub fn set_call(
    client: &HttpClient,
    api_url: &str,
    account_id: &str,
    type_name: &str,
    body: SetRequest<'_>,
    limits: &Limits,
) -> Result<SetOutcome, JmapError> {
    let mut items = collect_items(body.create.as_ref(), Section::Create);
    items.extend(collect_items(body.update.as_ref(), Section::Update));
    items.extend(collect_items(body.destroy.as_ref(), Section::Destroy));

    if items.is_empty() {
        return set_send(
            client,
            api_url,
            account_id,
            type_name,
            &[],
            body.extra_args,
            limits,
        );
    }

    let chunk = limits.max_objects_in_set.max(1) as usize;
    let mut outcome = SetOutcome::default();
    let mut start = 0;
    while start < items.len() {
        let end = (start + chunk).min(items.len());
        let part = set_send(
            client,
            api_url,
            account_id,
            type_name,
            &items[start..end],
            body.extra_args,
            limits,
        )?;
        outcome.absorb(part);
        start = end;
    }
    Ok(outcome)
}

fn set_send(
    client: &HttpClient,
    api_url: &str,
    account_id: &str,
    type_name: &str,
    items: &[(Section, String, Value)],
    extra_args: &[(&str, Value)],
    limits: &Limits,
) -> Result<SetOutcome, JmapError> {
    let mut args = Map::new();
    args.insert("accountId".to_owned(), Value::String(account_id.to_owned()));
    let mut create = Map::new();
    let mut update = Map::new();
    let mut destroy: Vec<Value> = Vec::new();
    for (section, key, val) in items {
        match section {
            Section::Create => {
                create.insert(key.clone(), val.clone());
            }
            Section::Update => {
                update.insert(key.clone(), val.clone());
            }
            Section::Destroy => destroy.push(Value::String(key.clone())),
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
    req.call(format!("{type_name}/set"), Value::Object(args), "s");

    let oversize = req.fits(limits).is_err();
    let send_result = if oversize {
        Err(JmapError::RequestTooLarge)
    } else {
        req.send(client, api_url).and_then(|resp| {
            let mr = resp.first()?;
            check_method_error(mr)?;
            Ok(decode_set(mr))
        })
    };

    match send_result {
        Ok(outcome) => Ok(outcome),
        Err(JmapError::RequestTooLarge) => {
            if items.len() <= 1 {
                return Err(JmapError::SingleObjectTooLarge(format!(
                    "{type_name}/set of a single object exceeds maxSizeRequest"
                )));
            }
            let mid = items.len() / 2;
            let mut left = set_send(
                client,
                api_url,
                account_id,
                type_name,
                &items[..mid],
                extra_args,
                limits,
            )?;
            let right = set_send(
                client,
                api_url,
                account_id,
                type_name,
                &items[mid..],
                extra_args,
                limits,
            )?;
            left.absorb(right);
            Ok(left)
        }
        Err(e) => Err(e),
    }
}

fn pairs(args: &Value, key: &str) -> Vec<(String, Value)> {
    args.get(key)
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

fn keys(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn decode_set(mr: &MethodCall) -> SetOutcome {
    SetOutcome {
        created: pairs(&mr.args, "created"),
        updated: mr
            .args
            .get("updated")
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default(),
        destroyed: keys(&mr.args, "destroyed"),
        not_created: pairs(&mr.args, "notCreated"),
        not_updated: pairs(&mr.args, "notUpdated"),
        not_destroyed: pairs(&mr.args, "notDestroyed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn using_union_includes_core_and_dedups() {
        let mut r = Request::new();
        r.call("Mailbox/get", json!({}), "a");
        r.call("Email/query", json!({}), "b");
        let u = r.using();
        assert!(u.contains(&URN_CORE.to_owned()));
        assert!(u.contains(&"urn:ietf:params:jmap:mail".to_owned()));
        assert_eq!(u.iter().filter(|x| x.as_str() == URN_CORE).count(), 1);
    }

    #[test]
    fn using_maps_principal_and_stalwart() {
        assert_eq!(
            using_urn("Principal/query"),
            "urn:ietf:params:jmap:principals"
        );
        assert_eq!(using_urn("x:Account/set"), "urn:stalwart:jmap");
        assert_eq!(
            using_urn("CalendarEvent/set"),
            "urn:ietf:params:jmap:calendars"
        );
        assert_eq!(using_urn("Identity/get"), "urn:ietf:params:jmap:submission");
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
        assert!(check_method_error(r.by_call_id("g").unwrap()).is_ok());
        let err = check_method_error(r.by_call_id("x").unwrap()).unwrap_err();
        assert!(matches!(err, JmapError::Method { .. }));
    }

    #[test]
    fn anchor_not_found_is_classified() {
        let v = json!({ "methodResponses": [["error", {"type": "anchorNotFound"}, "q"]] });
        let r = Response::parse(v).unwrap();
        let err = check_method_error(r.first().unwrap()).unwrap_err();
        assert!(matches!(err, JmapError::AnchorNotFound));
    }
}
