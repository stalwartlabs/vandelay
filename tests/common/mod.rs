/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

#![allow(dead_code)]

pub mod line_server;

use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs, process, thread};

use mockito::{Mock, Server};
use rusqlite::Connection;
use serde_json::{Map, Value, json};
use vandelay::archive::sources::SourceChange;
use vandelay::export::{Confirm, Export, Prune};
use vandelay::import::jmap::JmapImport;
use vandelay::jmap::account::AccountSelector;
use vandelay::jmap::connect::ConnectConfig;
use vandelay::logging::{Level, Logger};
use vandelay::net::auth::Auth;
use vandelay::net::http::TlsPolicy;
use vandelay::sync::{RunMode, RunOptions};
use vandelay::types::ObjectType;

pub const JSON: &str = "application/json";

pub fn tmp_archive(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut name = String::new();
    let _ = write!(
        name,
        "vandelay-{tag}-{}-{:?}-{seq}-{nanos}.sqlite",
        process::id(),
        thread::current().id()
    );
    let path = env::temp_dir().join(name.replace(['(', ')'], ""));
    let _ = fs::remove_file(&path);
    path
}

pub fn options(archive: &Path) -> RunOptions {
    RunOptions {
        archive: archive.to_path_buf(),
        threads: 1,
        mode: RunMode::Apply,
        max_retries: 1,
        tls: TlsPolicy::Verify,
        logger: Logger::new(Level::Quiet),
    }
}

pub fn dry_options(archive: &Path) -> RunOptions {
    RunOptions {
        mode: RunMode::DryRun,
        ..options(archive)
    }
}

pub fn live_options(archive: &Path) -> RunOptions {
    RunOptions {
        archive: archive.to_path_buf(),
        threads: 4,
        mode: RunMode::Apply,
        max_retries: 5,
        tls: TlsPolicy::AcceptInvalid,
        logger: Logger::new(Level::Default),
    }
}

pub fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

pub fn basic(user: &str, password: &str) -> Auth {
    Auth::Basic {
        user: user.to_owned(),
        password: password.to_owned(),
    }
}

pub fn connect(url: &str, auth: Auth, account_id: &str) -> ConnectConfig {
    ConnectConfig {
        url: url.to_owned(),
        auth,
        account: AccountSelector::Id(account_id.to_owned()),
    }
}

pub fn jmap_import(connect: ConnectConfig, objects: Option<Vec<ObjectType>>) -> JmapImport {
    JmapImport {
        connect,
        objects: objects.map(Into::into),
        source_change: SourceChange::Reject,
    }
}

pub fn export(connect: ConnectConfig, objects: Option<Vec<ObjectType>>, prune: Prune) -> Export {
    Export {
        connect,
        objects: objects.map(Into::into),
        prune,
        confirm: Confirm::Assume,
    }
}

pub fn json_mock(server: &mut Server, method: &str, path: &str, body: &str) -> Mock {
    server
        .mock(method, path)
        .with_status(200)
        .with_header("content-type", JSON)
        .with_body(body)
        .create()
}

pub struct SessionJson {
    base: String,
    account_id: String,
    core: Map<String, Value>,
    capabilities: Vec<&'static str>,
}

impl SessionJson {
    pub fn new(base: &str) -> SessionJson {
        let core = [
            ("maxObjectsInGet", 500),
            ("maxObjectsInSet", 500),
            ("maxCallsInRequest", 16),
            ("maxConcurrentRequests", 4),
            ("maxConcurrentUpload", 4),
            ("maxSizeRequest", 10_000_000),
            ("maxSizeUpload", 50_000_000),
        ]
        .into_iter()
        .map(|(name, value): (&str, u64)| (name.to_owned(), json!(value)))
        .collect();
        SessionJson {
            base: base.to_owned(),
            account_id: "w".to_owned(),
            core,
            capabilities: vec!["urn:ietf:params:jmap:mail"],
        }
    }

    pub fn limit(mut self, name: &str, value: u64) -> SessionJson {
        self.core.insert(name.to_owned(), json!(value));
        self
    }

    pub fn account(mut self, account_id: &str) -> SessionJson {
        self.account_id = account_id.to_owned();
        self
    }

    pub fn capability(mut self, urn: &'static str) -> SessionJson {
        self.capabilities.push(urn);
        self
    }

    pub fn all_capabilities(self) -> SessionJson {
        self.capability("urn:ietf:params:jmap:sieve")
            .capability("urn:ietf:params:jmap:contacts")
            .capability("urn:ietf:params:jmap:calendars")
            .capability("urn:ietf:params:jmap:filenode")
    }

    pub fn build(&self) -> String {
        let base = &self.base;
        let account_capabilities: Map<String, Value> = self
            .capabilities
            .iter()
            .map(|urn| ((*urn).to_owned(), json!({})))
            .collect();
        json!({
            "apiUrl": format!("{base}/jmap/api"),
            "uploadUrl": format!("{base}/jmap/upload/{{accountId}}/"),
            "downloadUrl": format!("{base}/jmap/dl/{{accountId}}/{{blobId}}/{{type}}/{{name}}"),
            "capabilities": { "urn:ietf:params:jmap:core": self.core },
            "accounts": { &self.account_id: { "name": "alice",
                "accountCapabilities": account_capabilities } }
        })
        .to_string()
    }
}

pub const GRAPH_WELL_KNOWN: [&str; 6] = [
    "inbox",
    "drafts",
    "sentitems",
    "deleteditems",
    "junkemail",
    "archive",
];

pub fn graph_mail_tree(server: &mut Server, inbox_id: &str, display_name: &str) -> Vec<Mock> {
    let mut mocks =
        vec![
        server
            .mock("GET", mockito::Matcher::Regex(r"^/me\?\$select=id".to_owned()))
            .with_status(200)
            .with_header("content-type", JSON)
            .with_body(r#"{"id":"uid-1","userPrincipalName":"alice@x.com"}"#)
            .create(),
        json_mock(
            server,
            "GET",
            "/me/mailFolders?$top=100&includeHiddenFolders=true",
            &json!({"value": [{"id": inbox_id, "displayName": display_name, "isHidden": false}]})
                .to_string(),
        ),
        json_mock(
            server,
            "GET",
            &format!("/me/mailFolders/{inbox_id}/childFolders?$top=100&includeHiddenFolders=true"),
            r#"{"value":[]}"#,
        ),
    ];
    for name in GRAPH_WELL_KNOWN {
        let id = if name == "inbox" { inbox_id } else { "OTHER" };
        mocks.push(json_mock(
            server,
            "GET",
            &format!("/me/mailFolders/{name}?$select=id"),
            &json!({ "id": id }).to_string(),
        ));
    }
    mocks
}
