/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt::Write;

use serde_json::{Value, json};

use crate::error::Error;
use crate::jmap::error::JmapError;
use crate::jmap::http::JmapClient;
use crate::jmap::request::Request;
use crate::jmap::session::Session;

#[derive(Debug, Clone)]
pub enum AccountSelector {
    Id(String),
    Name(String),
}

const PRINCIPALS_URN: &str = "urn:ietf:params:jmap:principals";
const STATUS_BAD_REQUEST: u16 = 400;

impl AccountSelector {
    pub fn resolve(&self, session: &Session, client: &JmapClient) -> Result<String, Error> {
        match self {
            AccountSelector::Id(id) => Ok(id.clone()),
            AccountSelector::Name(name) => match session
                .accounts
                .iter()
                .find(|(_, account)| account.name == *name)
            {
                Some((id, _)) => Ok(id.clone()),
                None => AccountSelector::resolve_via_principal(name, session, client),
            },
        }
    }

    fn resolve_via_principal(
        name: &str,
        session: &Session,
        client: &JmapClient,
    ) -> Result<String, Error> {
        if !session.capabilities.contains_key(PRINCIPALS_URN) {
            return Err(AccountSelector::unsupported_principals(name, session));
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
            Err(JmapError::HttpStatus {
                status: STATUS_BAD_REQUEST,
                body,
            }) if body.contains("unknownCapability") => {
                return Err(AccountSelector::unsupported_principals(name, session));
            }
            Err(e) => {
                return Err(Error::Account(format!(
                    "principal resolution request failed: {e}"
                )));
            }
        };

        let account_error = |e: JmapError| Error::Account(e.to_string());
        resp.by_call_id("q")
            .and_then(|query| query.check_error())
            .map_err(account_error)?;
        let get = resp.by_call_id("g").map_err(account_error)?;
        get.check_error().map_err(account_error)?;

        let list = get
            .args
            .get("list")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Account("Principal/get returned no list".to_owned()))?;

        let mut matches = list
            .iter()
            .filter(|p| p.get("name").and_then(Value::as_str) == Some(name));
        match (matches.next(), matches.next()) {
            (None, _) => Err(Error::Account(format!(
                "account name '{name}' not found via Principal/query"
            ))),
            (Some(only), None) => Principal(only).account_id(name),
            (Some(first), Some(second)) => {
                let mut ids = String::new();
                for (i, id) in [first, second]
                    .into_iter()
                    .chain(matches)
                    .filter_map(|p| p.get("id").and_then(Value::as_str))
                    .enumerate()
                {
                    if i > 0 {
                        ids.push_str(", ");
                    }
                    ids.push_str(id);
                }
                Err(Error::Account(format!(
                    "account name '{name}' is ambiguous; candidate principals: {ids}"
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
}

struct Principal<'a>(&'a Value);

impl Principal<'_> {
    fn account_id(&self, name: &str) -> Result<String, Error> {
        if let Some(accounts) = self.0.get("accounts").and_then(Value::as_object) {
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
        self.0
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                Error::Account(format!(
                    "principal for '{name}' has no resolvable data account id"
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ExitStatus;
    use crate::logging::{Level, Logger};
    use crate::net::auth::Auth;
    use crate::net::http::{ClientOptions, TlsPolicy};
    use crate::net::retry::RetryPolicy;

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

    fn client() -> JmapClient {
        JmapClient::new(
            Auth::Bearer { token: "t".into() },
            ClientOptions {
                retry: RetryPolicy::new(0),
                tls: TlsPolicy::Verify,
                logger: Logger::new(Level::Default),
            },
        )
    }

    #[test]
    fn id_selector_is_verbatim() {
        let s = session_with("alice", "a");
        assert_eq!(
            AccountSelector::Id("zzz".into())
                .resolve(&s, &client())
                .unwrap(),
            "zzz"
        );
    }

    #[test]
    fn name_selector_exact_case_sensitive_match() {
        let s = session_with("alice@example.org", "w");
        assert_eq!(
            AccountSelector::Name("alice@example.org".into())
                .resolve(&s, &client())
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
        assert_eq!(Principal(matches[0]).account_id("alice").unwrap(), "w");
    }

    #[test]
    fn missing_principals_capability_is_actionable_without_a_request() {
        let s = session_with("alice@example.org", "w");
        let err = AccountSelector::Name("bob@example.org".into())
            .resolve(&s, &client())
            .unwrap_err();
        assert_eq!(err.exit_status(), ExitStatus::Account);
        let msg = err.to_string();
        assert!(msg.contains(PRINCIPALS_URN));
        assert!(msg.contains("--account-id"));
        assert!(msg.contains("alice@example.org (w)"));
        assert!(msg.contains("bob@example.org"));
    }

    #[test]
    fn extract_falls_back_to_principal_id() {
        let p = json!({ "id": "acc-7", "name": "bob", "accounts": {} });
        assert_eq!(Principal(&p).account_id("bob").unwrap(), "acc-7");
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
        assert_eq!(
            Principal(&p).account_id("bob@example.org").unwrap(),
            "data-9"
        );
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
        assert_eq!(Principal(&p).account_id("bob").unwrap(), "data-9");
    }
}
