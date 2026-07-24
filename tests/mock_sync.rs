/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::path::{Path, PathBuf};

use mockito::Matcher;
use serde_json::{Value, json};
use vandelay::db;
use vandelay::jmap::account::AccountSelector;
use vandelay::jmap::http::Auth;
use vandelay::logging::Logger;
use vandelay::sync::{self, CommonConfig, ConnectConfig, ExportConfig, ImportConfig};
use vandelay::types::ObjectType;

fn tmp() -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "vandelay-mocksync-{}-{:?}-{n}.sqlite",
        std::process::id(),
        std::thread::current().id(),
    ));
    let _ = std::fs::remove_file(&p);
    p
}

fn session_body(base: &str) -> String {
    json!({
        "apiUrl": format!("{base}/jmap/api"),
        "uploadUrl": format!("{base}/jmap/upload/{{accountId}}/"),
        "downloadUrl": format!("{base}/jmap/dl/{{accountId}}/{{blobId}}/{{type}}/{{name}}"),
        "capabilities": { "urn:ietf:params:jmap:core": {
            "maxObjectsInGet": 500, "maxObjectsInSet": 500, "maxCallsInRequest": 16,
            "maxConcurrentRequests": 4, "maxConcurrentUpload": 4,
            "maxSizeRequest": 10000000, "maxSizeUpload": 50000000
        } },
        "accounts": { "w": { "name": "alice",
            "accountCapabilities": { "urn:ietf:params:jmap:mail": {} } } }
    })
    .to_string()
}

#[test]
fn export_email_already_exists_is_matched_not_failed() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";

    let archive = tmp();
    {
        let conn = db::init::open(&archive).unwrap();
        conn.execute(
            "INSERT INTO mailboxes (id,name,parent_id,role) VALUES (1,'Inbox',NULL,'inbox')",
            [],
        )
        .unwrap();
        let blob = db::blobs::intern_blob(
            &conn,
            b"From: a@x\r\nSubject: hi\r\nMessage-ID: <m-1@h>\r\n\r\nbody",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO emails (blob_id,received_at,mailbox_ids,keywords)
             VALUES (?1,'2020-01-01T00:00:00Z','[1]','[\"$seen\"]')",
            rusqlite::params![blob],
        )
        .unwrap();
    }

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body(&base))
        .create();

    let _mq1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
            {"accountId":"w","ids":["t1"]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _mq2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
            {"accountId":"w","ids":[]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _mg = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/get".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/get",{"accountId":"w","list":[
            {"id":"t1","name":"Inbox","role":"inbox","parentId":null,
             "myRights":{"mayDelete":true}}],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _eq = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/query".into()))
        .with_body(
            json!({"methodResponses":[["Email/query",
            {"accountId":"w","ids":[]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _up = server
        .mock("POST", Matcher::Regex("/jmap/upload/".into()))
        .with_body(json!({"blobId":"UPLOADED"}).to_string())
        .expect(1)
        .create();
    let _imp = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/import".into()))
        .with_body(
            json!({"methodResponses":[["Email/import",{"accountId":"w",
                "notCreated":{"e1":{"type":"alreadyExists","existingId":"x9"}}},"i"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let summary = sync::export::run(
        CommonConfig {
            archive: archive.clone(),
            threads: 1,
            dry_run: false,
            max_retries: 1,
            allow_invalid_certs: false,
            logger: Logger::from_flags(true, 0),
        },
        ExportConfig {
            connect: ConnectConfig {
                url: base.clone(),
                auth: Auth::Basic {
                    user: "u".into(),
                    password: "p".into(),
                },
                account: AccountSelector::Id("w".into()),
            },
            objects: None,
            prune: false,
            yes: true,
        },
    )
    .expect("export run");

    let email = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "Email")
        .map(|(_, c)| c.clone())
        .expect("email counts");
    assert_eq!(email.created, 0);
    assert_eq!(email.failed, 0, "alreadyExists must not be a failure");
    assert_eq!(email.skipped, 1, "alreadyExists folds into matched");
    assert!(!summary.any_failed());

    let _ = std::fs::remove_file(&archive);
}

#[test]
fn export_mailbox_name_collision_merges_without_create() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";

    let archive = tmp();
    {
        let conn = db::init::open(&archive).unwrap();
        conn.execute(
            "INSERT INTO mailboxes (id,name,parent_id,role) VALUES
                 (1,'Junk Email',NULL,'junk'),
                 (2,'Junk Mail',NULL,NULL)",
            [],
        )
        .unwrap();
        let blob = db::blobs::intern_blob(
            &conn,
            b"From: a@x\r\nSubject: spam\r\nMessage-ID: <m-2@h>\r\n\r\nbody",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO emails (blob_id,received_at,mailbox_ids,keywords)
             VALUES (?1,'2020-01-01T00:00:00Z','[2]','[\"$seen\"]')",
            rusqlite::params![blob],
        )
        .unwrap();
    }

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body(&base))
        .create();

    let _mq1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
            {"accountId":"w","ids":["c"]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _mq2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
            {"accountId":"w","ids":[]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _mg = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/get".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/get",{"accountId":"w","list":[
            {"id":"c","name":"Junk Mail","role":"junk","parentId":null,
             "myRights":{"mayDelete":true}}],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let no_set = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/set".into()))
        .expect(0)
        .create();

    let _eq = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/query".into()))
        .with_body(
            json!({"methodResponses":[["Email/query",
            {"accountId":"w","ids":[]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _up = server
        .mock("POST", Matcher::Regex("/jmap/upload/".into()))
        .with_body(json!({"blobId":"UPLOADED"}).to_string())
        .expect(1)
        .create();
    let import_into_c = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("Email/import".into()),
            Matcher::Regex(r#""c":true"#.into()),
        ]))
        .with_body(
            json!({"methodResponses":[["Email/import",{"accountId":"w",
                "created":{"e1":{"id":"E1","blobId":"b","threadId":"t","size":10}}},"i"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let summary = sync::export::run(
        CommonConfig {
            archive: archive.clone(),
            threads: 1,
            dry_run: false,
            max_retries: 1,
            allow_invalid_certs: false,
            logger: Logger::from_flags(true, 0),
        },
        ExportConfig {
            connect: ConnectConfig {
                url: base.clone(),
                auth: Auth::Basic {
                    user: "u".into(),
                    password: "p".into(),
                },
                account: AccountSelector::Id("w".into()),
            },
            objects: None,
            prune: false,
            yes: true,
        },
    )
    .expect("export run");

    let mailbox = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "Mailbox")
        .map(|(_, c)| c.clone())
        .expect("mailbox counts");
    assert_eq!(mailbox.created, 0, "no mailbox is created");
    assert_eq!(mailbox.failed, 0, "the name collision is not a failure");
    assert_eq!(
        mailbox.skipped, 2,
        "role-matched Junk Email + merged Junk Mail"
    );

    let email = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "Email")
        .map(|(_, c)| c.clone())
        .expect("email counts");
    assert_eq!(email.created, 1, "the Junk Mail email lands on the target");
    assert_eq!(email.failed, 0);
    assert!(!summary.any_failed());

    no_set.assert();
    import_into_c.assert();
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn export_mailbox_already_exists_maps_existing_id() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";

    let archive = tmp();
    {
        let conn = db::init::open(&archive).unwrap();
        conn.execute(
            "INSERT INTO mailboxes (id,name,parent_id,role) VALUES (1,'Junk Mail',NULL,NULL)",
            [],
        )
        .unwrap();
        let blob = db::blobs::intern_blob(
            &conn,
            b"From: a@x\r\nSubject: spam\r\nMessage-ID: <m-3@h>\r\n\r\nbody",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO emails (blob_id,received_at,mailbox_ids,keywords)
             VALUES (?1,'2020-01-01T00:00:00Z','[1]','[\"$seen\"]')",
            rusqlite::params![blob],
        )
        .unwrap();
    }

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body(&base))
        .create();

    let _mq1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
            {"accountId":"w","ids":["t1"]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _mq2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
            {"accountId":"w","ids":[]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _mg = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/get".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/get",{"accountId":"w","list":[
            {"id":"t1","name":"Inbox","role":"inbox","parentId":null,
             "myRights":{"mayDelete":true}}],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let set_collides = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/set".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/set",{"accountId":"w",
                "notCreated":{"c1":{"type":"alreadyExists","existingId":"c"}}},"s"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let _eq = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/query".into()))
        .with_body(
            json!({"methodResponses":[["Email/query",
            {"accountId":"w","ids":[]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _up = server
        .mock("POST", Matcher::Regex("/jmap/upload/".into()))
        .with_body(json!({"blobId":"UPLOADED"}).to_string())
        .expect(1)
        .create();
    let import_into_c = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("Email/import".into()),
            Matcher::Regex(r#""c":true"#.into()),
        ]))
        .with_body(
            json!({"methodResponses":[["Email/import",{"accountId":"w",
                "created":{"e1":{"id":"E1","blobId":"b","threadId":"t","size":10}}},"i"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let summary = sync::export::run(
        CommonConfig {
            archive: archive.clone(),
            threads: 1,
            dry_run: false,
            max_retries: 1,
            allow_invalid_certs: false,
            logger: Logger::from_flags(true, 0),
        },
        ExportConfig {
            connect: ConnectConfig {
                url: base.clone(),
                auth: Auth::Basic {
                    user: "u".into(),
                    password: "p".into(),
                },
                account: AccountSelector::Id("w".into()),
            },
            objects: None,
            prune: false,
            yes: true,
        },
    )
    .expect("export run");

    let mailbox = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "Mailbox")
        .map(|(_, c)| c.clone())
        .expect("mailbox counts");
    assert_eq!(mailbox.created, 0, "the create collided");
    assert_eq!(
        mailbox.failed, 0,
        "alreadyExists on Mailbox/set is not a failure"
    );
    assert_eq!(mailbox.skipped, 1, "existingId folds into matched");

    let email = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "Email")
        .map(|(_, c)| c.clone())
        .expect("email counts");
    assert_eq!(email.created, 1, "the email lands in the existing folder");
    assert_eq!(email.failed, 0);
    assert!(!summary.any_failed());

    set_collides.assert();
    import_into_c.assert();
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn email_export_sends_one_email_per_import_call() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";

    let archive = tmp();
    {
        let conn = db::init::open(&archive).unwrap();
        conn.execute(
            "INSERT INTO mailboxes (id,name,parent_id,role) VALUES (1,'Inbox',NULL,'inbox')",
            [],
        )
        .unwrap();
        for n in 1..=2 {
            let raw =
                format!("From: a@x\r\nSubject: m{n}\r\nMessage-ID: <m-{n}@h>\r\n\r\nbody {n}",);
            let blob = db::blobs::intern_blob(&conn, raw.as_bytes()).unwrap();
            conn.execute(
                "INSERT INTO emails (blob_id,received_at,mailbox_ids,keywords)
                 VALUES (?1,'2020-01-01T00:00:00Z','[1]','[]')",
                rusqlite::params![blob],
            )
            .unwrap();
        }
    }

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body(&base))
        .create();

    let _mq = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
                {"accountId":"w","ids":["t1"]},"q"]]})
            .to_string(),
        )
        .expect_at_least(1)
        .create();
    let _mq_empty = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("Mailbox/query".into()),
            Matcher::Regex("anchor".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
                {"accountId":"w","ids":[]},"q"]]})
            .to_string(),
        )
        .create();
    let _mg = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/get".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/get",{"accountId":"w","list":[
                {"id":"t1","name":"Inbox","role":"inbox","parentId":null,
                 "myRights":{"mayDelete":true}}],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect_at_least(1)
        .create();
    let _eq = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/query".into()))
        .with_body(
            json!({"methodResponses":[["Email/query",
                {"accountId":"w","ids":[]},"q"]]})
            .to_string(),
        )
        .expect_at_least(1)
        .create();

    let _ups = server
        .mock("POST", Matcher::Regex("/jmap/upload/".into()))
        .with_body(json!({"blobId":"UP1"}).to_string())
        .expect(2)
        .create();

    let single_only = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("Email/import".into()),
            Matcher::Regex("e1".into()),
            Matcher::Regex("e2".into()),
        ]))
        .expect(0)
        .create();

    let imports = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/import".into()))
        .with_body(
            json!({"methodResponses":[["Email/import",
                {"accountId":"w","created":{"e":{"id":"x","blobId":"b","threadId":"t","size":10}}},"i"]]})
            .to_string(),
        )
        .expect(2)
        .create();

    let summary = sync::export::run(
        CommonConfig {
            archive: archive.clone(),
            threads: 1,
            dry_run: false,
            max_retries: 0,
            allow_invalid_certs: false,
            logger: Logger::from_flags(true, 0),
        },
        ExportConfig {
            connect: ConnectConfig {
                url: base.clone(),
                auth: Auth::Basic {
                    user: "u".into(),
                    password: "p".into(),
                },
                account: AccountSelector::Id("w".into()),
            },
            objects: None,
            prune: false,
            yes: true,
        },
    )
    .expect("export run");

    let email = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "Email")
        .map(|(_, c)| c.clone())
        .expect("email counts");
    assert_eq!(email.created, 2, "both emails imported in per-item rounds");
    assert_eq!(email.failed, 0, "no per-unit failure");
    assert!(!summary.any_failed(), "no whole-run failure");

    single_only.assert();
    imports.assert();
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn export_email_blob_not_found_reuploads_and_retries() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";

    let archive = tmp();
    {
        let conn = db::init::open(&archive).unwrap();
        conn.execute(
            "INSERT INTO mailboxes (id,name,parent_id,role) VALUES (1,'Inbox',NULL,'inbox')",
            [],
        )
        .unwrap();
        let raw = b"From: a@x\r\nSubject: dup\r\nMessage-ID: <dup-1@h>\r\n\r\nbody";
        let blob = db::blobs::intern_blob(&conn, raw).unwrap();
        for _ in 0..2 {
            conn.execute(
                "INSERT INTO emails (blob_id,received_at,mailbox_ids,keywords)
                 VALUES (?1,'2020-01-01T00:00:00Z','[1]','[]')",
                rusqlite::params![blob],
            )
            .unwrap();
        }
    }

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body(&base))
        .create();

    let _mq1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
            {"accountId":"w","ids":["t1"]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _mq2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
            {"accountId":"w","ids":[]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _mg = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/get".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/get",{"accountId":"w","list":[
            {"id":"t1","name":"Inbox","role":"inbox","parentId":null,
             "myRights":{"mayDelete":true}}],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _eq = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/query".into()))
        .with_body(
            json!({"methodResponses":[["Email/query",
            {"accountId":"w","ids":[]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let up1 = server
        .mock("POST", Matcher::Regex("/jmap/upload/".into()))
        .with_body(json!({"blobId":"UP1"}).to_string())
        .expect(1)
        .create();
    let up2 = server
        .mock("POST", Matcher::Regex("/jmap/upload/".into()))
        .with_body(json!({"blobId":"UP2"}).to_string())
        .expect(1)
        .create();

    let imp_e1 = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("Email/import".into()),
            Matcher::Regex("e1".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["Email/import",{"accountId":"w",
                "created":{"e1":{"id":"x1","blobId":"UP1","threadId":"t","size":10}}},"i"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let imp_e2_stale = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("Email/import".into()),
            Matcher::Regex("e2".into()),
            Matcher::Regex("UP1".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["Email/import",{"accountId":"w",
                "notCreated":{"e2":{"type":"blobNotFound"}}},"i"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let imp_e2_fresh = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("Email/import".into()),
            Matcher::Regex("e2".into()),
            Matcher::Regex("UP2".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["Email/import",{"accountId":"w",
                "created":{"e2":{"id":"x2","blobId":"UP2","threadId":"t","size":10}}},"i"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let summary = sync::export::run(
        CommonConfig {
            archive: archive.clone(),
            threads: 1,
            dry_run: false,
            max_retries: 1,
            allow_invalid_certs: false,
            logger: Logger::from_flags(true, 0),
        },
        ExportConfig {
            connect: ConnectConfig {
                url: base.clone(),
                auth: Auth::Basic {
                    user: "u".into(),
                    password: "p".into(),
                },
                account: AccountSelector::Id("w".into()),
            },
            objects: None,
            prune: false,
            yes: true,
        },
    )
    .expect("export run");

    let email = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "Email")
        .map(|(_, c)| c.clone())
        .expect("email counts");
    assert_eq!(email.created, 2, "both emails end up created");
    assert_eq!(email.failed, 0, "blobNotFound self-heals, not a failure");
    assert_eq!(email.skipped, 0);
    assert!(!summary.any_failed());

    up1.assert();
    up2.assert();
    imp_e1.assert();
    imp_e2_stale.assert();
    imp_e2_fresh.assert();
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn export_email_parallel_imports_each_email() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";

    let archive = tmp();
    {
        let conn = db::init::open(&archive).unwrap();
        conn.execute(
            "INSERT INTO mailboxes (id,name,parent_id,role) VALUES (1,'Inbox',NULL,'inbox')",
            [],
        )
        .unwrap();
        for n in 1..=6 {
            let raw =
                format!("From: a@x\r\nSubject: p{n}\r\nMessage-ID: <par-{n}@h>\r\n\r\nbody {n}");
            let blob = db::blobs::intern_blob(&conn, raw.as_bytes()).unwrap();
            conn.execute(
                "INSERT INTO emails (blob_id,received_at,mailbox_ids,keywords)
                 VALUES (?1,'2020-01-01T00:00:00Z','[1]','[]')",
                rusqlite::params![blob],
            )
            .unwrap();
        }
    }

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body(&base))
        .create();

    let _anchor = anchor_terminator(&mut server, api, "Mailbox");
    let _mq = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
                {"accountId":"w","ids":["t1"]},"q"]]})
            .to_string(),
        )
        .expect_at_least(1)
        .create();
    let _mg = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/get".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/get",{"accountId":"w","list":[
                {"id":"t1","name":"Inbox","role":"inbox","parentId":null,
                 "myRights":{"mayDelete":true}}],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect_at_least(1)
        .create();
    let _eq = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/query".into()))
        .with_body(
            json!({"methodResponses":[["Email/query",
                {"accountId":"w","ids":[]},"q"]]})
            .to_string(),
        )
        .expect_at_least(1)
        .create();

    let ups = server
        .mock("POST", Matcher::Regex("/jmap/upload/".into()))
        .with_body(json!({"blobId":"UP"}).to_string())
        .expect(6)
        .create();
    let imports = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/import".into()))
        .with_body_from_request(|req| {
            let v: Value = serde_json::from_slice(req.body().unwrap()).unwrap();
            let cid = v["methodCalls"][0][1]["emails"]
                .as_object()
                .unwrap()
                .keys()
                .next()
                .unwrap()
                .clone();
            let mut created = serde_json::Map::new();
            created.insert(
                cid.clone(),
                json!({"id": format!("x-{cid}"), "blobId": "b", "threadId": "t", "size": 10}),
            );
            json!({"methodResponses":[["Email/import",
                {"accountId":"w","created": created},"i"]]})
            .to_string()
            .into_bytes()
        })
        .expect(6)
        .create();

    let summary = sync::export::run(
        CommonConfig {
            threads: 4,
            ..common(&archive)
        },
        ExportConfig {
            connect: ConnectConfig {
                url: base.clone(),
                auth: Auth::Basic {
                    user: "u".into(),
                    password: "p".into(),
                },
                account: AccountSelector::Id("w".into()),
            },
            objects: None,
            prune: false,
            yes: true,
        },
    )
    .expect("export run");

    let email = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "Email")
        .map(|(_, c)| c.clone())
        .expect("email counts");
    assert_eq!(email.created, 6, "every email imported");
    assert_eq!(email.failed, 0);
    assert_eq!(email.skipped, 0);
    assert!(!summary.any_failed());

    ups.assert();
    imports.assert();
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn export_email_parallel_blob_not_found_self_heals() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";

    let archive = tmp();
    {
        let conn = db::init::open(&archive).unwrap();
        conn.execute(
            "INSERT INTO mailboxes (id,name,parent_id,role) VALUES (1,'Inbox',NULL,'inbox')",
            [],
        )
        .unwrap();
        let raw = b"From: a@x\r\nSubject: heal\r\nMessage-ID: <heal-1@h>\r\n\r\nbody";
        let blob = db::blobs::intern_blob(&conn, raw).unwrap();
        conn.execute(
            "INSERT INTO emails (blob_id,received_at,mailbox_ids,keywords)
             VALUES (?1,'2020-01-01T00:00:00Z','[1]','[]')",
            rusqlite::params![blob],
        )
        .unwrap();
    }

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body(&base))
        .create();

    let _anchor = anchor_terminator(&mut server, api, "Mailbox");
    let _mq = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
            {"accountId":"w","ids":["t1"]},"q"]]})
            .to_string(),
        )
        .expect_at_least(1)
        .create();
    let _mg = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/get".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/get",{"accountId":"w","list":[
            {"id":"t1","name":"Inbox","role":"inbox","parentId":null,
             "myRights":{"mayDelete":true}}],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect_at_least(1)
        .create();
    let _eq = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/query".into()))
        .with_body(
            json!({"methodResponses":[["Email/query",
            {"accountId":"w","ids":[]},"q"]]})
            .to_string(),
        )
        .expect_at_least(1)
        .create();

    let up1 = server
        .mock("POST", Matcher::Regex("/jmap/upload/".into()))
        .with_body(json!({"blobId":"UP1"}).to_string())
        .expect(1)
        .create();
    let up2 = server
        .mock("POST", Matcher::Regex("/jmap/upload/".into()))
        .with_body(json!({"blobId":"UP2"}).to_string())
        .expect(1)
        .create();

    let imp_stale = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("Email/import".into()),
            Matcher::Regex("UP1".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["Email/import",{"accountId":"w",
                "notCreated":{"e1":{"type":"blobNotFound"}}},"i"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let imp_fresh = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("Email/import".into()),
            Matcher::Regex("UP2".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["Email/import",{"accountId":"w",
                "created":{"e1":{"id":"x1","blobId":"UP2","threadId":"t","size":10}}},"i"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let summary = sync::export::run(
        CommonConfig {
            threads: 4,
            ..common(&archive)
        },
        ExportConfig {
            connect: ConnectConfig {
                url: base.clone(),
                auth: Auth::Basic {
                    user: "u".into(),
                    password: "p".into(),
                },
                account: AccountSelector::Id("w".into()),
            },
            objects: None,
            prune: false,
            yes: true,
        },
    )
    .expect("export run");

    let email = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "Email")
        .map(|(_, c)| c.clone())
        .expect("email counts");
    assert_eq!(email.created, 1, "email created after blob re-upload");
    assert_eq!(email.failed, 0, "blobNotFound self-heals in parallel mode");
    assert!(!summary.any_failed());

    up1.assert();
    up2.assert();
    imp_stale.assert();
    imp_fresh.assert();
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn export_contact_cards_parallel_creates_each() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";

    let archive = tmp();
    {
        let conn = db::init::open(&archive).unwrap();
        conn.execute(
            "INSERT INTO address_books (id,name,is_default) VALUES (1,'Default',0)",
            [],
        )
        .unwrap();
        for n in 1..=3 {
            conn.execute(
                "INSERT INTO contact_cards (id,uid,address_book_ids,data)
                 VALUES (?1,?2,'[1]',?3)",
                rusqlite::params![
                    n,
                    format!("card-{n}"),
                    json!({"name": {"full": format!("Person {n}")}}).to_string()
                ],
            )
            .unwrap();
        }
    }

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .create();

    let _abg = server
        .mock("POST", api)
        .match_body(Matcher::Regex("AddressBook/get".into()))
        .with_body(
            json!({"methodResponses":[["AddressBook/get",
                {"accountId":"w","list":[],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _abs = server
        .mock("POST", api)
        .match_body(Matcher::Regex("AddressBook/set".into()))
        .with_body(
            json!({"methodResponses":[["AddressBook/set",
                {"accountId":"w","created":{"c1":{"id":"ab1"}}},"0"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _ccq = server
        .mock("POST", api)
        .match_body(Matcher::Regex("ContactCard/query".into()))
        .with_body(
            json!({"methodResponses":[["ContactCard/query",
                {"accountId":"w","ids":[]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let sets = server
        .mock("POST", api)
        .match_body(Matcher::Regex("ContactCard/set".into()))
        .with_body_from_request(|req| {
            let v: Value = serde_json::from_slice(req.body().unwrap()).unwrap();
            let cid = v["methodCalls"][0][1]["create"]
                .as_object()
                .unwrap()
                .keys()
                .next()
                .unwrap()
                .clone();
            let mut created = serde_json::Map::new();
            created.insert(cid.clone(), json!({"id": format!("t-{cid}")}));
            json!({"methodResponses":[["ContactCard/set",
                {"accountId":"w","created": created},"0"]]})
            .to_string()
            .into_bytes()
        })
        .expect(3)
        .create();

    let summary = sync::export::run(
        CommonConfig {
            threads: 4,
            ..common(&archive)
        },
        export_cfg_objects(
            &base,
            vec![ObjectType::AddressBook, ObjectType::ContactCard],
        ),
    )
    .expect("export run");

    let cards = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "ContactCard")
        .map(|(_, c)| c.clone())
        .expect("contact card counts");
    assert_eq!(cards.created, 3, "every card created");
    assert_eq!(cards.failed, 0);
    assert!(!summary.any_failed());

    sets.assert();
    let _ = std::fs::remove_file(&archive);
}

fn session_body_full(base: &str) -> String {
    json!({
        "apiUrl": format!("{base}/jmap/api"),
        "uploadUrl": format!("{base}/jmap/upload/{{accountId}}/"),
        "downloadUrl": format!("{base}/jmap/dl/{{accountId}}/{{blobId}}/{{type}}/{{name}}"),
        "capabilities": { "urn:ietf:params:jmap:core": {
            "maxObjectsInGet": 500, "maxObjectsInSet": 500, "maxCallsInRequest": 16,
            "maxConcurrentRequests": 4, "maxConcurrentUpload": 4,
            "maxSizeRequest": 10000000, "maxSizeUpload": 50000000
        } },
        "accounts": { "w": { "name": "alice",
            "accountCapabilities": {
                "urn:ietf:params:jmap:mail": {},
                "urn:ietf:params:jmap:sieve": {},
                "urn:ietf:params:jmap:contacts": {},
                "urn:ietf:params:jmap:calendars": {},
                "urn:ietf:params:jmap:filenode": {}
            } } }
    })
    .to_string()
}

fn import_cfg_objects(base: &str, objects: Vec<ObjectType>) -> ImportConfig {
    ImportConfig {
        connect: ConnectConfig {
            url: base.to_owned(),
            auth: Auth::Basic {
                user: "u".into(),
                password: "p".into(),
            },
            account: AccountSelector::Id("w".into()),
        },
        objects: Some(objects),
        allow_source_change: false,
    }
}

fn export_cfg_objects(base: &str, objects: Vec<ObjectType>) -> ExportConfig {
    ExportConfig {
        connect: ConnectConfig {
            url: base.to_owned(),
            auth: Auth::Basic {
                user: "u".into(),
                password: "p".into(),
            },
            account: AccountSelector::Id("w".into()),
        },
        objects: Some(objects),
        prune: false,
        yes: true,
    }
}

fn common(archive: &Path) -> CommonConfig {
    CommonConfig {
        archive: archive.to_path_buf(),
        threads: 1,
        dry_run: false,
        max_retries: 1,
        allow_invalid_certs: false,
        logger: Logger::from_flags(true, 0),
    }
}

fn common_dry(archive: &Path) -> CommonConfig {
    CommonConfig {
        dry_run: true,
        ..common(archive)
    }
}

fn anchor_terminator(server: &mut mockito::Server, api: &str, type_name: &str) -> mockito::Mock {
    server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex(format!("{type_name}/query")),
            Matcher::Regex("\"anchor\"".into()),
        ]))
        .with_body(
            json!({"methodResponses":[[format!("{type_name}/query"),
                {"accountId":"w","ids":[]},"q"]]})
            .to_string(),
        )
        .expect_at_most(1024)
        .create()
}

#[test]
fn import_removes_vanished_mailbox_from_archive_on_second_pass() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .expect_at_least(1)
        .create();
    let _term = anchor_terminator(&mut server, api, "Mailbox");

    let _q1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
                {"accountId":"w","ids":["A","B","C"]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _g1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/get".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/get",{"accountId":"w","state":"s1","list":[
                {"id":"A","name":"alpha","parentId":null,"role":null,"sortOrder":0,"isSubscribed":true},
                {"id":"B","name":"bravo","parentId":null,"role":null,"sortOrder":0,"isSubscribed":true},
                {"id":"C","name":"charlie","parentId":null,"role":null,"sortOrder":0,"isSubscribed":true}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let s1 = sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::Mailbox]),
    )
    .expect("first import");
    let mb1 = s1
        .per_type
        .iter()
        .find(|(t, _)| *t == "Mailbox")
        .map(|(_, c)| c.clone())
        .expect("mailbox counts");
    assert_eq!(mb1.fetched, 3, "first pass fetched all three mailboxes");
    {
        let conn = rusqlite::Connection::open(&archive).unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM mailboxes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 3);
    }

    let _q2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
                {"accountId":"w","ids":["A","C"]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _ch2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/changes".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/changes",{"accountId":"w","oldState":"s1",
                "newState":"s2","hasMoreChanges":false,"created":[],"updated":[],"destroyed":[]},"c"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let s2 = sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::Mailbox]),
    )
    .expect("second import");
    let mb2 = s2
        .per_type
        .iter()
        .find(|(t, _)| *t == "Mailbox")
        .map(|(_, c)| c.clone())
        .expect("mailbox counts");
    assert_eq!(mb2.fetched, 0, "second pass fetches nothing");
    assert_eq!(mb2.updated, 0, "no changed mailboxes reported");
    assert_eq!(mb2.deleted, 1, "vanished mailbox B is deleted");
    {
        let conn = rusqlite::Connection::open(&archive).unwrap();
        let names: Vec<String> = conn
            .prepare("SELECT name FROM mailboxes ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(names, vec!["alpha".to_owned(), "charlie".to_owned()]);
    }
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn import_present_item_change_is_propagated_via_changes() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .expect_at_least(1)
        .create();
    let _term = anchor_terminator(&mut server, api, "Mailbox");

    let _q1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
                {"accountId":"w","ids":["A"]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _g1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/get".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/get",{"accountId":"w","state":"s1","list":[
                {"id":"A","name":"OriginalName","parentId":null,"role":null,"sortOrder":0,"isSubscribed":true}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::Mailbox]),
    )
    .expect("first import");
    {
        let conn = rusqlite::Connection::open(&archive).unwrap();
        let cursor: String = conn
            .query_row(
                "SELECT state FROM sync_state_jmap WHERE type_name='Mailbox'",
                [],
                |r| r.get(0),
            )
            .expect("first import records the state cursor");
        assert_eq!(cursor, "s1");
    }

    let _q2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
                {"accountId":"w","ids":["A"]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let changes = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("Mailbox/changes".into()),
            Matcher::Regex("\"sinceState\":\"s1\"".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["Mailbox/changes",{"accountId":"w","oldState":"s1",
                "newState":"s2","hasMoreChanges":false,"created":[],"updated":["A"],"destroyed":[]},"c"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _g2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/get".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/get",{"accountId":"w","state":"s2","list":[
                {"id":"A","name":"UpdatedName","parentId":null,"role":null,"sortOrder":0,"isSubscribed":true}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let s2 = sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::Mailbox]),
    )
    .expect("second import");
    changes.assert();
    let mb2 = s2
        .per_type
        .iter()
        .find(|(t, _)| *t == "Mailbox")
        .map(|(_, c)| c.clone())
        .expect("mailbox counts");
    assert_eq!(mb2.fetched, 0, "no new objects on the second pass");
    assert_eq!(
        mb2.updated, 1,
        "the changed mailbox is detected via /changes and refreshed in place"
    );
    let conn = rusqlite::Connection::open(&archive).unwrap();
    let name: String = conn
        .query_row("SELECT name FROM mailboxes WHERE id=1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        name, "UpdatedName",
        "a server-side property change is propagated into the archive"
    );
    let cursor: String = conn
        .query_row(
            "SELECT state FROM sync_state_jmap WHERE type_name='Mailbox'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cursor, "s2", "cursor advances to the changes newState");
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn import_removes_vanished_email_and_drops_cross_ref() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .expect_at_least(1)
        .create();
    let _mbterm = anchor_terminator(&mut server, api, "Mailbox");
    let _emterm = anchor_terminator(&mut server, api, "Email");

    let _mbq1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
                {"accountId":"w","ids":["MX"]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _mbg1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/get".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/get",{"accountId":"w","state":"sm1","list":[
                {"id":"MX","name":"Inbox","parentId":null,"role":"inbox","sortOrder":0,"isSubscribed":true}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _eq1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/query".into()))
        .with_body(
            json!({"methodResponses":[["Email/query",
                {"accountId":"w","ids":["E1","E2"]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _eg1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/get".into()))
        .with_body(
            json!({"methodResponses":[["Email/get",{"accountId":"w","state":"se1","list":[
                {"id":"E1","blobId":"BLB1","receivedAt":"2020-01-01T00:00:00Z","mailboxIds":{"MX":true},"keywords":{"$seen":true}},
                {"id":"E2","blobId":"BLB2","receivedAt":"2020-01-02T00:00:00Z","mailboxIds":{"MX":true},"keywords":{}}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _dl1 = server
        .mock("GET", Matcher::Regex("/jmap/dl/w/BLB1/.*".into()))
        .with_body("From: a@x\r\nMessage-ID: <1@h>\r\n\r\nbody-one")
        .expect(1)
        .create();
    let _dl2 = server
        .mock("GET", Matcher::Regex("/jmap/dl/w/BLB2/.*".into()))
        .with_body("From: b@x\r\nMessage-ID: <2@h>\r\n\r\nbody-two")
        .expect(1)
        .create();

    sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::Mailbox, ObjectType::Email]),
    )
    .expect("first import");
    {
        let conn = rusqlite::Connection::open(&archive).unwrap();
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT count(*) FROM emails", [], |r| r.get(0))
                .unwrap(),
            2
        );
    }

    let _mbq2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
                {"accountId":"w","ids":["MX"]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _eq2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/query".into()))
        .with_body(
            json!({"methodResponses":[["Email/query",
                {"accountId":"w","ids":["E2"]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _mbch2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/changes".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/changes",{"accountId":"w","oldState":"sm1",
                "newState":"sm2","hasMoreChanges":false,"created":[],"updated":[],"destroyed":[]},"c"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _ech2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/changes".into()))
        .with_body(
            json!({"methodResponses":[["Email/changes",{"accountId":"w","oldState":"se1",
                "newState":"se2","hasMoreChanges":false,"created":[],"updated":[],"destroyed":[]},"c"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let s2 = sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::Mailbox, ObjectType::Email]),
    )
    .expect("second import");
    let em2 = s2
        .per_type
        .iter()
        .find(|(t, _)| *t == "Email")
        .map(|(_, c)| c.clone())
        .expect("email counts");
    assert_eq!(em2.deleted, 1, "vanished email is deleted from archive");
    let conn = rusqlite::Connection::open(&archive).unwrap();
    let remaining: i64 = conn
        .query_row("SELECT count(*) FROM emails", [], |r| r.get(0))
        .unwrap();
    assert_eq!(remaining, 1, "only the still-present email remains");
    let blobs: i64 = conn
        .query_row("SELECT count(*) FROM blobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(blobs, 1, "blob GC reclaims orphan blob of deleted email");
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn import_missing_email_blob_is_skipped_and_counted_once() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .expect_at_least(1)
        .create();
    let _mbterm = anchor_terminator(&mut server, api, "Mailbox");
    let _emterm = anchor_terminator(&mut server, api, "Email");

    let _mbq = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
                {"accountId":"w","ids":["MX"]},"q"]]})
            .to_string(),
        )
        .create();
    let _mbg = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/get".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/get",{"accountId":"w","state":"sm1","list":[
                {"id":"MX","name":"Inbox","parentId":null,"role":"inbox","sortOrder":0,"isSubscribed":true}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .create();
    let _eq = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/query".into()))
        .with_body(
            json!({"methodResponses":[["Email/query",
                {"accountId":"w","ids":["E1","E2"]},"q"]]})
            .to_string(),
        )
        .create();
    let _eg = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/get".into()))
        .with_body(
            json!({"methodResponses":[["Email/get",{"accountId":"w","state":"se1","list":[
                {"id":"E1","blobId":"BLB1","receivedAt":"2020-01-01T00:00:00Z","mailboxIds":{"MX":true},"keywords":{"$seen":true}},
                {"id":"E2","blobId":"BLB2","receivedAt":"2020-01-02T00:00:00Z","mailboxIds":{"MX":true},"keywords":{}}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .create();
    let _dl1 = server
        .mock("GET", Matcher::Regex("/jmap/dl/w/BLB1/.*".into()))
        .with_body("From: a@x\r\nMessage-ID: <1@h>\r\n\r\nbody-one")
        .create();
    let _dl2 = server
        .mock("GET", Matcher::Regex("/jmap/dl/w/BLB2/.*".into()))
        .with_status(404)
        .with_body(json!({"status":404,"title":"Not Found"}).to_string())
        .create();

    let summary = sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::Mailbox, ObjectType::Email]),
    )
    .expect("import does not abort on a missing blob");

    let email = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "Email")
        .map(|(_, c)| c.clone())
        .expect("email counts");
    assert_eq!(
        email.fetched, 1,
        "only the email with a present blob imports"
    );
    assert_eq!(
        email.failed, 1,
        "a missing blob counts the email failed exactly once, not twice"
    );
    assert!(summary.any_failed());

    let conn = rusqlite::Connection::open(&archive).unwrap();
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM emails", [], |r| r.get(0))
            .unwrap(),
        1,
        "the skipped email leaves no row"
    );
    assert_eq!(
        conn.query_row::<i64, _, _>(
            "SELECT count(*) FROM sync_id_jmap WHERE type_name='Email'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        1,
        "no id mapping is recorded for the skipped email, so a re-run retries it"
    );
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn export_missing_target_email_is_created_on_rerun() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();
    {
        let conn = db::init::open(&archive).unwrap();
        conn.execute(
            "INSERT INTO mailboxes (id,name,parent_id,role) VALUES (1,'Inbox',NULL,'inbox')",
            [],
        )
        .unwrap();
        for n in 1..=2 {
            let raw =
                format!("From: a@x\r\nSubject: m{n}\r\nMessage-ID: <m-{n}@h>\r\n\r\nbody {n}",);
            let blob = db::blobs::intern_blob(&conn, raw.as_bytes()).unwrap();
            let mm = vandelay::sync::keys::index_to_json(
                &vandelay::sync::emailmeta::email_index_from_blob(raw.as_bytes()),
            );
            conn.execute(
                "INSERT INTO emails (blob_id,received_at,mailbox_ids,keywords,message_match)
                 VALUES (?1,'2020-01-01T00:00:00Z','[1]','[]', ?2)",
                rusqlite::params![blob, mm],
            )
            .unwrap();
        }
    }

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .expect_at_least(1)
        .create();
    let _mbterm = anchor_terminator(&mut server, api, "Mailbox");
    let _emterm = anchor_terminator(&mut server, api, "Email");

    let _mq = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
                {"accountId":"w","ids":["T1"]},"q"]]})
            .to_string(),
        )
        .expect_at_least(1)
        .create();
    let _mg = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/get".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/get",{"accountId":"w","list":[
                {"id":"T1","name":"Inbox","role":"inbox","parentId":null,"myRights":{"mayDelete":true}}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect_at_least(1)
        .create();

    let _eq = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/query".into()))
        .with_body(
            json!({"methodResponses":[["Email/query",
                {"accountId":"w","ids":["X1"]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _eg = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/get".into()))
        .with_body(
            json!({"methodResponses":[["Email/get",{"accountId":"w","list":[
                {"id":"X1","messageId":["m-1@h"]}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let upload = server
        .mock("POST", Matcher::Regex("/jmap/upload/".into()))
        .with_body(json!({"blobId":"BUP"}).to_string())
        .expect(1)
        .create();
    let create = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/import".into()))
        .with_body(
            json!({"methodResponses":[["Email/import",{"accountId":"w",
                "created":{"e2":{"id":"Y2","blobId":"BUP","threadId":"t","size":10}}},"i"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let summary = sync::export::run(
        common(&archive),
        export_cfg_objects(&base, vec![ObjectType::Mailbox, ObjectType::Email]),
    )
    .expect("export");
    let email = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "Email")
        .map(|(_, c)| c.clone())
        .expect("email counts");
    assert_eq!(email.skipped, 1, "Message-ID match with X1 skips it");
    assert_eq!(email.created, 1, "missing email is created");
    assert_eq!(email.failed, 0);
    upload.assert();
    create.assert();
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn export_email_blake3_fallback_matches_when_no_message_id() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();
    let raw = "From: a@x\r\nSubject: hello\r\n\r\nno-msg-id-body";
    {
        let conn = db::init::open(&archive).unwrap();
        conn.execute(
            "INSERT INTO mailboxes (id,name,parent_id,role) VALUES (1,'Inbox',NULL,'inbox')",
            [],
        )
        .unwrap();
        let blob = db::blobs::intern_blob(&conn, raw.as_bytes()).unwrap();
        let mm = vandelay::sync::keys::index_to_json(
            &vandelay::sync::emailmeta::email_index_from_blob(raw.as_bytes()),
        );
        conn.execute(
            "INSERT INTO emails (blob_id,received_at,mailbox_ids,keywords,message_match)
             VALUES (?1,'2020-01-01T00:00:00Z','[1]','[]', ?2)",
            rusqlite::params![blob, mm],
        )
        .unwrap();
    }

    let local_idx = vandelay::sync::emailmeta::email_index_from_blob(raw.as_bytes());
    assert!(local_idx.mids.is_empty(), "blob must lack Message-ID");

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .expect_at_least(1)
        .create();
    let _mbterm = anchor_terminator(&mut server, api, "Mailbox");
    let _emterm = anchor_terminator(&mut server, api, "Email");
    let _mq = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
                {"accountId":"w","ids":["T1"]},"q"]]})
            .to_string(),
        )
        .expect_at_least(1)
        .create();
    let _mg = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/get".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/get",{"accountId":"w","list":[
                {"id":"T1","name":"Inbox","role":"inbox","parentId":null,"myRights":{"mayDelete":true}}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect_at_least(1)
        .create();
    let _eq = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/query".into()))
        .with_body(
            json!({"methodResponses":[["Email/query",
                {"accountId":"w","ids":["X1"]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _eg_min = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("Email/get".into()),
            Matcher::Regex("messageId".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["Email/get",{"accountId":"w","list":[
                {"id":"X1","messageId":[]}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _eg_full = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("Email/get".into()),
            Matcher::Regex("from".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["Email/get",{"accountId":"w","list":[
                {"id":"X1","messageId":[],"from":[{"email":"a@x"}],"subject":"hello","sentAt":"","to":[]}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let no_upload = server
        .mock("POST", Matcher::Regex("/jmap/upload/".into()))
        .expect(0)
        .create();
    let no_import = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/import".into()))
        .expect(0)
        .create();

    let summary = sync::export::run(
        common(&archive),
        export_cfg_objects(&base, vec![ObjectType::Mailbox, ObjectType::Email]),
    )
    .expect("export");
    no_upload.assert();
    no_import.assert();
    let email = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "Email")
        .map(|(_, c)| c.clone())
        .expect("email counts");
    assert_eq!(email.skipped, 1, "BLAKE3 fallback matched target");
    assert_eq!(email.created, 0);
    assert_eq!(email.failed, 0);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn export_address_book_creates_only_missing() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();
    {
        let conn = db::init::open(&archive).unwrap();
        conn.execute(
            "INSERT INTO address_books (id,name,description,is_default)
             VALUES (1,'Personal',NULL,1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO address_books (id,name,description,is_default)
             VALUES (2,'Work',NULL,0)",
            [],
        )
        .unwrap();
    }

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .expect_at_least(1)
        .create();

    let _g = server
        .mock("POST", api)
        .match_body(Matcher::Regex("AddressBook/get".into()))
        .with_body(
            json!({"methodResponses":[["AddressBook/get",{"accountId":"w","list":[
                {"id":"P","name":"personal","isDefault":true,"myRights":{"mayDelete":false}}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let create = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("AddressBook/set".into()),
            Matcher::Regex("Work".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["AddressBook/set",{"accountId":"w",
                "created":{"c2":{"id":"WID"}}},"s"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let summary = sync::export::run(
        common(&archive),
        export_cfg_objects(&base, vec![ObjectType::AddressBook]),
    )
    .expect("export");
    create.assert();
    let counts = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "AddressBook")
        .map(|(_, c)| c.clone())
        .expect("address book counts");
    assert_eq!(counts.skipped, 1, "Personal matches existing (case-fold)");
    assert_eq!(counts.created, 1, "Work is created");
    assert_eq!(counts.failed, 0);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn export_calendar_creates_only_missing() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();
    {
        let conn = db::init::open(&archive).unwrap();
        conn.execute(
            "INSERT INTO calendars (id,name,is_default) VALUES (1,'Family',1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO calendars (id,name,is_default) VALUES (2,'Team',0)",
            [],
        )
        .unwrap();
    }

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .expect_at_least(1)
        .create();
    let _g = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Calendar/get".into()))
        .with_body(
            json!({"methodResponses":[["Calendar/get",{"accountId":"w","list":[
                {"id":"F","name":"family","isDefault":true,"myRights":{"mayDelete":false}}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let create = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("Calendar/set".into()),
            Matcher::Regex("Team".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["Calendar/set",{"accountId":"w",
                "created":{"c2":{"id":"TID"}}},"s"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let summary = sync::export::run(
        common(&archive),
        export_cfg_objects(&base, vec![ObjectType::Calendar]),
    )
    .expect("export");
    create.assert();
    let counts = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "Calendar")
        .map(|(_, c)| c.clone())
        .expect("calendar counts");
    assert_eq!(counts.skipped, 1, "Family matches existing (case-fold)");
    assert_eq!(counts.created, 1, "Team is created");
    assert_eq!(counts.failed, 0);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn export_sieve_script_matches_by_name_not_content() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();
    let keepall_local = b"require [\"fileinto\"];\nkeep;\n";
    let reject_local = b"require [\"reject\"];\nreject \"go away\";\n";
    {
        let conn = db::init::open(&archive).unwrap();
        let blob1 = db::blobs::intern_blob(&conn, keepall_local).unwrap();
        let blob2 = db::blobs::intern_blob(&conn, reject_local).unwrap();
        conn.execute(
            "INSERT INTO sieve_scripts (id,name,is_active,blob_id) VALUES (1,'keepall',1,?1)",
            rusqlite::params![blob1],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sieve_scripts (id,name,is_active,blob_id) VALUES (2,'reject',0,?1)",
            rusqlite::params![blob2],
        )
        .unwrap();
    }

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .expect_at_least(1)
        .create();
    let _g = server
        .mock("POST", api)
        .match_body(Matcher::Regex("SieveScript/get".into()))
        .with_body(
            json!({"methodResponses":[["SieveScript/get",{"accountId":"w","list":[
                {"id":"S1","name":"keepall","isActive":false,"blobId":"BSRV"}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let no_download = server
        .mock("GET", Matcher::Regex("/jmap/dl/w/BSRV/.*".into()))
        .with_body(b"unused".as_slice())
        .expect(0)
        .create();
    let upload = server
        .mock("POST", Matcher::Regex("/jmap/upload/".into()))
        .with_body(json!({"blobId":"UPN"}).to_string())
        .expect(1)
        .create();
    let create = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("SieveScript/set".into()),
            Matcher::Regex("reject".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["SieveScript/set",{"accountId":"w",
                "created":{"c2":{"id":"S2"}}},"s"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _activate = server
        .mock("POST", api)
        .match_body(Matcher::Regex("onSuccessActivateScript".into()))
        .with_body(
            json!({"methodResponses":[["SieveScript/set",{"accountId":"w"},"a"]]}).to_string(),
        )
        .expect(1)
        .create();

    let summary = sync::export::run(
        common(&archive),
        export_cfg_objects(&base, vec![ObjectType::SieveScript]),
    )
    .expect("export");
    upload.assert();
    create.assert();
    no_download.assert();
    let counts = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "SieveScript")
        .map(|(_, c)| c.clone())
        .expect("sieve counts");
    assert_eq!(
        counts.skipped, 1,
        "name-matched script is skipped even though its content differs from the target"
    );
    assert_eq!(counts.created, 1, "the unmatched name is created");
    assert_eq!(counts.failed, 0);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn export_sieve_scripts_identical_content_different_names_both_created() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();
    let shared = b"require [\"fileinto\"];\nfileinto \"Archive\";\n";
    {
        let conn = db::init::open(&archive).unwrap();
        let blob = db::blobs::intern_blob(&conn, shared).unwrap();
        conn.execute(
            "INSERT INTO sieve_scripts (id,name,is_active,blob_id) VALUES (1,'duplicate-A',0,?1)",
            rusqlite::params![blob],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sieve_scripts (id,name,is_active,blob_id) VALUES (2,'duplicate-B',0,?1)",
            rusqlite::params![blob],
        )
        .unwrap();
        let dup_count: i64 = conn
            .query_row(
                "SELECT count(DISTINCT blob_id) FROM sieve_scripts",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(dup_count, 1, "both scripts share one blob (byte-identical)");
    }

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .expect_at_least(1)
        .create();
    let _g = server
        .mock("POST", api)
        .match_body(Matcher::Regex("SieveScript/get".into()))
        .with_body(
            json!({"methodResponses":[["SieveScript/get",
                {"accountId":"w","list":[],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let upload = server
        .mock("POST", Matcher::Regex("/jmap/upload/".into()))
        .with_body(json!({"blobId":"UPN"}).to_string())
        .expect(1)
        .create();
    let create_a = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("SieveScript/set".into()),
            Matcher::Regex("duplicate-A".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["SieveScript/set",{"accountId":"w",
                "created":{"c1":{"id":"S1"}}},"s"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let create_b = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("SieveScript/set".into()),
            Matcher::Regex("duplicate-B".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["SieveScript/set",{"accountId":"w",
                "created":{"c2":{"id":"S2"}}},"s"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _deactivate = server
        .mock("POST", api)
        .match_body(Matcher::Regex("onSuccessDeactivateScript".into()))
        .with_body(
            json!({"methodResponses":[["SieveScript/set",{"accountId":"w"},"a"]]}).to_string(),
        )
        .expect(1)
        .create();

    let summary = sync::export::run(
        common(&archive),
        export_cfg_objects(&base, vec![ObjectType::SieveScript]),
    )
    .expect("export");
    upload.assert();
    create_a.assert();
    create_b.assert();
    let counts = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "SieveScript")
        .map(|(_, c)| c.clone())
        .expect("sieve counts");
    assert_eq!(
        counts.created, 2,
        "two scripts with identical content but distinct names must both reach the target"
    );
    assert_eq!(
        counts.skipped, 0,
        "neither distinct name collapses onto the other"
    );
    assert_eq!(counts.failed, 0);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn export_sieve_blob_not_found_on_dedup_reuse_reuploads_and_retries() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();
    let shared = b"require [\"fileinto\"];\nfileinto \"Archive\";\n";
    {
        let conn = db::init::open(&archive).unwrap();
        let blob = db::blobs::intern_blob(&conn, shared).unwrap();
        conn.execute(
            "INSERT INTO sieve_scripts (id,name,is_active,blob_id) VALUES (1,'dup-A',0,?1)",
            rusqlite::params![blob],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sieve_scripts (id,name,is_active,blob_id) VALUES (2,'dup-B',0,?1)",
            rusqlite::params![blob],
        )
        .unwrap();
    }

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .expect_at_least(1)
        .create();
    let _g = server
        .mock("POST", api)
        .match_body(Matcher::Regex("SieveScript/get".into()))
        .with_body(
            json!({"methodResponses":[["SieveScript/get",
                {"accountId":"w","list":[],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let up1 = server
        .mock("POST", Matcher::Regex("/jmap/upload/".into()))
        .with_body(json!({"blobId":"UP1"}).to_string())
        .expect(1)
        .create();
    let up2 = server
        .mock("POST", Matcher::Regex("/jmap/upload/".into()))
        .with_body(json!({"blobId":"UP2"}).to_string())
        .expect(1)
        .create();
    let create_a = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("SieveScript/set".into()),
            Matcher::Regex("dup-A".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["SieveScript/set",{"accountId":"w",
                "created":{"c1":{"id":"S1"}}},"s"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let create_b_stale = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("SieveScript/set".into()),
            Matcher::Regex("dup-B".into()),
            Matcher::Regex("UP1".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["SieveScript/set",{"accountId":"w",
                "notCreated":{"c2":{"type":"blobNotFound"}}},"s"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let create_b_fresh = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("SieveScript/set".into()),
            Matcher::Regex("dup-B".into()),
            Matcher::Regex("UP2".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["SieveScript/set",{"accountId":"w",
                "created":{"c2":{"id":"S2"}}},"s"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _deactivate = server
        .mock("POST", api)
        .match_body(Matcher::Regex("onSuccessDeactivateScript".into()))
        .with_body(
            json!({"methodResponses":[["SieveScript/set",{"accountId":"w"},"a"]]}).to_string(),
        )
        .expect(1)
        .create();

    let summary = sync::export::run(
        common(&archive),
        export_cfg_objects(&base, vec![ObjectType::SieveScript]),
    )
    .expect("export");
    up1.assert();
    up2.assert();
    create_a.assert();
    create_b_stale.assert();
    create_b_fresh.assert();
    let counts = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "SieveScript")
        .map(|(_, c)| c.clone())
        .expect("sieve counts");
    assert_eq!(
        counts.created, 2,
        "the dedup-reused blobId that came back blobNotFound self-heals via re-upload"
    );
    assert_eq!(
        counts.failed, 0,
        "blobNotFound on a reused blob is not a failure"
    );
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn import_deeply_nested_mailbox_tree_orders_correctly() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();
    const DEPTH: usize = 10;

    let ids: Vec<String> = (0..DEPTH).map(|i| format!("L{i}")).collect();
    let mut servlist = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        let parent = if i == 0 {
            Value::Null
        } else {
            Value::String(ids[i - 1].clone())
        };
        servlist.push(json!({
            "id": id, "name": format!("level{i}"),
            "parentId": parent, "role": null,
            "sortOrder": 0, "isSubscribed": true
        }));
    }

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .expect_at_least(1)
        .create();
    let _mbterm = anchor_terminator(&mut server, api, "Mailbox");
    let server_ids: Vec<Value> = ids.iter().rev().map(|s| Value::String(s.clone())).collect();
    let _q = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
                {"accountId":"w","ids": server_ids},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _g = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/get".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/get",
                {"accountId":"w","list": servlist,"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let summary = sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::Mailbox]),
    )
    .expect("import");
    assert!(!summary.any_failed(), "import had failures: {summary:?}");

    let conn = rusqlite::Connection::open(&archive).unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM mailboxes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n as usize, DEPTH);
    for i in 1..DEPTH {
        let pname: String = conn
            .query_row(
                "SELECT p.name FROM mailboxes c JOIN mailboxes p ON c.parent_id = p.id
                 WHERE c.name = ?1",
                rusqlite::params![format!("level{i}")],
                |r| r.get(0),
            )
            .unwrap_or_else(|e| panic!("level{i} parent lookup failed: {e}"));
        assert_eq!(pname, format!("level{}", i - 1));
    }
    let root: Option<i64> = conn
        .query_row(
            "SELECT parent_id FROM mailboxes WHERE name='level0'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(root, None, "root has no parent");
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn export_dry_run_sends_no_mutating_calls() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";

    let archive = tmp();
    {
        let conn = db::init::open(&archive).unwrap();
        conn.execute(
            "INSERT INTO mailboxes (id,name,parent_id,role) VALUES (1,'Inbox',NULL,'inbox')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mailboxes (id,name,parent_id,role) VALUES (2,'Sent',NULL,NULL)",
            [],
        )
        .unwrap();
        let blob = db::blobs::intern_blob(
            &conn,
            b"From: a@x\r\nSubject: hi\r\nMessage-ID: <m-1@h>\r\n\r\nbody",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO emails (blob_id,received_at,mailbox_ids,keywords)
             VALUES (?1,'2020-01-01T00:00:00Z','[1]','[]')",
            rusqlite::params![blob],
        )
        .unwrap();
    }

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body(&base))
        .create();
    let _mq = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
                {"accountId":"w","ids":[]},"q"]]})
            .to_string(),
        )
        .expect_at_least(1)
        .create();
    let _eq = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/query".into()))
        .with_body(
            json!({"methodResponses":[["Email/query",
                {"accountId":"w","ids":[]},"q"]]})
            .to_string(),
        )
        .expect_at_least(1)
        .create();

    let no_set = server
        .mock("POST", api)
        .match_body(Matcher::Regex(r"/(set|import)".into()))
        .expect(0)
        .create();
    let no_upload = server
        .mock("POST", Matcher::Regex("/jmap/upload/".into()))
        .expect(0)
        .create();

    let summary = sync::export::run(
        common_dry(&archive),
        export_cfg_objects(&base, vec![ObjectType::Mailbox, ObjectType::Email]),
    )
    .expect("dry-run export must succeed");
    assert!(
        !summary.any_failed(),
        "dry-run summary should not record failures: {summary:?}"
    );

    no_set.assert();
    no_upload.assert();
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn import_dry_run_does_not_write_archive_or_download_blobs() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";

    let archive = tmp();
    {
        let _ = db::init::open(&archive).unwrap();
    }

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body(&base))
        .create();
    let _term = anchor_terminator(&mut server, api, "Mailbox");
    let _mq = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
                {"accountId":"w","ids":["s1"]},"q"]]})
            .to_string(),
        )
        .expect_at_least(1)
        .create();

    let no_set = server
        .mock("POST", api)
        .match_body(Matcher::Regex(r"/(set|import)".into()))
        .expect(0)
        .create();
    let no_get = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/get".into()))
        .expect(0)
        .create();
    let no_download = server
        .mock("GET", Matcher::Regex("/jmap/dl/".into()))
        .expect(0)
        .create();

    let summary = sync::import_jmap::run(
        common_dry(&archive),
        import_cfg_objects(&base, vec![ObjectType::Mailbox]),
    )
    .expect("dry-run import must succeed");
    assert!(
        !summary.any_failed(),
        "dry-run import should not record failures: {summary:?}"
    );

    no_set.assert();
    no_get.assert();
    no_download.assert();

    let conn = rusqlite::Connection::open(&archive).unwrap();
    let mailbox_rows: i64 = conn
        .query_row("SELECT count(*) FROM mailboxes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mailbox_rows, 0, "dry-run must not insert into the archive");
    let source_rows: i64 = conn
        .query_row("SELECT count(*) FROM sources", [], |r| r.get(0))
        .unwrap();
    assert_eq!(source_rows, 0, "dry-run must not record the JMAP source");
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn import_email_keyword_change_is_propagated_without_blob_refetch() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .expect_at_least(1)
        .create();
    let _mbterm = anchor_terminator(&mut server, api, "Mailbox");
    let _emterm = anchor_terminator(&mut server, api, "Email");

    let _mbq1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",{"accountId":"w","ids":["MX"]},"q"]]})
                .to_string(),
        )
        .expect(1)
        .create();
    let _mbg1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/get".into()))
        .with_body(json!({"methodResponses":[["Mailbox/get",{"accountId":"w","state":"sm1","list":[
            {"id":"MX","name":"Inbox","parentId":null,"role":"inbox","sortOrder":0,"isSubscribed":true}
        ],"notFound":[]},"g"]]}).to_string())
        .expect(1)
        .create();
    let _eq1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/query".into()))
        .with_body(
            json!({"methodResponses":[["Email/query",{"accountId":"w","ids":["E1"]},"q"]]})
                .to_string(),
        )
        .expect(1)
        .create();
    let _eg1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/get".into()))
        .with_body(json!({"methodResponses":[["Email/get",{"accountId":"w","state":"se1","list":[
            {"id":"E1","blobId":"BLB1","receivedAt":"2020-01-01T00:00:00Z","mailboxIds":{"MX":true},"keywords":{"$seen":true}}
        ],"notFound":[]},"g"]]}).to_string())
        .expect(1)
        .create();
    let _dl1 = server
        .mock("GET", Matcher::Regex("/jmap/dl/w/BLB1/.*".into()))
        .with_body("From: a@x\r\nMessage-ID: <1@h>\r\n\r\nbody-one")
        .expect(1)
        .create();

    sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::Mailbox, ObjectType::Email]),
    )
    .expect("first import");

    let _mbq2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",{"accountId":"w","ids":["MX"]},"q"]]})
                .to_string(),
        )
        .expect(1)
        .create();
    let _mbch2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/changes".into()))
        .with_body(json!({"methodResponses":[["Mailbox/changes",{"accountId":"w","oldState":"sm1","newState":"sm2","hasMoreChanges":false,"created":[],"updated":[],"destroyed":[]},"c"]]}).to_string())
        .expect(1)
        .create();
    let _eq2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/query".into()))
        .with_body(
            json!({"methodResponses":[["Email/query",{"accountId":"w","ids":["E1"]},"q"]]})
                .to_string(),
        )
        .expect(1)
        .create();
    let echanges = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/changes".into()))
        .with_body(json!({"methodResponses":[["Email/changes",{"accountId":"w","oldState":"se1","newState":"se2","hasMoreChanges":false,"created":[],"updated":["E1"],"destroyed":[]},"c"]]}).to_string())
        .expect(1)
        .create();
    let _eg2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/get".into()))
        .with_body(
            json!({"methodResponses":[["Email/get",{"accountId":"w","state":"se2","list":[
            {"id":"E1","mailboxIds":{"MX":true},"keywords":{"$seen":true,"$flagged":true}}
        ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let no_blob_refetch = server
        .mock("GET", Matcher::Regex("/jmap/dl/w/BLB1/.*".into()))
        .with_body("should-not-be-fetched")
        .expect(0)
        .create();

    let s2 = sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::Mailbox, ObjectType::Email]),
    )
    .expect("second import");
    echanges.assert();
    no_blob_refetch.assert();
    let em2 = s2
        .per_type
        .iter()
        .find(|(t, _)| *t == "Email")
        .map(|(_, c)| c.clone())
        .expect("email counts");
    assert_eq!(em2.updated, 1, "the changed email is refreshed");
    assert_eq!(em2.fetched, 0, "no new emails");
    let conn = rusqlite::Connection::open(&archive).unwrap();
    let kw: String = conn
        .query_row("SELECT keywords FROM emails LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert!(
        kw.contains("$seen") && kw.contains("$flagged"),
        "keyword change propagated into the archive: {kw}"
    );
    let blobs: i64 = conn
        .query_row("SELECT count(*) FROM blobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        blobs, 1,
        "the immutable body blob is not re-downloaded on update"
    );
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn import_cannot_calculate_changes_falls_back_to_full_refresh() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .expect_at_least(1)
        .create();
    let _term = anchor_terminator(&mut server, api, "Mailbox");

    let _q1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",{"accountId":"w","ids":["A"]},"q"]]})
                .to_string(),
        )
        .expect(1)
        .create();
    let _g1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/get".into()))
        .with_body(json!({"methodResponses":[["Mailbox/get",{"accountId":"w","state":"s1","list":[
            {"id":"A","name":"OriginalName","parentId":null,"role":null,"sortOrder":0,"isSubscribed":true}
        ],"notFound":[]},"g"]]}).to_string())
        .expect(1)
        .create();

    sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::Mailbox]),
    )
    .expect("first import");

    let _q2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",{"accountId":"w","ids":["A"]},"q"]]})
                .to_string(),
        )
        .expect(1)
        .create();
    let cannot = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/changes".into()))
        .with_body(
            json!({"methodResponses":[["error",{"type":"cannotCalculateChanges"},"c"]]})
                .to_string(),
        )
        .expect(1)
        .create();
    let capture_state = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("Mailbox/get".into()),
            Matcher::Regex("\"ids\":\\[\\]".into()),
        ]))
        .with_body(json!({"methodResponses":[["Mailbox/get",{"accountId":"w","state":"s9","list":[],"notFound":[]},"g"]]}).to_string())
        .expect(1)
        .create();
    let refresh_get = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("Mailbox/get".into()),
            Matcher::Regex("\"A\"".into()),
        ]))
        .with_body(json!({"methodResponses":[["Mailbox/get",{"accountId":"w","state":"s9","list":[
            {"id":"A","name":"RefreshedName","parentId":null,"role":null,"sortOrder":0,"isSubscribed":true}
        ],"notFound":[]},"g"]]}).to_string())
        .expect(1)
        .create();

    let s2 = sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::Mailbox]),
    )
    .expect("second import");
    cannot.assert();
    capture_state.assert();
    refresh_get.assert();
    let mb2 = s2
        .per_type
        .iter()
        .find(|(t, _)| *t == "Mailbox")
        .map(|(_, c)| c.clone())
        .expect("mailbox counts");
    assert_eq!(mb2.updated, 1, "fallback refreshes the present object");
    let conn = rusqlite::Connection::open(&archive).unwrap();
    let name: String = conn
        .query_row("SELECT name FROM mailboxes WHERE id=1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(name, "RefreshedName", "A-fallback propagated the change");
    let cursor: String = conn
        .query_row(
            "SELECT state FROM sync_state_jmap WHERE type_name='Mailbox'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        cursor, "s9",
        "fallback captured a fresh cursor for the next run"
    );
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn import_failed_update_holds_cursor_for_retry() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .expect_at_least(1)
        .create();
    let _mbterm = anchor_terminator(&mut server, api, "Mailbox");
    let _emterm = anchor_terminator(&mut server, api, "Email");

    let _mbq1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",{"accountId":"w","ids":["MX"]},"q"]]})
                .to_string(),
        )
        .expect(1)
        .create();
    let _mbg1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/get".into()))
        .with_body(json!({"methodResponses":[["Mailbox/get",{"accountId":"w","state":"sm1","list":[
            {"id":"MX","name":"Inbox","parentId":null,"role":"inbox","sortOrder":0,"isSubscribed":true}
        ],"notFound":[]},"g"]]}).to_string())
        .expect(1)
        .create();
    let _eq1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/query".into()))
        .with_body(
            json!({"methodResponses":[["Email/query",{"accountId":"w","ids":["E1"]},"q"]]})
                .to_string(),
        )
        .expect(1)
        .create();
    let _eg1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/get".into()))
        .with_body(json!({"methodResponses":[["Email/get",{"accountId":"w","state":"se1","list":[
            {"id":"E1","blobId":"BLB1","receivedAt":"2020-01-01T00:00:00Z","mailboxIds":{"MX":true},"keywords":{"$seen":true}}
        ],"notFound":[]},"g"]]}).to_string())
        .expect(1)
        .create();
    let _dl1 = server
        .mock("GET", Matcher::Regex("/jmap/dl/w/BLB1/.*".into()))
        .with_body("From: a@x\r\nMessage-ID: <1@h>\r\n\r\nbody-one")
        .expect(1)
        .create();

    sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::Mailbox, ObjectType::Email]),
    )
    .expect("first import");

    let _mbq2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",{"accountId":"w","ids":["MX"]},"q"]]})
                .to_string(),
        )
        .expect(1)
        .create();
    let _mbch2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/changes".into()))
        .with_body(json!({"methodResponses":[["Mailbox/changes",{"accountId":"w","oldState":"sm1","newState":"sm2","hasMoreChanges":false,"created":[],"updated":[],"destroyed":[]},"c"]]}).to_string())
        .expect(1)
        .create();
    let _eq2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/query".into()))
        .with_body(
            json!({"methodResponses":[["Email/query",{"accountId":"w","ids":["E1"]},"q"]]})
                .to_string(),
        )
        .expect(1)
        .create();
    let _ech2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/changes".into()))
        .with_body(json!({"methodResponses":[["Email/changes",{"accountId":"w","oldState":"se1","newState":"se2","hasMoreChanges":false,"created":[],"updated":["E1"],"destroyed":[]},"c"]]}).to_string())
        .expect(1)
        .create();
    let bad_update = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/get".into()))
        .with_body(
            json!({"methodResponses":[["Email/get",{"accountId":"w","state":"se2","list":[
            {"id":"E1","mailboxIds":{},"keywords":{"$seen":true,"$flagged":true}}
        ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let s2 = sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::Mailbox, ObjectType::Email]),
    )
    .expect("second import");
    bad_update.assert();
    let em2 = s2
        .per_type
        .iter()
        .find(|(t, _)| *t == "Email")
        .map(|(_, c)| c.clone())
        .expect("email counts");
    assert_eq!(
        em2.updated, 0,
        "the failed update is not counted as applied"
    );
    assert!(em2.failed >= 1, "the unresolvable update is counted failed");
    let conn = rusqlite::Connection::open(&archive).unwrap();
    let cursor: String = conn
        .query_row(
            "SELECT state FROM sync_state_jmap WHERE type_name='Email'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        cursor, "se1",
        "cursor is held at the pre-change state so the failed update retries next run"
    );
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn import_unknown_method_changes_falls_back_to_full_refresh() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .expect_at_least(1)
        .create();
    let _term = anchor_terminator(&mut server, api, "Mailbox");

    let _q1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",{"accountId":"w","ids":["A"]},"q"]]})
                .to_string(),
        )
        .expect(1)
        .create();
    let _g1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/get".into()))
        .with_body(json!({"methodResponses":[["Mailbox/get",{"accountId":"w","state":"s1","list":[
            {"id":"A","name":"OriginalName","parentId":null,"role":null,"sortOrder":0,"isSubscribed":true}
        ],"notFound":[]},"g"]]}).to_string())
        .expect(1)
        .create();

    sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::Mailbox]),
    )
    .expect("first import");

    let _q2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",{"accountId":"w","ids":["A"]},"q"]]})
                .to_string(),
        )
        .expect(1)
        .create();
    let unknown = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/changes".into()))
        .with_body(json!({"methodResponses":[["error",{"type":"unknownMethod"},"c"]]}).to_string())
        .expect(1)
        .create();
    let _capture_state = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("Mailbox/get".into()),
            Matcher::Regex("\"ids\":\\[\\]".into()),
        ]))
        .with_body(json!({"methodResponses":[["Mailbox/get",{"accountId":"w","state":"s9","list":[],"notFound":[]},"g"]]}).to_string())
        .expect(1)
        .create();
    let refresh_get = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("Mailbox/get".into()),
            Matcher::Regex("\"A\"".into()),
        ]))
        .with_body(json!({"methodResponses":[["Mailbox/get",{"accountId":"w","state":"s9","list":[
            {"id":"A","name":"RefreshedName","parentId":null,"role":null,"sortOrder":0,"isSubscribed":true}
        ],"notFound":[]},"g"]]}).to_string())
        .expect(1)
        .create();

    let s2 = sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::Mailbox]),
    )
    .expect("second import");
    unknown.assert();
    refresh_get.assert();
    let mb2 = s2
        .per_type
        .iter()
        .find(|(t, _)| *t == "Mailbox")
        .map(|(_, c)| c.clone())
        .expect("mailbox counts");
    assert_eq!(
        mb2.updated, 1,
        "a server without Mailbox/changes degrades to a full refresh instead of aborting"
    );
    let conn = rusqlite::Connection::open(&archive).unwrap();
    let name: String = conn
        .query_row("SELECT name FROM mailboxes WHERE id=1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(name, "RefreshedName");
    let _ = std::fs::remove_file(&archive);
}

fn sieve_get_body(name: &str, blob: &str) -> String {
    json!({"methodResponses":[["SieveScript/get",{"accountId":"w","state":"x","list":[
        {"id":"S1","name":name,"isActive":true,"blobId":blob}
    ],"notFound":[]},"g"]]})
    .to_string()
}

#[test]
fn import_sieve_script_reimport_unchanged_is_convergent() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .expect_at_least(1)
        .create();
    let _term = anchor_terminator(&mut server, api, "SieveScript");
    let _dl = server
        .mock("GET", Matcher::Regex("/jmap/dl/w/B1/.*".into()))
        .with_body("keep;\n")
        .create();

    let _q1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("SieveScript/query".into()))
        .with_body(
            json!({"methodResponses":[["SieveScript/query",{"accountId":"w","ids":["S1"]},"q"]]})
                .to_string(),
        )
        .expect(1)
        .create();
    let _g1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("SieveScript/get".into()))
        .with_body(sieve_get_body("main", "B1"))
        .expect(1)
        .create();

    sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::SieveScript]),
    )
    .expect("first import");

    let _q2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("SieveScript/query".into()))
        .with_body(
            json!({"methodResponses":[["SieveScript/query",{"accountId":"w","ids":["S1"]},"q"]]})
                .to_string(),
        )
        .expect(1)
        .create();
    let _g2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("SieveScript/get".into()))
        .with_body(sieve_get_body("main", "B1"))
        .expect(1)
        .create();

    let s2 = sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::SieveScript]),
    )
    .expect("second import");
    let ss = s2
        .per_type
        .iter()
        .find(|(t, _)| *t == "SieveScript")
        .map(|(_, c)| c.clone())
        .expect("sieve counts");
    assert_eq!(ss.created, 0, "no new scripts");
    assert_eq!(
        ss.updated, 0,
        "an unchanged SieveScript must not be counted as updated on re-import (convergent)"
    );
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn import_sieve_script_content_change_is_propagated() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .expect_at_least(1)
        .create();
    let _term = anchor_terminator(&mut server, api, "SieveScript");
    let _dl1 = server
        .mock("GET", Matcher::Regex("/jmap/dl/w/B1/.*".into()))
        .with_body("keep;\n")
        .create();
    let _dl2 = server
        .mock("GET", Matcher::Regex("/jmap/dl/w/B2/.*".into()))
        .with_body("discard;\n")
        .create();

    let _q1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("SieveScript/query".into()))
        .with_body(
            json!({"methodResponses":[["SieveScript/query",{"accountId":"w","ids":["S1"]},"q"]]})
                .to_string(),
        )
        .expect(1)
        .create();
    let _g1 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("SieveScript/get".into()))
        .with_body(sieve_get_body("main", "B1"))
        .expect(1)
        .create();

    sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::SieveScript]),
    )
    .expect("first import");

    let _q2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("SieveScript/query".into()))
        .with_body(
            json!({"methodResponses":[["SieveScript/query",{"accountId":"w","ids":["S1"]},"q"]]})
                .to_string(),
        )
        .expect(1)
        .create();
    let _g2 = server
        .mock("POST", api)
        .match_body(Matcher::Regex("SieveScript/get".into()))
        .with_body(sieve_get_body("main", "B2"))
        .expect(1)
        .create();

    let s2 = sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::SieveScript]),
    )
    .expect("second import");
    let ss = s2
        .per_type
        .iter()
        .find(|(t, _)| *t == "SieveScript")
        .map(|(_, c)| c.clone())
        .expect("sieve counts");
    assert_eq!(
        ss.updated, 1,
        "the changed script content is re-fetched and updated"
    );
    let conn = rusqlite::Connection::open(&archive).unwrap();
    let body: Vec<u8> = conn
        .query_row(
            "SELECT b.data FROM blobs b JOIN sieve_scripts s ON s.blob_id = b.id LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        String::from_utf8(body).unwrap(),
        "discard;\n",
        "new script content propagated into the archive blob"
    );
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn first_run_cursor_is_captured_up_front_not_from_the_fetch() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .expect_at_least(1)
        .create();
    let _term = anchor_terminator(&mut server, api, "Mailbox");
    let _q = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",{"accountId":"w","ids":["A"]},"q"]]})
                .to_string(),
        )
        .expect(1)
        .create();
    // Up-front state snapshot (ids:[]) reports an EARLIER state than the new-fetch.
    let _state = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("Mailbox/get".into()),
            Matcher::Regex("\"ids\":\\[\\]".into()),
        ]))
        .with_body(json!({"methodResponses":[["Mailbox/get",{"accountId":"w","state":"before","list":[],"notFound":[]},"g"]]}).to_string())
        .expect(1)
        .create();
    // The new-fetch reports a LATER state; if we (incorrectly) captured from here, the cursor
    // would be "after" and an object changed mid-run could be missed next run.
    let _g = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("Mailbox/get".into()),
            Matcher::Regex("\"A\"".into()),
        ]))
        .with_body(json!({"methodResponses":[["Mailbox/get",{"accountId":"w","state":"after","list":[
            {"id":"A","name":"Personal","parentId":null,"role":null,"sortOrder":0,"isSubscribed":true}
        ],"notFound":[]},"g"]]}).to_string())
        .expect(1)
        .create();

    sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::Mailbox]),
    )
    .expect("import");
    let cursor: String = rusqlite::Connection::open(&archive)
        .unwrap()
        .query_row(
            "SELECT state FROM sync_state_jmap WHERE type_name='Mailbox'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        cursor, "before",
        "cursor must be the pre-fetch snapshot (lower bound), not the post-fetch state"
    );
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn export_duplicate_role_mailbox_created_as_plain_folder_keeping_subtree() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();
    {
        let conn = db::init::open(&archive).unwrap();
        conn.execute(
            "INSERT INTO mailboxes (id,name,parent_id,role) VALUES (1,'Inbox',NULL,'inbox')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mailboxes (id,name,parent_id,role) VALUES (2,'Sent',NULL,'sent')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mailboxes (id,name,parent_id,role) VALUES (3,'Éléments envoyés',NULL,'sent')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mailboxes (id,name,parent_id,role) VALUES (4,'Brouillons locaux',3,NULL)",
            [],
        )
        .unwrap();
    }

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .expect_at_least(1)
        .create();
    let _mbterm = anchor_terminator(&mut server, api, "Mailbox");

    let _mq = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
                {"accountId":"w","ids":["TI","TS"]},"q"]]})
            .to_string(),
        )
        .expect_at_least(1)
        .create();
    let _mg = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/get".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/get",{"accountId":"w","list":[
                {"id":"TI","name":"Inbox","role":"inbox","parentId":null,"myRights":{"mayDelete":true}},
                {"id":"TS","name":"Sent","role":"sent","parentId":null,"myRights":{"mayDelete":true}}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect_at_least(1)
        .create();

    let role_create = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("Mailbox/set".into()),
            Matcher::Regex("envoy".into()),
            Matcher::Regex("\"role\"".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["Mailbox/set",{"accountId":"w",
                "notCreated":{"c3":{"type":"invalidProperties","properties":["role"],
                "description":"A mailbox with role 'sent' already exists."}}},"s"]]})
            .to_string(),
        )
        .expect_at_most(1)
        .create();
    let folder_create = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("Mailbox/set".into()),
            Matcher::Regex("envoy".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["Mailbox/set",{"accountId":"w",
                "created":{"c3":{"id":"M3"}}},"s"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let child_create = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("Mailbox/set".into()),
            Matcher::Regex("Brouillons".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["Mailbox/set",{"accountId":"w",
                "created":{"c4":{"id":"M4"}}},"s"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let summary = sync::export::run(
        common(&archive),
        export_cfg_objects(&base, vec![ObjectType::Mailbox]),
    )
    .expect("export");

    role_create.assert();
    folder_create.assert();
    child_create.assert();

    let counts = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "Mailbox")
        .map(|(_, c)| c.clone())
        .expect("mailbox counts");
    assert_eq!(
        counts.skipped, 2,
        "Inbox and Sent match existing role mailboxes"
    );
    assert_eq!(
        counts.created, 2,
        "duplicate-role folder and its child are both created"
    );
    assert_eq!(counts.failed, 0, "no cascade skip of the subtree");
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn import_jmap_duplicate_role_is_deduplicated_to_single_mailbox() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .expect_at_least(1)
        .create();
    let _term = anchor_terminator(&mut server, api, "Mailbox");

    let _q = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
                {"accountId":"w","ids":["A","B","C"]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _g = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/get".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/get",{"accountId":"w","state":"s1","list":[
                {"id":"A","name":"Sent","parentId":null,"role":"sent","sortOrder":0,"isSubscribed":true},
                {"id":"B","name":"Éléments envoyés","parentId":null,"role":"sent","sortOrder":0,"isSubscribed":true},
                {"id":"C","name":"Sent Items","parentId":null,"role":"sent","sortOrder":0,"isSubscribed":true}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::Mailbox]),
    )
    .expect("import");

    let conn = rusqlite::Connection::open(&archive).unwrap();
    let total: i64 = conn
        .query_row("SELECT count(*) FROM mailboxes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total, 3, "all three folders are imported");
    let with_role: i64 = conn
        .query_row(
            "SELECT count(*) FROM mailboxes WHERE role = 'sent'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(with_role, 1, "exactly one mailbox keeps the sent role");
    let null_roles: i64 = conn
        .query_row(
            "SELECT count(*) FROM mailboxes WHERE role IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(null_roles, 2, "the surplus duplicates become plain folders");
    drop(conn);
    let _ = std::fs::remove_file(&archive);
}
