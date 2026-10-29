/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::error::Error;
use crate::jmap::error::JmapError;
use crate::jmap::http::HttpClient;
use crate::jmap::request::{Request, check_method_error};
use crate::jmap::session::Session;
use serde_json::{Value, json};
use std::fmt::Write;

#[derive(Debug, Clone)]
pub enum AccountSelector {
    Id(String),
    Name(String),
}

const PRINCIPALS_URN: &str = "urn:ietf:params:jmap:principals";

pub fn resolve(
    selector: &AccountSelector,
    session: &Session,
    client: &HttpClient,
) -> Result<String, Error> {
    match selector {
        AccountSelector::Id(id) => Ok(id.clone()),
        AccountSelector::Name(name) => {
            for (id, account) in &session.accounts {
                if account.name == *name {
                    return Ok(id.clone());
                }
            }
            resolve_via_principal(name, session, client)
        }
    }
}

fn resolve_via_principal(
    name: &str,
    session: &Session,
    client: &HttpClient,
) -> Result<String, Error> {
    if !session.capabilities.contains_key(PRINCIPALS_URN) {
        return Err(unsupported_principals(name, session));
    }

    let mut req = Request::new();
    req.call(
        "Principal/query",
        json!({ "filter": { "name": name } }),
        "q",
    );
    req.call(
        "Principal/get",
        json!({
            "#ids": { "resultOf": "q", "name": "Principal/query", "path": "/ids" },
            "properties": ["id", "name", "accounts"]
        }),
        "g",
    );
    let resp = match req.send(client, &session.api_url) {
        Ok(resp) => resp,
        Err(JmapError::HttpStatus { status: 400, body }) if body.contains("unknownCapability") => {
            return Err(unsupported_principals(name, session));
        }
        Err(e) => {
            return Err(Error::Account(format!(
                "principal resolution request failed: {e}"
            )));
        }
    };

    let query = resp
        .by_call_id("q")
        .map_err(|e| Error::Account(e.to_string()))?;
    check_method_error(query).map_err(|e| Error::Account(e.to_string()))?;
    let get = resp
        .by_call_id("g")
        .map_err(|e| Error::Account(e.to_string()))?;
    check_method_error(get).map_err(|e| Error::Account(e.to_string()))?;

    let list = get
        .args
        .get("list")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Account("Principal/get returned no list".to_owned()))?;

    let mut matches: Vec<&Value> = list
        .iter()
        .filter(|p| {
            p.get("name")
                .and_then(Value::as_str)
                .map(|n| n == name)
                .unwrap_or(false)
        })
        .collect();

    match matches.len() {
        0 => Err(Error::Account(format!(
            "account name '{name}' not found via Principal/query"
        ))),
        1 => extract_account_id(matches.remove(0), name),
        _ => {
            let ids: Vec<String> = matches
                .iter()
                .filter_map(|p| p.get("id").and_then(Value::as_str).map(str::to_owned))
                .collect();
            Err(Error::Account(format!(
                "account name '{name}' is ambiguous; candidate principals: {}",
                ids.join(", ")
            )))
        }
    }
}

fn unsupported_principals(name: &str, session: &Session) -> Error {
    let mut available = String::new();
    for (id, account) in &session.accounts {
        if !available.is_empty() {
            available.push_str(", ");
        }
        let _ = write!(available, "{} ({id})", account.name);
    }
    if available.is_empty() {
        available.push_str("(none)");
    }
    Error::Account(format!(
        "account name '{name}' is not enumerated in this session and the source server does not \
         support '{PRINCIPALS_URN}', which is required to resolve an account by name from an \
         administrator session. Accounts visible to these credentials: {available}. Pass \
         --account-id <id> to use an account id verbatim (no principals lookup is performed), \
         authenticate directly as the target user, or import over IMAP instead."
    ))
}

fn extract_account_id(principal: &Value, name: &str) -> Result<String, Error> {
    if let Some(accounts) = principal.get("accounts").and_then(Value::as_object) {
        let mut keys = accounts.keys();
        if let (Some(only), None) = (keys.next(), keys.next()) {
            return Ok(only.clone());
        }
        let named = accounts
            .iter()
            .find(|(_, account)| account.get("name").and_then(Value::as_str) == Some(name));
        if let Some((id, _)) = named {
            return Ok(id.clone());
        }
    }
    principal
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            Error::Account(format!(
                "principal for '{name}' has no resolvable data account id"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_with(name: &str, id: &str) -> Session {
        let raw = serde_json::json!({
            "apiUrl": "https://example.org/api",
            "uploadUrl": "https://example.org/upload",
            "downloadUrl": "https://example.org/download",
            "capabilities": {},
            "accounts": { id: { "name": name } }
        });
        serde_json::from_value(raw).unwrap()
    }

    fn client() -> HttpClient {
        HttpClient::new(
            crate::jmap::http::Auth::Bearer { token: "t".into() },
            crate::jmap::http::RetryPolicy::new(0),
            false,
        )
    }

    #[test]
    fn id_selector_is_verbatim() {
        let s = session_with("alice", "a");
        assert_eq!(
            resolve(&AccountSelector::Id("zzz".into()), &s, &client()).unwrap(),
            "zzz"
        );
    }

    #[test]
    fn name_selector_exact_case_sensitive_match() {
        let s = session_with("alice@example.org", "w");
        assert_eq!(
            resolve(
                &AccountSelector::Name("alice@example.org".into()),
                &s,
                &client()
            )
            .unwrap(),
            "w"
        );
    }

    #[test]
    fn substring_near_match_is_rejected_exactly() {
        let list = json!([
            { "id": "p1", "name": "alice2", "accounts": {} },
            { "id": "p2", "name": "alice", "accounts": {
                "w": { "urn:ietf:params:jmap:principals:owner":
                       { "accountIdForPrincipal": "w" } } } }
        ]);
        let matches: Vec<&Value> = list
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p.get("name").and_then(Value::as_str) == Some("alice"))
            .collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(extract_account_id(matches[0], "alice").unwrap(), "w");
    }

    #[test]
    fn missing_principals_capability_is_actionable_without_a_request() {
        let s = session_with("alice@example.org", "w");
        let err = resolve(
            &AccountSelector::Name("bob@example.org".into()),
            &s,
            &client(),
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 3);
        let msg = err.to_string();
        assert!(msg.contains(PRINCIPALS_URN));
        assert!(msg.contains("--account-id"));
        assert!(msg.contains("alice@example.org (w)"));
        assert!(msg.contains("bob@example.org"));
    }

    #[test]
    fn extract_falls_back_to_principal_id() {
        let p = json!({ "id": "acc-7", "name": "bob", "accounts": {} });
        assert_eq!(extract_account_id(&p, "bob").unwrap(), "acc-7");
    }

    // RFC 9670: the data account is the key of "accounts", not accountIdForPrincipal.
    #[test]
    fn extract_uses_accounts_key_not_account_id_for_principal() {
        let p = json!({
            "id": "px",
            "name": "bob@example.org",
            "accounts": { "data-9": {
                "name": "bob@example.org",
                "isPersonal": false,
                "accountCapabilities": {
                    "urn:ietf:params:jmap:principals:owner":
                        { "accountIdForPrincipal": "admin-1", "principalId": "px" }
                }
            } }
        });
        assert_eq!(extract_account_id(&p, "bob@example.org").unwrap(), "data-9");
    }

    #[test]
    fn extract_picks_the_named_account_among_several() {
        let p = json!({
            "id": "px",
            "name": "bob",
            "accounts": {
                "shared-1": { "name": "team" },
                "data-9": { "name": "bob" }
            }
        });
        assert_eq!(extract_account_id(&p, "bob").unwrap(), "data-9");
    }
}
