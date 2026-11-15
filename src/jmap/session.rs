/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt;

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::error::Error;
use crate::jmap::error::JmapError;
use crate::jmap::http::JmapClient;
use crate::net::encode::{Percent, SafeSet};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub api_url: String,
    pub upload_url: String,
    pub download_url: String,
    pub capabilities: IndexMap<String, Value>,
    pub accounts: IndexMap<String, Account>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub name: String,
    #[serde(default)]
    pub account_capabilities: IndexMap<String, Value>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    pub max_objects_in_get: u64,
    pub max_objects_in_set: u64,
    pub max_calls_in_request: u64,
    pub max_concurrent_requests: u64,
    pub max_concurrent_upload: u64,
    pub max_size_request: u64,
    pub max_size_upload: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Api,
    Upload,
}

impl Limits {
    pub fn workers(&self, requested: usize, lane: Lane) -> usize {
        let server_cap = match lane {
            Lane::Api => self.max_concurrent_requests,
            Lane::Upload => self.max_concurrent_upload,
        };
        requested.clamp(1, usize::try_from(server_cap.max(1)).unwrap_or(usize::MAX))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SessionUrl<'a>(pub &'a str);

impl<'a> SessionUrl<'a> {
    fn split_scheme(self) -> Option<(&'a str, &'a str)> {
        let (scheme, rest) = self.0.split_once(':')?;
        let (first, tail) = scheme.as_bytes().split_first()?;
        (first.is_ascii_alphabetic()
            && tail
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')))
        .then_some((scheme, rest))
    }

    pub fn resolve(self, reference: &str) -> String {
        let base = self.0;
        if reference.is_empty() {
            return base.to_owned();
        }
        if SessionUrl(reference).split_scheme().is_some() {
            return reference.to_owned();
        }
        let Some((scheme, after_scheme)) = self.split_scheme() else {
            return reference.to_owned();
        };
        let Some(authority_and_path) = after_scheme.strip_prefix("//") else {
            return reference.to_owned();
        };
        let mut out = String::with_capacity(scheme.len() + 3 + base.len() + reference.len());
        out.push_str(scheme);
        out.push_str("://");
        if let Some(rest) = reference.strip_prefix("//") {
            out.push_str(rest);
            return out;
        }
        let authority = authority_and_path
            .split(['/', '?', '#'])
            .next()
            .unwrap_or_default();
        out.push_str(authority);
        if reference.starts_with('/') {
            out.push_str(reference);
            return out;
        }
        let path = authority_and_path
            .strip_prefix(authority)
            .unwrap_or_default()
            .split(['?', '#'])
            .next()
            .unwrap_or_default();
        if let Some((dir, _)) = path.rsplit_once('/') {
            out.push_str(dir);
        }
        out.push('/');
        out.push_str(reference);
        out
    }

    pub fn origin(self) -> Option<String> {
        let origin = Url::parse(self.0).ok()?.origin();
        origin.is_tuple().then(|| origin.ascii_serialization())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginMismatch {
    pub fields: Vec<&'static str>,
    pub session_origin: String,
    pub connect_origin: String,
}

impl fmt::Display for OriginMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("session advertises ")?;
        for (index, field) in self.fields.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            f.write_str(field)?;
        }
        write!(
            f,
            " on {} (connected to {}); vandelay must use the advertised URL. \
             If this is wrong, fix the server's advertised HTTP URL setting.",
            self.session_origin, self.connect_origin
        )
    }
}

impl Session {
    fn parse(body: &str) -> Option<Session> {
        let value: Value = serde_json::from_str(body).ok()?;
        let has_shape = value.get("apiUrl").is_some()
            && value.get("accounts").is_some()
            && value.get("capabilities").is_some();
        if !has_shape {
            return None;
        }
        serde_json::from_value(value).ok()
    }

    fn well_known_url(base: &str) -> String {
        let trimmed = base.trim_end_matches('/');
        format!("{trimmed}/.well-known/jmap")
    }

    pub fn discover(client: &JmapClient, url: &str) -> Result<Session, JmapError> {
        match client.get(url) {
            Ok(direct) => {
                if let Some(session) = Session::parse(&direct) {
                    return session.rebased_on(url).ensure_authenticated();
                }
            }
            Err(JmapError::Auth(m)) => return Err(JmapError::Auth(m)),
            Err(_) => {}
        }
        let well_known = Session::well_known_url(url);
        match client.get(&well_known) {
            Ok(body) => {
                if let Some(session) = Session::parse(&body) {
                    return session.rebased_on(&well_known).ensure_authenticated();
                }
                Err(JmapError::Connect(format!(
                    "no JMAP Session object at {url} or {well_known}"
                )))
            }
            Err(JmapError::Auth(m)) => Err(JmapError::Auth(m)),
            Err(e) => Err(JmapError::Connect(format!(
                "session discovery failed: no Session at {url}; {well_known}: {e}"
            ))),
        }
    }

    fn rebased_on(mut self, session_resource_url: &str) -> Session {
        self.api_url = SessionUrl(session_resource_url).resolve(&self.api_url);
        self.upload_url = SessionUrl(session_resource_url).resolve(&self.upload_url);
        self.download_url = SessionUrl(session_resource_url).resolve(&self.download_url);
        self
    }

    pub fn origin_mismatches(&self, connect_url: &str) -> Vec<OriginMismatch> {
        let Some(connect_origin) = SessionUrl(connect_url).origin() else {
            return Vec::new();
        };
        let mut mismatches: Vec<OriginMismatch> = Vec::new();
        for (field, value) in [
            ("apiUrl", &self.api_url),
            ("uploadUrl", &self.upload_url),
            ("downloadUrl", &self.download_url),
        ] {
            let Some(session_origin) = SessionUrl(value).origin() else {
                continue;
            };
            if session_origin == connect_origin {
                continue;
            }
            match mismatches
                .iter_mut()
                .find(|m| m.session_origin == session_origin)
            {
                Some(existing) => existing.fields.push(field),
                None => mismatches.push(OriginMismatch {
                    fields: vec![field],
                    session_origin,
                    connect_origin: connect_origin.clone(),
                }),
            }
        }
        mismatches
    }

    fn ensure_authenticated(self) -> Result<Session, JmapError> {
        if self.accounts.is_empty() {
            return Err(JmapError::Auth(
                "session enumerates no accounts (anonymous session: authentication failed)"
                    .to_owned(),
            ));
        }
        Ok(self)
    }

    pub fn core_limits(&self) -> Result<Limits, Error> {
        let core = self
            .capabilities
            .get("urn:ietf:params:jmap:core")
            .ok_or_else(|| {
                Error::Connection("session has no urn:ietf:params:jmap:core capability".to_owned())
            })?;
        serde_json::from_value(core.clone())
            .map_err(|e| Error::Connection(format!("session core capability is malformed: {e}")))
    }

    pub fn account(&self, account_id: &str) -> Option<&Account> {
        self.accounts.get(account_id)
    }

    pub fn account_capabilities(&self, account_id: &str) -> Option<&IndexMap<String, Value>> {
        self.accounts
            .get(account_id)
            .map(|a| &a.account_capabilities)
    }

    pub fn supports(&self, account_id: &str, capability_urn: &str) -> bool {
        self.account_capabilities(account_id)
            .map(|caps| caps.contains_key(capability_urn))
            .unwrap_or(false)
    }

    pub fn may_support(&self, account_id: &str, capability_urn: &str) -> bool {
        match self.account_capabilities(account_id) {
            Some(caps) => caps.contains_key(capability_urn),
            None => self.capabilities.contains_key(capability_urn),
        }
    }

    pub fn upload_url_for(&self, account_id: &str) -> String {
        self.upload_url.replace(
            "{accountId}",
            &Percent::encode(account_id, SafeSet::Unreserved),
        )
    }

    pub fn download_url_for(
        &self,
        account_id: &str,
        blob_id: &str,
        type_hint: &str,
        name: &str,
    ) -> String {
        self.download_url
            .replace(
                "{accountId}",
                &Percent::encode(account_id, SafeSet::Unreserved),
            )
            .replace("{blobId}", &Percent::encode(blob_id, SafeSet::Unreserved))
            .replace("{type}", &Percent::encode(type_hint, SafeSet::Unreserved))
            .replace("{name}", &Percent::encode(name, SafeSet::Unreserved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_count_is_clamped_to_server_limits() {
        let l = Limits {
            max_objects_in_get: 500,
            max_objects_in_set: 500,
            max_calls_in_request: 16,
            max_concurrent_requests: 4,
            max_concurrent_upload: 2,
            max_size_request: 10_000_000,
            max_size_upload: 50_000_000,
        };
        assert_eq!(l.workers(16, Lane::Api), 4);
        assert_eq!(l.workers(16, Lane::Upload), 2);
        assert_eq!(l.workers(1, Lane::Api), 1);
        assert_eq!(l.workers(0, Lane::Api), 1);
    }

    fn raw_session() -> &'static str {
        r#"{
            "apiUrl": "https://example.org/jmap/api",
            "uploadUrl": "https://example.org/jmap/upload/{accountId}/",
            "downloadUrl": "https://example.org/jmap/download/{accountId}/{blobId}/{type}/{name}",
            "capabilities": {
                "urn:ietf:params:jmap:core": {
                    "maxObjectsInGet": 500,
                    "maxObjectsInSet": 500,
                    "maxCallsInRequest": 16,
                    "maxConcurrentRequests": 4,
                    "maxConcurrentUpload": 4,
                    "maxSizeRequest": 10000000,
                    "maxSizeUpload": 50000000
                }
            },
            "accounts": {
                "w": {
                    "name": "vspec-user@example.org",
                    "accountCapabilities": { "urn:ietf:params:jmap:mail": {} }
                }
            }
        }"#
    }

    #[test]
    fn parses_a_session_and_reads_limits() {
        let session = Session::parse(raw_session()).unwrap();
        let limits = session.core_limits().unwrap();
        assert_eq!(limits.max_objects_in_get, 500);
        assert_eq!(limits.max_size_upload, 50000000);
        assert_eq!(session.accounts["w"].name, "vspec-user@example.org");
    }

    #[test]
    fn capability_gate_reads_account_capabilities() {
        let session = Session::parse(raw_session()).unwrap();
        assert!(session.supports("w", "urn:ietf:params:jmap:mail"));
        assert!(!session.supports("w", "urn:ietf:params:jmap:filenode"));
        assert!(!session.supports("missing", "urn:ietf:params:jmap:mail"));
    }

    // Listed accounts use their own capabilities; unlisted ones fall back to the session's.
    #[test]
    fn may_support_falls_back_to_session_capabilities_for_unlisted_account() {
        let session = Session::parse(raw_session()).unwrap();
        assert!(session.may_support("w", "urn:ietf:params:jmap:mail"));
        assert!(!session.may_support("w", "urn:ietf:params:jmap:core"));
        assert!(session.may_support("missing", "urn:ietf:params:jmap:core"));
        assert!(!session.may_support("missing", "urn:ietf:params:jmap:mail"));
    }

    #[test]
    fn templates_upload_and_download_urls_with_encoding() {
        let session = Session::parse(raw_session()).unwrap();
        assert_eq!(
            session.upload_url_for("a c"),
            "https://example.org/jmap/upload/a%20c/"
        );
        assert_eq!(
            session.download_url_for("w", "G1/2", "application/sieve", "my script"),
            "https://example.org/jmap/download/w/G1%2F2/application%2Fsieve/my%20script"
        );
    }

    #[test]
    fn rejects_body_that_is_not_a_session() {
        assert!(Session::parse("{\"hello\":true}").is_none());
        assert!(Session::parse("not json").is_none());
    }

    #[test]
    fn anonymous_session_is_auth_failure() {
        let raw =
            r#"{"apiUrl":"x","uploadUrl":"u","downloadUrl":"d","capabilities":{},"accounts":{}}"#;
        let session = Session::parse(raw).unwrap();
        assert!(matches!(
            session.ensure_authenticated(),
            Err(JmapError::Auth(_))
        ));
    }

    #[test]
    fn origin_treats_the_default_port_as_equivalent() {
        assert_eq!(
            SessionUrl("https://mail.example/jmap").origin(),
            SessionUrl("https://mail.example:443/jmap").origin()
        );
        assert_eq!(
            SessionUrl("http://mail.example/jmap").origin(),
            SessionUrl("http://mail.example:80/jmap").origin()
        );
    }

    #[test]
    fn origin_distinguishes_port_host_and_scheme() {
        let base = SessionUrl("https://mail.example:10443/jmap").origin();
        assert_ne!(base, SessionUrl("https://mail.example/jmap").origin());
        assert_ne!(
            base,
            SessionUrl("https://other.example:10443/jmap").origin()
        );
        assert_ne!(base, SessionUrl("http://mail.example:10443/jmap").origin());
    }

    fn session_on(base: &str) -> Session {
        let raw = format!(
            r#"{{"apiUrl":"{base}/jmap/","uploadUrl":"{base}/jmap/upload/{{accountId}}/",
                "downloadUrl":"{base}/jmap/dl/{{accountId}}/{{blobId}}/{{type}}/{{name}}",
                "capabilities":{{}},"accounts":{{}}}}"#
        );
        Session::parse(&raw).expect("session")
    }

    #[test]
    fn matching_origin_reports_no_mismatch() {
        let session = session_on("https://mail.example:10443");
        assert!(
            session
                .origin_mismatches("https://mail.example:10443")
                .is_empty()
        );
        assert!(
            session_on("https://mail.example")
                .origin_mismatches("https://mail.example:443/")
                .is_empty()
        );
    }

    #[test]
    fn dropped_port_is_reported_once_naming_every_field() {
        let session = session_on("https://mail.example");
        let mismatches = session.origin_mismatches("https://mail.example:10443");
        assert_eq!(mismatches.len(), 1, "one warning per distinct origin");
        assert_eq!(
            mismatches[0].fields,
            vec!["apiUrl", "uploadUrl", "downloadUrl"]
        );
        assert_eq!(mismatches[0].session_origin, "https://mail.example");
        assert_eq!(mismatches[0].connect_origin, "https://mail.example:10443");
        let warning = mismatches[0].to_string();
        assert!(warning.contains("session advertises apiUrl"), "{warning}");
        assert!(
            warning.contains("on https://mail.example (connected to https://mail.example:10443)"),
            "{warning}"
        );
        assert!(warning.contains("advertised HTTP URL setting"), "{warning}");
    }

    #[test]
    fn a_split_upload_host_is_reported_separately_from_the_api_host() {
        let raw = r#"{"apiUrl":"https://mail.example:10443/jmap/",
            "uploadUrl":"https://blobs.example/upload/{accountId}/",
            "downloadUrl":"https://blobs.example/dl/{accountId}/{blobId}/{type}/{name}",
            "capabilities":{},"accounts":{}}"#;
        let session = Session::parse(raw).expect("session");
        let mismatches = session.origin_mismatches("https://mail.example:10443");
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].fields, vec!["uploadUrl", "downloadUrl"]);
        assert_eq!(mismatches[0].session_origin, "https://blobs.example");
    }

    #[test]
    fn resolve_reference_keeps_absolute_urls_verbatim() {
        assert_eq!(
            SessionUrl("https://h.example/.well-known/jmap")
                .resolve("https://other.example:8443/jmap/api"),
            "https://other.example:8443/jmap/api"
        );
    }

    #[test]
    fn resolve_reference_handles_scheme_relative_absolute_and_relative_paths() {
        let base = "https://h.example:10443/.well-known/jmap";
        assert_eq!(
            SessionUrl(base).resolve("//other.example/jmap/api"),
            "https://other.example/jmap/api"
        );
        assert_eq!(
            SessionUrl(base).resolve("/jmap/api"),
            "https://h.example:10443/jmap/api"
        );
        assert_eq!(
            SessionUrl(base).resolve("api"),
            "https://h.example:10443/.well-known/api"
        );
    }

    #[test]
    fn resolve_reference_preserves_uri_template_braces() {
        assert_eq!(
            SessionUrl("https://h.example/.well-known/jmap").resolve("/jmap/upload/{accountId}/"),
            "https://h.example/jmap/upload/{accountId}/"
        );
    }

    #[test]
    fn relative_session_urls_are_resolved_against_the_session_resource() {
        let raw = r#"{"apiUrl":"/jmap/","uploadUrl":"/jmap/upload/{accountId}/",
            "downloadUrl":"/jmap/download/{accountId}/{blobId}/{type}/{name}",
            "capabilities":{},"accounts":{}}"#;
        let session = Session::parse(raw)
            .expect("session")
            .rebased_on("https://h.example:10443/.well-known/jmap");
        assert_eq!(session.api_url, "https://h.example:10443/jmap/");
        assert_eq!(
            session.upload_url_for("w"),
            "https://h.example:10443/jmap/upload/w/"
        );
        assert!(
            session
                .origin_mismatches("https://h.example:10443")
                .is_empty(),
            "a resolved relative URL shares the connect origin"
        );
    }

    #[test]
    fn well_known_appends_correctly() {
        assert_eq!(
            Session::well_known_url("https://h.example/"),
            "https://h.example/.well-known/jmap"
        );
        assert_eq!(
            Session::well_known_url("https://h.example"),
            "https://h.example/.well-known/jmap"
        );
    }
}
