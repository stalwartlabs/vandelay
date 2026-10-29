/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::sync::Arc;

use encodify::base64::STANDARD;
use serde_json::{Value, json};
use ureq::Agent;
use ureq::config::RedirectAuthHeaders;
use ureq::tls::{TlsConfig, TlsProvider};

use super::error::{SeedError, SeedResult};

pub struct Jmap {
    agent: Agent,
    api_url: String,
    upload_url: String,
    auth: String,
    pub account_id: String,
}

fn build_agent() -> Agent {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let tls = TlsConfig::builder()
        .provider(TlsProvider::Rustls)
        .unversioned_rustls_crypto_provider(provider)
        .disable_verification(true)
        .build();
    Agent::config_builder()
        .tls_config(tls)
        .http_status_as_error(false)
        .max_redirects(10)
        .redirect_auth_headers(RedirectAuthHeaders::SameHost)
        .build()
        .new_agent()
}

fn basic(user: &str, password: &str) -> String {
    let mut raw = String::with_capacity(user.len() + password.len() + 1);
    raw.push_str(user);
    raw.push(':');
    raw.push_str(password);
    let mut header = String::from("Basic ");
    STANDARD.encode_append(raw, &mut header);
    header
}

impl Jmap {
    pub fn connect(base: &str, user: &str, password: &str) -> SeedResult<Jmap> {
        let agent = build_agent();
        let auth = basic(user, password);
        let session = discover(&agent, base, &auth)?;
        let account_id = primary_account(&session)?;
        let api_url = string_field(&session, "apiUrl")?;
        let upload_url = string_field(&session, "uploadUrl")?;
        Ok(Jmap {
            agent,
            api_url,
            upload_url,
            auth,
            account_id,
        })
    }

    pub fn request(&self, using: &[&str], method_calls: Value) -> SeedResult<Value> {
        let body = json!({ "using": using, "methodCalls": method_calls });
        let mut resp = self
            .agent
            .post(&self.api_url)
            .header("Authorization", &self.auth)
            .header("Content-Type", "application/json")
            .send_json(&body)
            .map_err(|e| SeedError::Http(e.to_string()))?;
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| SeedError::Http(e.to_string()))?;
        if status != 200 {
            return Err(SeedError::Http(format!("status {status}: {text}")));
        }
        let parsed: Value = serde_json::from_str(&text)?;
        Ok(parsed)
    }

    pub fn call(
        &self,
        using: &[&str],
        method: &str,
        account_id: &str,
        mut args: Value,
    ) -> SeedResult<Value> {
        if let Value::Object(map) = &mut args {
            map.insert("accountId".to_owned(), Value::String(account_id.to_owned()));
        }
        let calls = json!([[method, args, "c0"]]);
        let parsed = self.request(using, calls)?;
        let response = first_response(&parsed, method)?;
        Ok(response)
    }

    pub fn set_create(
        &self,
        using: &[&str],
        method: &str,
        account_id: &str,
        creates: Value,
        extra_args: &[(&str, Value)],
    ) -> SeedResult<Value> {
        let mut args = json!({ "create": creates });
        if let Value::Object(map) = &mut args {
            for (k, v) in extra_args {
                map.insert((*k).to_owned(), v.clone());
            }
        }
        let response = self.call(using, method, account_id, args)?;
        if let Some(not_created) = response.get("notCreated")
            && not_created.is_object()
            && !not_created
                .as_object()
                .map(|m| m.is_empty())
                .unwrap_or(true)
        {
            return Err(SeedError::Method {
                method: method.to_owned(),
                detail: format!("notCreated: {not_created}"),
            });
        }
        Ok(response)
    }

    pub fn upload(&self, account_id: &str, content_type: &str, bytes: &[u8]) -> SeedResult<String> {
        let url = self.upload_url.replace("{accountId}", account_id);
        let mut resp = self
            .agent
            .post(&url)
            .header("Authorization", &self.auth)
            .header("Content-Type", content_type)
            .send(bytes)
            .map_err(|e| SeedError::Http(e.to_string()))?;
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| SeedError::Http(e.to_string()))?;
        if status != 200 {
            return Err(SeedError::Http(format!("upload status {status}: {text}")));
        }
        let parsed: Value = serde_json::from_str(&text)?;
        parsed
            .get("blobId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| SeedError::Shape(format!("upload response missing blobId: {text}")))
    }
}

fn discover(agent: &Agent, base: &str, auth: &str) -> SeedResult<Value> {
    let candidates = if base.contains("/.well-known/jmap") {
        vec![base.to_owned()]
    } else {
        vec![
            base.to_owned(),
            format!("{}/.well-known/jmap", base.trim_end_matches('/')),
        ]
    };
    let mut last = String::new();
    for url in candidates {
        let mut resp = agent
            .get(&url)
            .header("Authorization", auth)
            .call()
            .map_err(|e| SeedError::Http(e.to_string()))?;
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| SeedError::Http(e.to_string()))?;
        if status == 200
            && let Ok(v) = serde_json::from_str::<Value>(&text)
            && v.get("apiUrl").is_some()
            && v.get("accounts")
                .and_then(Value::as_object)
                .map(|a| !a.is_empty())
                .unwrap_or(false)
        {
            return Ok(v);
        }
        last = format!("status {status}: {text}");
    }
    Err(SeedError::Shape(format!(
        "no JMAP session object found ({last})"
    )))
}

fn string_field(session: &Value, key: &str) -> SeedResult<String> {
    session
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| SeedError::Shape(format!("session missing {key}")))
}

fn primary_account(session: &Value) -> SeedResult<String> {
    if let Some(id) = session
        .get("primaryAccounts")
        .and_then(|p| p.get("urn:ietf:params:jmap:mail"))
        .and_then(Value::as_str)
    {
        return Ok(id.to_owned());
    }
    session
        .get("accounts")
        .and_then(Value::as_object)
        .and_then(|m| m.keys().next().cloned())
        .ok_or_else(|| SeedError::Shape("session has no accounts".to_owned()))
}

fn first_response(parsed: &Value, method: &str) -> SeedResult<Value> {
    let responses = parsed
        .get("methodResponses")
        .and_then(Value::as_array)
        .ok_or_else(|| SeedError::Shape("response missing methodResponses".to_owned()))?;
    let first = responses
        .first()
        .and_then(Value::as_array)
        .ok_or_else(|| SeedError::Shape("empty methodResponses".to_owned()))?;
    let name = first.first().and_then(Value::as_str).unwrap_or_default();
    let payload = first
        .get(1)
        .cloned()
        .ok_or_else(|| SeedError::Shape("method response missing payload".to_owned()))?;
    if name == "error" {
        return Err(SeedError::Method {
            method: method.to_owned(),
            detail: payload.to_string(),
        });
    }
    Ok(payload)
}
