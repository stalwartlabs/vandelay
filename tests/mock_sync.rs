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

fn session_body_set_limit(base: &str, max_objects_in_set: u64) -> String {
    json!({
        "apiUrl": format!("{base}/jmap/api"),
        "uploadUrl": format!("{base}/jmap/upload/{{accountId}}/"),
        "downloadUrl": format!("{base}/jmap/dl/{{accountId}}/{{blobId}}/{{type}}/{{name}}"),
        "capabilities": { "urn:ietf:params:jmap:core": {
            "maxObjectsInGet": 500, "maxObjectsInSet": max_objects_in_set, "maxCallsInRequest": 16,
            "maxConcurrentRequests": 4, "maxConcurrentUpload": 4,
            "maxSizeRequest": 10000000, "maxSizeUpload": 50000000
        } },
        "accounts": { "w": { "name": "alice",
            "accountCapabilities": { "urn:ietf:params:jmap:mail": {} } } }
    })
    .to_string()
}

fn seed_inbox_emails(archive: &Path, n: usize, shared_blob: bool) {
    let conn = db::init::open(archive).unwrap();
    conn.execute(
        "INSERT INTO mailboxes (id,name,parent_id,role) VALUES (1,'Inbox',NULL,'inbox')",
        [],
    )
    .unwrap();
    for i in 1..=n {
        let raw = if shared_blob {
            "From: a@x\r\nSubject: dup\r\nMessage-ID: <dup-1@h>\r\n\r\nbody".to_owned()
        } else {
            format!("From: a@x\r\nSubject: m{i}\r\nMessage-ID: <m-{i}@h>\r\n\r\nbody {i}")
        };
        let blob = db::blobs::intern_blob(&conn, raw.as_bytes()).unwrap();
        conn.execute(
            "INSERT INTO emails (blob_id,received_at,mailbox_ids,keywords)
             VALUES (?1,'2020-01-01T00:00:00Z','[1]','[]')",
            rusqlite::params![blob],
        )
        .unwrap();
    }
}

fn mock_empty_target_with_inbox(
    server: &mut mockito::Server,
    session: String,
) -> Vec<mockito::Mock> {
    let api = "/jmap/api";
    vec![
        server.mock("GET", "/").with_status(404).create(),
        server
            .mock("GET", "/.well-known/jmap")
            .with_body(session)
            .create(),
        server
            .mock("POST", api)
            .match_body(Matcher::Regex("Mailbox/query".into()))
            .with_body(
                json!({"methodResponses":[["Mailbox/query",
                    {"accountId":"w","ids":["t1"]},"q"]]})
                .to_string(),
            )
            .expect(1)
            .create(),
        server
            .mock("POST", api)
            .match_body(Matcher::Regex("Mailbox/query".into()))
            .with_body(
                json!({"methodResponses":[["Mailbox/query",
                    {"accountId":"w","ids":[]},"q"]]})
                .to_string(),
            )
            .create(),
        server
            .mock("POST", api)
            .match_body(Matcher::Regex("Mailbox/get".into()))
            .with_body(
                json!({"methodResponses":[["Mailbox/get",{"accountId":"w","list":[
                    {"id":"t1","name":"Inbox","role":"inbox","parentId":null,
                     "myRights":{"mayDelete":true}}],"notFound":[]},"g"]]})
                .to_string(),
            )
            .create(),
        server
            .mock("POST", api)
            .match_body(Matcher::Regex("Email/query".into()))
            .with_body(
                json!({"methodResponses":[["Email/query",
                    {"accountId":"w","ids":[]},"q"]]})
                .to_string(),
            )
            .create(),
    ]
}

fn import_cids(body: &[u8]) -> Vec<String> {
    let v: Value = serde_json::from_slice(body).unwrap();
    v["methodCalls"][0][1]["emails"]
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

fn export_email_cfg(base: &str) -> ExportConfig {
    export_cfg_objects(base, vec![ObjectType::Mailbox, ObjectType::Email])
}

#[test]
fn email_export_batches_imports_by_max_objects_in_set() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    // 5 emails with maxObjectsInSet 2 need ceil(5/2) = 3 Email/import calls, none above 2 items.
    let mut server = mockito::Server::new();
    let base = server.url();
    let archive = tmp();
    seed_inbox_emails(&archive, 5, false);
    let _target = mock_empty_target_with_inbox(&mut server, session_body_set_limit(&base, 2));

    let blob_seq = Arc::new(AtomicUsize::new(0));
    let seq = blob_seq.clone();
    let ups = server
        .mock("POST", Matcher::Regex("/jmap/upload/".into()))
        .with_body_from_request(move |_| {
            let n = seq.fetch_add(1, Ordering::Relaxed);
            json!({ "blobId": format!("UP{n}") })
                .to_string()
                .into_bytes()
        })
        .expect(5)
        .create();

    let sizes = Arc::new(Mutex::new(Vec::new()));
    let seen = sizes.clone();
    let imports = server
        .mock("POST", "/jmap/api")
        .match_body(Matcher::Regex("Email/import".into()))
        .with_body_from_request(move |req| {
            let cids = import_cids(req.body().unwrap());
            seen.lock().unwrap().push(cids.len());
            let created: serde_json::Map<String, Value> = cids
                .into_iter()
                .map(|c| {
                    let id = format!("x-{c}");
                    (
                        c,
                        json!({"id": id, "blobId": "b", "threadId": "t", "size": 10}),
                    )
                })
                .collect();
            json!({"methodResponses":[["Email/import",
                {"accountId":"w","created": created},"i"]]})
            .to_string()
            .into_bytes()
        })
        .expect(3)
        .create();

    let mut cfg = common(&archive);
    cfg.threads = 4;
    let summary = sync::export::run(cfg, export_email_cfg(&base)).expect("export run");

    let email = email_counts(&summary);
    assert_eq!(email.created, 5);
    assert_eq!(email.failed, 0);
    let mut sizes = sizes.lock().unwrap().clone();
    sizes.sort_unstable();
    assert_eq!(sizes, vec![1, 2, 2]);
    ups.assert();
    imports.assert();
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn email_export_counts_mixed_batch_outcomes_per_item() {
    // One batch: e1 created, e2 alreadyExists (skipped), e3 overQuota (failed), e4 omitted (failed).
    let mut server = mockito::Server::new();
    let base = server.url();
    let archive = tmp();
    seed_inbox_emails(&archive, 4, false);
    let _target = mock_empty_target_with_inbox(&mut server, session_body(&base));

    let _ups = server
        .mock("POST", Matcher::Regex("/jmap/upload/".into()))
        .with_body(json!({"blobId":"UP"}).to_string())
        .expect(4)
        .create();
    let imports = server
        .mock("POST", "/jmap/api")
        .match_body(Matcher::Regex("Email/import".into()))
        .with_body(
            json!({"methodResponses":[["Email/import",{"accountId":"w",
                "created":{"e1":{"id":"x1","blobId":"UP","threadId":"t","size":10}},
                "notCreated":{
                    "e2":{"type":"alreadyExists","existingId":"x9"},
                    "e3":{"type":"overQuota"}
                }},"i"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let summary = sync::export::run(common(&archive), export_email_cfg(&base)).expect("export run");

    let email = email_counts(&summary);
    assert_eq!(email.created, 1);
    assert_eq!(email.skipped, 1);
    assert_eq!(email.failed, 2);
    imports.assert();
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn export_email_blob_not_found_reuploads_and_retries() {
    // Two emails share one blob; e2 gets blobNotFound in the batch, so only e2 is re-uploaded and retried.
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();
    seed_inbox_emails(&archive, 2, true);
    let _target = mock_empty_target_with_inbox(&mut server, session_body(&base));

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

    let batch = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("Email/import".into()),
            Matcher::Regex("UP1".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["Email/import",{"accountId":"w",
                "created":{"e1":{"id":"x1","blobId":"UP1","threadId":"t","size":10}},
                "notCreated":{"e2":{"type":"blobNotFound"}}},"i"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let retry = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("Email/import".into()),
            Matcher::Regex("UP2".into()),
        ]))
        .with_body_from_request(|req| {
            let cids = import_cids(req.body().unwrap());
            assert_eq!(
                cids,
                vec!["e2".to_owned()],
                "only the affected item is retried"
            );
            json!({"methodResponses":[["Email/import",{"accountId":"w",
                "created":{"e2":{"id":"x2","blobId":"UP2","threadId":"t","size":10}}},"i"]]})
            .to_string()
            .into_bytes()
        })
        .expect(1)
        .create();

    let summary = sync::export::run(common(&archive), export_email_cfg(&base)).expect("export run");

    let email = email_counts(&summary);
    assert_eq!(email.created, 2, "both emails end up created");
    assert_eq!(email.failed, 0, "blobNotFound self-heals, not a failure");
    assert_eq!(email.skipped, 0);
    assert!(!summary.any_failed());

    up1.assert();
    up2.assert();
    batch.assert();
    retry.assert();
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
fn import_unusable_received_at_falls_back_instead_of_dropping_the_email() {
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
                {"accountId":"w","ids":["E1","E2","E3"]},"q"]]})
            .to_string(),
        )
        .create();
    let _eg = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/get".into()))
        .with_body(
            json!({"methodResponses":[["Email/get",{"accountId":"w","state":"se1","list":[
                {"id":"E1","blobId":"BLB1","receivedAt":"30828-09-14T02:48:05Z","mailboxIds":{"MX":true},"keywords":{}},
                {"id":"E2","blobId":"BLB2","mailboxIds":{"MX":true},"keywords":{}},
                {"id":"E3","blobId":"BLB3","receivedAt":"2020-01-03 04:05:06Z","mailboxIds":{"MX":true},"keywords":{}}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .create();
    let _dl1 = server
        .mock("GET", Matcher::Regex("/jmap/dl/w/BLB1/.*".into()))
        .with_body(
            "From: a@x\r\nMessage-ID: <1@h>\r\nDate: Mon, 12 May 2025 10:00:00 +0200\r\n\r\none",
        )
        .create();
    let _dl2 = server
        .mock("GET", Matcher::Regex("/jmap/dl/w/BLB2/.*".into()))
        .with_body("From: b@x\r\nMessage-ID: <2@h>\r\n\r\ntwo")
        .create();
    let _dl3 = server
        .mock("GET", Matcher::Regex("/jmap/dl/w/BLB3/.*".into()))
        .with_body("From: c@x\r\nMessage-ID: <3@h>\r\n\r\nthree")
        .create();

    let summary = sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::Mailbox, ObjectType::Email]),
    )
    .expect("import does not abort on an unusable receivedAt");

    let email = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "Email")
        .map(|(_, c)| c.clone())
        .expect("email counts");
    assert_eq!(email.fetched, 3, "no email is dropped over its receivedAt");
    assert_eq!(email.failed, 0, "an unusable receivedAt is not a failure");
    assert!(!summary.any_failed());

    let conn = rusqlite::Connection::open(&archive).unwrap();
    let mut stmt = conn
        .prepare("SELECT received_at FROM emails ORDER BY id")
        .unwrap();
    let dates: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    drop(stmt);
    assert_eq!(
        dates[0], "2025-05-12T10:00:00+02:00",
        "an out-of-range receivedAt falls back to the Date header"
    );
    assert_eq!(
        dates[1], "1970-01-01T00:00:00Z",
        "no receivedAt and no Date header falls back to the epoch"
    );
    assert_eq!(
        dates[2], "2020-01-03T04:05:06Z",
        "a space-separated receivedAt is salvaged, not discarded"
    );
    let _ = std::fs::remove_file(&archive);
}

// Draft -18 FileNodes import by @type; an unrecognised @type fails only that node.
#[test]
fn import_file_node_by_type_and_skips_unknown_type() {
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
    let _fnterm = anchor_terminator(&mut server, api, "FileNode");

    let _fq = server
        .mock("POST", api)
        .match_body(Matcher::Regex("FileNode/query".into()))
        .with_body(
            json!({"methodResponses":[["FileNode/query",
                {"accountId":"w","ids":["F1","F2","F3","F4"]},"q"]]})
            .to_string(),
        )
        .create();
    let _fg = server
        .mock("POST", api)
        .match_body(Matcher::Regex("FileNode/get".into()))
        .with_body(
            json!({"methodResponses":[["FileNode/get",{"accountId":"w","state":"sf1","list":[
                {"@type":"Directory","id":"F1","parentId":null,"name":"docs","role":null,
                 "isSubscribed":true,"created":"2026-01-01T00:00:00Z"},
                {"@type":"File","id":"F2","parentId":"F1","name":"a.sh","blobId":"BLBF",
                 "size":10,"mediaType":"text/x-sh","executable":true,
                 "created":"2026-01-02T00:00:00Z"},
                {"@type":"SymLink","id":"F3","parentId":null,"name":"latest",
                 "target":["docs","a.sh"],"created":"2026-01-03T00:00:00Z"},
                {"@type":"Socket","id":"F4","parentId":null,"name":"sock",
                 "created":"2026-01-04T00:00:00Z"}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .create();
    let _dl = server
        .mock("GET", Matcher::Regex("/jmap/dl/w/BLBF/.*".into()))
        .with_body("file bytes")
        .create();

    let outcome = sync::import_jmap::run_reporting(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::FileNode]),
    );
    assert!(outcome.error.is_none(), "{:?}", outcome.error);

    let nodes = outcome
        .summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "FileNode")
        .map(|(_, c)| c.clone())
        .expect("file node counts");
    assert_eq!(nodes.fetched, 3);
    assert_eq!(nodes.failed, 1);

    let conn = rusqlite::Connection::open(&archive).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT name, node_type, media_type, executable, target FROM file_nodes ORDER BY name",
        )
        .unwrap();
    type Row = (String, String, Option<String>, i64, Option<String>);
    let rows: Vec<Row> = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    drop(stmt);
    assert_eq!(
        rows,
        vec![
            (
                "a.sh".to_owned(),
                "file".to_owned(),
                Some("text/x-sh".to_owned()),
                1,
                None
            ),
            ("docs".to_owned(), "directory".to_owned(), None, 0, None),
            (
                "latest".to_owned(),
                "symlink".to_owned(),
                None,
                0,
                Some(r#"["docs","a.sh"]"#.to_owned())
            ),
        ]
    );
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
fn import_writes_each_email_blob_before_later_downloads_complete() {
    // Each download waits for the previous blob to reach the archive WAL; collecting a page first would time out.
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();
    let wal = PathBuf::from(format!("{}-wal", archive.display()));

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
                {"accountId":"w","ids":["E1","E2","E3"]},"q"]]})
            .to_string(),
        )
        .create();
    let list: Vec<Value> = (1..=3)
        .map(|n| {
            json!({"id":format!("E{n}"),"blobId":format!("BLB{n}"),
                "receivedAt":"2020-01-01T00:00:00Z","mailboxIds":{"MX":true},"keywords":{}})
        })
        .collect();
    let _eg = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/get".into()))
        .with_body(
            json!({"methodResponses":[["Email/get",{"accountId":"w","state":"se1",
                "list":list,"notFound":[]},"g"]]})
            .to_string(),
        )
        .create();

    let wal_seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
    let wal_len = {
        let wal = wal.clone();
        move || std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0)
    };
    let mut downloads = Vec::new();
    for n in 1..=3u8 {
        let seen = wal_seen.clone();
        let wal_len = wal_len.clone();
        downloads.push(
            server
                .mock("GET", Matcher::Regex(format!("/jmap/dl/w/BLB{n}/.*")))
                .with_body_from_request(move |_| {
                    let floor = seen.lock().unwrap().last().copied();
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                    let mut len = wal_len();
                    while floor.is_some_and(|f| len <= f) && std::time::Instant::now() < deadline {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        len = wal_len();
                    }
                    seen.lock().unwrap().push(len);
                    let mut body = format!("From: a@x\r\nMessage-ID: <{n}@h>\r\n\r\n").into_bytes();
                    body.resize(body.len() + 64 * 1024, b'a' + n);
                    body
                })
                .create(),
        );
    }

    let summary = sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::Mailbox, ObjectType::Email]),
    )
    .expect("import succeeds");
    assert!(!summary.any_failed());

    let seen = wal_seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 3);
    assert!(
        seen.windows(2).all(|w| w[1] > w[0]),
        "each blob reaches the archive before the next download finishes: {seen:?}"
    );
    let conn = rusqlite::Connection::open(&archive).unwrap();
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM emails", [], |r| r.get(0))
            .unwrap(),
        3
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
fn export_leaves_an_existing_target_default_address_book_alone() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();
    {
        let conn = db::init::open(&archive).unwrap();
        conn.execute(
            "INSERT INTO address_books (id,name,description,is_default)
             VALUES (1,'Contacts',NULL,1)",
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
                {"id":"P","name":"Personal","isDefault":true,"myRights":{"mayDelete":false}}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let no_default_set = server
        .mock("POST", api)
        .match_body(Matcher::Regex("onSuccessSetIsDefault".into()))
        .with_body(
            json!({"methodResponses":[["AddressBook/set",{"accountId":"w"},"d"]]}).to_string(),
        )
        .expect(0)
        .create();
    let create = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("AddressBook/set".into()),
            Matcher::Regex("Contacts".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["AddressBook/set",{"accountId":"w",
                "created":{"c1":{"id":"CID"}}},"s"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    sync::export::run(
        common(&archive),
        export_cfg_objects(&base, vec![ObjectType::AddressBook]),
    )
    .expect("export");
    create.assert();
    no_default_set.assert();
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn export_claims_the_default_address_book_when_the_target_has_none() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();
    {
        let conn = db::init::open(&archive).unwrap();
        conn.execute(
            "INSERT INTO address_books (id,name,description,is_default)
             VALUES (1,'Contacts',NULL,1)",
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
                {"id":"P","name":"Personal","isDefault":false,"myRights":{"mayDelete":true}}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let default_set = server
        .mock("POST", api)
        .match_body(Matcher::Regex("onSuccessSetIsDefault".into()))
        .with_body(
            json!({"methodResponses":[["AddressBook/set",{"accountId":"w"},"d"]]}).to_string(),
        )
        .expect(1)
        .create();
    let create = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("AddressBook/set".into()),
            Matcher::Regex("Contacts".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["AddressBook/set",{"accountId":"w",
                "created":{"c1":{"id":"CID"}}},"s"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    sync::export::run(
        common(&archive),
        export_cfg_objects(&base, vec![ObjectType::AddressBook]),
    )
    .expect("export");
    create.assert();
    default_set.assert();
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn import_never_stores_two_default_address_books() {
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
    let _abg = server
        .mock("POST", api)
        .match_body(Matcher::Regex("AddressBook/get".into()))
        .with_body(
            json!({"methodResponses":[["AddressBook/get",{"accountId":"w","state":"sa1","list":[
                {"id":"A1","name":"Contacts","isDefault":true,"isSubscribed":true,"sortOrder":0},
                {"id":"A2","name":"GAL","isDefault":true,"isSubscribed":true,"sortOrder":0}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .create();

    let summary = sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::AddressBook]),
    )
    .expect("import");
    let books = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "AddressBook")
        .map(|(_, c)| c.clone())
        .expect("address book counts");
    assert_eq!(books.fetched, 2, "both address books are imported");

    let conn = rusqlite::Connection::open(&archive).unwrap();
    let defaults: i64 = conn
        .query_row(
            "SELECT count(*) FROM address_books WHERE is_default = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        defaults, 1,
        "an archive must never hold more than one default address book"
    );
    let name: String = conn
        .query_row(
            "SELECT name FROM address_books WHERE is_default = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(name, "Contacts", "the first claimant keeps the default");
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

// DAV alpha colors are sent without alpha and unusable colors are dropped.
#[test]
fn export_calendar_normalizes_colors() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();
    {
        let conn = db::init::open(&archive).unwrap();
        conn.execute(
            "INSERT INTO calendars (id,name,color,is_default) VALUES
                (1,'Sogo','#AAAAAAFF',0), (2,'Short','#abcd',0), (3,'Odd','rgba(1,2,3,0.5)',0)",
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
            json!({"methodResponses":[["Calendar/get",{"accountId":"w","list":[],
                "notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let create = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Calendar/set".into()))
        .with_body_from_request(|req| {
            let body: Value = serde_json::from_slice(req.body().unwrap()).unwrap();
            let create = &body["methodCalls"][0][1]["create"];
            assert_eq!(create["c1"]["color"], json!("#AAAAAA"));
            assert_eq!(create["c2"]["color"], json!("#abc"));
            assert!(create["c3"].get("color").is_none(), "{create}");
            json!({"methodResponses":[["Calendar/set",{"accountId":"w",
                "created":{"c1":{"id":"T1"},"c2":{"id":"T2"},"c3":{"id":"T3"}}},"s"]]})
            .to_string()
            .into_bytes()
        })
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
    assert_eq!(counts.created, 3);
    assert_eq!(counts.failed, 0);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn export_contact_and_event_blobs_are_inlined_never_blob_ids() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();
    let photo: &[u8] = b"\x89PNG\r\n\x1a\nfake-photo-bytes";
    let agenda: &[u8] = b"agenda for the review meeting";
    {
        let conn = db::init::open(&archive).unwrap();
        conn.execute(
            "INSERT INTO address_books (id,name,is_default) VALUES (1,'Personal',1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO calendars (id,name,is_default) VALUES (1,'Work',1)",
            [],
        )
        .unwrap();
        let photo_blob = db::blobs::intern_blob(&conn, photo).unwrap();
        let agenda_blob = db::blobs::intern_blob(&conn, agenda).unwrap();
        let card = json!({
            "@type": "Card",
            "version": "1.0",
            "name": { "full": "Photo Person" },
            "media": { "photo": {
                "@type": "Media", "kind": "photo",
                "@blob": photo_blob, "mediaType": "image/png"
            } }
        })
        .to_string();
        conn.execute(
            "INSERT INTO contact_cards (id,uid,address_book_ids,data)
             VALUES (1,'card-uid-1','[1]',?1)",
            [card.as_str()],
        )
        .unwrap();
        let event = json!({
            "@type": "Event",
            "uid": "event-uid-1",
            "title": "Review",
            "start": "2026-03-01T09:00:00",
            "duration": "PT1H",
            "timeZone": "Etc/UTC",
            "links": { "1": {
                "@type": "Link", "rel": "enclosure",
                "@blob": agenda_blob, "contentType": "text/plain", "title": "agenda.txt"
            } }
        })
        .to_string();
        conn.execute(
            "INSERT INTO calendar_events (id,calendar_ids,is_draft,use_default_alerts,data)
             VALUES (1,'[1]',0,0,?1)",
            [event.as_str()],
        )
        .unwrap();
    }

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .expect_at_least(1)
        .create();

    let no_blob_id_in_card = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("ContactCard/set".into()),
            Matcher::Regex("blobId".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["ContactCard/set",{"accountId":"w",
                "created":{"c1":{"id":"TCARD"}}},"s"]]})
            .to_string(),
        )
        .expect(0)
        .create();
    let no_blob_id_in_event = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("CalendarEvent/set".into()),
            Matcher::Regex("blobId".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["CalendarEvent/set",{"accountId":"w",
                "created":{"c1":{"id":"TEVENT"}}},"s"]]})
            .to_string(),
        )
        .expect(0)
        .create();

    let _ab_get = server
        .mock("POST", api)
        .match_body(Matcher::Regex("AddressBook/get".into()))
        .with_body(
            json!({"methodResponses":[["AddressBook/get",{"accountId":"w","list":[],
                "notFound":[]},"g"]]})
            .to_string(),
        )
        .expect_at_least(1)
        .create();
    let _ab_set = server
        .mock("POST", api)
        .match_body(Matcher::Regex("AddressBook/set".into()))
        .with_body(
            json!({"methodResponses":[["AddressBook/set",{"accountId":"w",
                "created":{"c1":{"id":"TAB"}}},"s"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _cal_get = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Calendar/get".into()))
        .with_body(
            json!({"methodResponses":[["Calendar/get",{"accountId":"w","list":[],
                "notFound":[]},"g"]]})
            .to_string(),
        )
        .expect_at_least(1)
        .create();
    let _cal_set = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Calendar/set".into()))
        .with_body(
            json!({"methodResponses":[["Calendar/set",{"accountId":"w",
                "created":{"c1":{"id":"TCAL"}}},"s"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _cc_query = server
        .mock("POST", api)
        .match_body(Matcher::Regex("ContactCard/query".into()))
        .with_body(
            json!({"methodResponses":[["ContactCard/query",{"accountId":"w","ids":[]},"q"]]})
                .to_string(),
        )
        .expect_at_least(1)
        .create();
    let _ce_query = server
        .mock("POST", api)
        .match_body(Matcher::Regex("CalendarEvent/query".into()))
        .with_body(
            json!({"methodResponses":[["CalendarEvent/query",{"accountId":"w","ids":[]},"q"]]})
                .to_string(),
        )
        .expect_at_least(1)
        .create();

    let card_inlined = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("ContactCard/set".into()),
            Matcher::Regex("data:image/png;base64,".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["ContactCard/set",{"accountId":"w",
                "created":{"c1":{"id":"TCARD"}}},"s"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let event_inlined = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("CalendarEvent/set".into()),
            Matcher::Regex("data:text/plain;base64,".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["CalendarEvent/set",{"accountId":"w",
                "created":{"c1":{"id":"TEVENT"}}},"s"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let summary = sync::export::run(
        common(&archive),
        export_cfg_objects(
            &base,
            vec![
                ObjectType::AddressBook,
                ObjectType::Calendar,
                ObjectType::ContactCard,
                ObjectType::CalendarEvent,
            ],
        ),
    )
    .expect("export");

    no_blob_id_in_card.assert();
    no_blob_id_in_event.assert();
    card_inlined.assert();
    event_inlined.assert();

    for name in ["ContactCard", "CalendarEvent"] {
        let counts = summary
            .per_type
            .iter()
            .find(|(t, _)| *t == name)
            .map(|(_, c)| c.clone())
            .unwrap_or_else(|| panic!("{name} counts"));
        assert_eq!(counts.created, 1, "{name} created: {counts:?}");
        assert_eq!(counts.failed, 0, "{name} failed: {counts:?}");
    }
    let _ = std::fs::remove_file(&archive);
}

fn session_body_small_request(base: &str, max_size_request: u64) -> String {
    json!({
        "apiUrl": format!("{base}/jmap/api"),
        "uploadUrl": format!("{base}/jmap/upload/{{accountId}}/"),
        "downloadUrl": format!("{base}/jmap/dl/{{accountId}}/{{blobId}}/{{type}}/{{name}}"),
        "capabilities": { "urn:ietf:params:jmap:core": {
            "maxObjectsInGet": 500, "maxObjectsInSet": 500, "maxCallsInRequest": 16,
            "maxConcurrentRequests": 4, "maxConcurrentUpload": 4,
            "maxSizeRequest": max_size_request, "maxSizeUpload": 50000000
        } },
        "accounts": { "w": { "name": "alice",
            "accountCapabilities": {
                "urn:ietf:params:jmap:mail": {},
                "urn:ietf:params:jmap:contacts": {},
                "urn:ietf:params:jmap:calendars": {}
            } } }
    })
    .to_string()
}

#[test]
fn export_oversized_contact_card_fails_alone_and_the_type_keeps_going() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();
    {
        let conn = db::init::open(&archive).unwrap();
        conn.execute(
            "INSERT INTO address_books (id,name,is_default) VALUES (1,'Personal',1)",
            [],
        )
        .unwrap();
        let photo = vec![b'P'; 8000];
        let photo_blob = db::blobs::intern_blob(&conn, &photo).unwrap();
        let huge = json!({
            "@type": "Card",
            "name": { "full": "Photo Person" },
            "media": { "photo": {
                "@type": "Media", "kind": "photo",
                "@blob": photo_blob, "mediaType": "image/png"
            } }
        })
        .to_string();
        conn.execute(
            "INSERT INTO contact_cards (id,uid,address_book_ids,data)
             VALUES (1,'huge-card','[1]',?1)",
            [huge.as_str()],
        )
        .unwrap();
        let small = json!({ "@type": "Card", "name": { "full": "Small Person" } }).to_string();
        conn.execute(
            "INSERT INTO contact_cards (id,uid,address_book_ids,data)
             VALUES (2,'small-card','[1]',?1)",
            [small.as_str()],
        )
        .unwrap();
    }

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_small_request(&base, 4000))
        .expect_at_least(1)
        .create();
    let _ccterm = anchor_terminator(&mut server, api, "ContactCard");

    let _ab_get = server
        .mock("POST", api)
        .match_body(Matcher::Regex("AddressBook/get".into()))
        .with_body(
            json!({"methodResponses":[["AddressBook/get",{"accountId":"w","list":[],
                "notFound":[]},"g"]]})
            .to_string(),
        )
        .expect_at_least(1)
        .create();
    let _ab_set = server
        .mock("POST", api)
        .match_body(Matcher::Regex("AddressBook/set".into()))
        .with_body(
            json!({"methodResponses":[["AddressBook/set",{"accountId":"w",
                "created":{"c1":{"id":"TAB"}}},"s"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _cc_query = server
        .mock("POST", api)
        .match_body(Matcher::Regex("ContactCard/query".into()))
        .with_body(
            json!({"methodResponses":[["ContactCard/query",{"accountId":"w",
                "ids":["TSTALE"]},"q"]]})
            .to_string(),
        )
        .expect_at_least(1)
        .create();
    let _cc_get = server
        .mock("POST", api)
        .match_body(Matcher::Regex("ContactCard/get".into()))
        .with_body(
            json!({"methodResponses":[["ContactCard/get",{"accountId":"w","list":[
                {"id":"TSTALE","uid":"stale-card"}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect_at_least(1)
        .create();
    let destroyed = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("ContactCard/set".into()),
            Matcher::Regex("destroy".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["ContactCard/set",{"accountId":"w",
                "destroyed":["TSTALE"]},"s"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let created = server
        .mock("POST", api)
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("ContactCard/set".into()),
            Matcher::Regex("Small Person".into()),
        ]))
        .with_body(
            json!({"methodResponses":[["ContactCard/set",{"accountId":"w",
                "created":{"c2":{"id":"TSMALL"}}},"s"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let summary = sync::export::run(
        common(&archive),
        ExportConfig {
            prune: true,
            ..export_cfg_objects(
                &base,
                vec![ObjectType::AddressBook, ObjectType::ContactCard],
            )
        },
    )
    .expect("the oversized card must not abort the run");

    created.assert();
    destroyed.assert();
    let counts = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "ContactCard")
        .map(|(_, c)| c.clone())
        .expect("ContactCard counts");
    assert_eq!(counts.created, 1, "{counts:?}");
    assert_eq!(counts.failed, 1, "{counts:?}");
    assert_eq!(
        counts.deleted, 1,
        "the prune plan stays complete: {counts:?}"
    );
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn export_archive_read_failure_while_inlining_exits_seven() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();
    {
        let conn = db::init::open(&archive).unwrap();
        conn.execute(
            "INSERT INTO address_books (id,name,is_default) VALUES (1,'Personal',1)",
            [],
        )
        .unwrap();
        let photo_blob = db::blobs::intern_blob(&conn, b"photo-bytes").unwrap();
        let card = json!({
            "@type": "Card",
            "media": { "photo": { "@type": "Media", "kind": "photo",
                "@blob": photo_blob, "mediaType": "image/png" } }
        })
        .to_string();
        conn.execute(
            "INSERT INTO contact_cards (id,uid,address_book_ids,data)
             VALUES (1,'card-uid-1','[1]',?1)",
            [card.as_str()],
        )
        .unwrap();
        conn.execute("ALTER TABLE blobs RENAME COLUMN data TO payload", [])
            .unwrap();
    }

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .expect_at_least(1)
        .create();
    let _cc_query = server
        .mock("POST", api)
        .match_body(Matcher::Regex("ContactCard/query".into()))
        .with_body(
            json!({"methodResponses":[["ContactCard/query",{"accountId":"w","ids":[]},"q"]]})
                .to_string(),
        )
        .expect_at_least(1)
        .create();
    let no_create = server
        .mock("POST", api)
        .match_body(Matcher::Regex("ContactCard/set".into()))
        .with_body(
            json!({"methodResponses":[["ContactCard/set",{"accountId":"w",
                "created":{"c1":{"id":"TCARD"}}},"s"]]})
            .to_string(),
        )
        .expect(0)
        .create();

    let err = sync::export::run(
        common(&archive),
        export_cfg_objects(&base, vec![ObjectType::ContactCard]),
    )
    .expect_err("an unreadable blobs table must abort the run");

    no_create.assert();
    assert!(err.aborts_run(), "{err}");
    assert_eq!(err.exit_code(), 7, "{err}");
    let _ = std::fs::remove_file(&archive);
}

// Archived participant identities are never sent to the target, even with prune, and nothing fails.
#[test]
fn export_skips_participant_identities() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";
    let archive = tmp();
    {
        let conn = db::init::open(&archive).unwrap();
        conn.execute(
            "INSERT INTO participant_identities (id,name,calendar_address,is_default)
             VALUES (1,'Bob','mailto:bob@example.org',1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO calendars (id,name,is_default) VALUES (1,'Family',1)",
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
    let participant = server
        .mock("POST", api)
        .match_body(Matcher::Regex("ParticipantIdentity/".into()))
        .with_status(500)
        .expect(0)
        .create();
    let calendars = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Calendar/get".into()))
        .with_body(
            json!({"methodResponses":[["Calendar/get",{"accountId":"w","list":[
                {"id":"F","name":"Family","isDefault":true}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    for objects in [
        vec![ObjectType::ParticipantIdentity, ObjectType::Calendar],
        vec![ObjectType::ParticipantIdentity],
    ] {
        let mut config = export_cfg_objects(&base, objects);
        config.prune = true;
        let summary = sync::export::run(common(&archive), config).expect("export");
        assert!(!summary.any_failed(), "{:?}", summary.per_type);
        assert!(
            summary
                .per_type
                .iter()
                .all(|(t, _)| *t != "ParticipantIdentity")
        );
    }
    participant.assert();
    calendars.assert();
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

// Pruning stale scripts leaves the archive's active script active on the target.
#[test]
fn export_sieve_prune_keeps_archive_active_script_active() {
    prune_sieve_with_archive_active(true, Some("M1"));
}

// With no active script in the archive, the stale active target script is deactivated and pruned.
#[test]
fn export_sieve_prune_destroys_stale_active_script() {
    prune_sieve_with_archive_active(false, None);
}

fn prune_sieve_with_archive_active(archive_active: bool, expected_active: Option<&str>) {
    let mut server = mockito::Server::new();
    let base = server.url();
    let archive = tmp();
    {
        let conn = db::init::open(&archive).unwrap();
        let blob = db::blobs::intern_blob(&conn, b"keep;\n").unwrap();
        conn.execute(
            "INSERT INTO sieve_scripts (id,name,is_active,blob_id) VALUES (1,'main',?1,?2)",
            rusqlite::params![archive_active, blob],
        )
        .unwrap();
    }

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .expect_at_least(1)
        .create();
    let active = std::sync::Arc::new(std::sync::Mutex::new(Some("O1".to_owned())));
    let target_active = active.clone();
    let log = CallLog::default();
    let _api = jmap_dispatch(&mut server, &log, move |name, args| {
        let mut active = target_active.lock().unwrap();
        match name {
            "SieveScript/get" => json!({"accountId":"w","state":"x","notFound":[],"list":[
                {"id":"M1","name":"main","isActive":active.as_deref() == Some("M1"),"blobId":"BM"},
                {"id":"O1","name":"old","isActive":active.as_deref() == Some("O1"),"blobId":"BO"}
            ]}),
            "SieveScript/set" => {
                if args["onSuccessDeactivateScript"] == json!(true) {
                    *active = None;
                }
                if let Some(id) = args["onSuccessActivateScript"].as_str() {
                    *active = Some(id.to_owned());
                }
                let mut response = json!({"accountId":"w"});
                if let Some(ids) = args["destroy"].as_array() {
                    let (blocked, destroyed): (Vec<&Value>, Vec<&Value>) =
                        ids.iter().partition(|id| id.as_str() == active.as_deref());
                    response["destroyed"] = json!(destroyed);
                    response["notDestroyed"] = blocked
                        .iter()
                        .map(|id| {
                            (
                                id.as_str().unwrap().to_owned(),
                                json!({"type":"scriptIsActive"}),
                            )
                        })
                        .collect::<serde_json::Map<_, _>>()
                        .into();
                }
                response
            }
            other => panic!("unexpected {other}"),
        }
    });

    let mut config = export_cfg_objects(&base, vec![ObjectType::SieveScript]);
    config.prune = true;
    let summary = sync::export::run(common(&archive), config).expect("export");
    assert!(!summary.any_failed(), "{:?}", summary.per_type);
    let destroyed: Vec<Value> = log
        .lock()
        .unwrap()
        .iter()
        .filter_map(|(_, args)| args.get("destroy").cloned())
        .collect();
    assert_eq!(destroyed, vec![json!(["O1"])]);
    assert_eq!(active.lock().unwrap().as_deref(), expected_active);
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

#[test]
fn export_aborts_with_exit_two_when_the_advertised_api_url_is_unreachable() {
    let mut session_host = mockito::Server::new();
    let mut api_host = mockito::Server::new();
    let session_base = session_host.url();
    let api_base = api_host.url();
    let archive = tmp();
    {
        let conn = db::init::open(&archive).unwrap();
        conn.execute(
            "INSERT INTO mailboxes (id,name,parent_id,role,sort_order,is_subscribed)
             VALUES (1,'Inbox',NULL,'inbox',0,1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO address_books (id,name,description,is_default)
             VALUES (1,'Personal',NULL,1)",
            [],
        )
        .unwrap();
    }

    let _root = session_host.mock("GET", "/").with_status(404).create();
    let _wk = session_host
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&api_base))
        .expect_at_least(1)
        .create();
    let nginx = api_host
        .mock("POST", "/jmap/api")
        .with_status(404)
        .with_header("content-type", "text/html")
        .with_body("<html><head><title>404 Not Found</title></head><body>nginx</body></html>")
        .expect(1)
        .create();

    let err = sync::export::run(
        CommonConfig {
            archive: archive.clone(),
            threads: 1,
            dry_run: false,
            max_retries: 0,
            allow_invalid_certs: false,
            logger: Logger::from_flags(true, 0),
        },
        export_cfg_objects(
            &session_base,
            vec![ObjectType::Mailbox, ObjectType::AddressBook],
        ),
    )
    .expect_err("an unreachable apiUrl must abort the run");

    assert!(
        matches!(err, vandelay::error::Error::Connection(_)),
        "a 404 from the advertised apiUrl is a whole-run connection failure, got {err:?}"
    );
    assert_eq!(err.exit_code(), 2, "must not report a partial failure");
    nginx.assert();
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn aborting_import_still_reports_the_types_that_finished() {
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

    let _mq = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
                {"accountId":"w","ids":["A"]},"q"]]})
            .to_string(),
        )
        .expect_at_least(1)
        .create();
    let _mg = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/get".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/get",{"accountId":"w","state":"s1","list":[
                {"id":"A","name":"alpha","parentId":null,"role":null,"sortOrder":0,"isSubscribed":true}
            ],"notFound":[]},"g"]]})
            .to_string(),
        )
        .expect_at_least(1)
        .create();
    let _dead_endpoint = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/query".into()))
        .with_status(404)
        .with_header("content-type", "text/html")
        .with_body("<html><head><title>404 Not Found</title></head><body>nginx</body></html>")
        .expect_at_least(1)
        .create();

    let outcome = sync::import_jmap::run_reporting(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::Mailbox, ObjectType::Email]),
    );

    let err = outcome
        .error
        .as_ref()
        .expect("a 404 from the api endpoint aborts the run");
    assert!(
        matches!(err, vandelay::error::Error::Connection(_)),
        "the abort must keep its own exit code, got {err:?}"
    );
    assert_eq!(err.exit_code(), 2);

    let mailbox = outcome
        .summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "Mailbox")
        .map(|(_, c)| c.clone())
        .expect("the type that finished before the abort must still be reported");
    assert_eq!(mailbox.fetched, 1);
    assert!(
        !outcome.summary.per_type.iter().any(|(t, _)| *t == "Email"),
        "the type that aborted contributes no counts"
    );
    let _ = std::fs::remove_file(&archive);
}

fn seed_single_email(archive: &Path) {
    let conn = db::init::open(archive).unwrap();
    conn.execute(
        "INSERT INTO mailboxes (id,name,parent_id,role) VALUES (1,'Inbox',NULL,'inbox')",
        [],
    )
    .unwrap();
    let blob = db::blobs::intern_blob(
        &conn,
        b"From: a@x\r\nSubject: hi\r\nMessage-ID: <m-9@h>\r\n\r\nbody",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO emails (blob_id,received_at,mailbox_ids,keywords)
         VALUES (?1,'2020-01-01T00:00:00Z','[1]','[\"$seen\"]')",
        rusqlite::params![blob],
    )
    .unwrap();
}

fn mock_reconcile_for_single_email(server: &mut mockito::ServerGuard, base: &str, api: &str) {
    server.mock("GET", "/").with_status(404).create();
    server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body(base))
        .create();
    server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
            {"accountId":"w","ids":["t1"]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    server
        .mock("POST", api)
        .match_body(Matcher::Regex("Mailbox/query".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/query",
            {"accountId":"w","ids":[]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    server
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
    server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/query".into()))
        .with_body(
            json!({"methodResponses":[["Email/query",
            {"accountId":"w","ids":[]},"q"]]})
            .to_string(),
        )
        .expect(1)
        .create();
}

fn run_export(archive: &Path, base: &str, max_retries: u32) -> sync::Summary {
    sync::export::run(
        CommonConfig {
            archive: archive.to_path_buf(),
            threads: 1,
            dry_run: false,
            max_retries,
            allow_invalid_certs: false,
            logger: Logger::from_flags(true, 0),
        },
        ExportConfig {
            connect: ConnectConfig {
                url: base.to_owned(),
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
    .expect("export run")
}

fn email_counts(summary: &sync::Summary) -> sync::TypeCounts {
    summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "Email")
        .map(|(_, c)| c.clone())
        .expect("email counts")
}

fn method_error_body(error_type: &str) -> String {
    json!({"methodResponses":[["error",
        {"type": error_type,
         "description":"This server is temporarily unavailable. \
                        Attempting this same operation later may succeed."},
        "i"]]})
    .to_string()
}

#[test]
fn export_email_server_unavailable_is_retried_not_dropped() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";

    let archive = tmp();
    seed_single_email(&archive);
    mock_reconcile_for_single_email(&mut server, &base, api);

    let up = server
        .mock("POST", Matcher::Regex("/jmap/upload/".into()))
        .with_body(json!({"blobId":"UPLOADED"}).to_string())
        .expect(1)
        .create();
    let failing = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/import".into()))
        .with_body(method_error_body("serverUnavailable"))
        .expect(2)
        .create();
    let succeeding = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/import".into()))
        .with_body(
            json!({"methodResponses":[["Email/import",{"accountId":"w",
                "created":{"e1":{"id":"E1","blobId":"UPLOADED","threadId":"T1","size":48}}},"i"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let summary = run_export(&archive, &base, 3);
    let email = email_counts(&summary);

    failing.assert();
    succeeding.assert();
    up.assert();
    assert_eq!(
        email.created, 1,
        "the message must survive a transient error"
    );
    assert_eq!(email.failed, 0, "a retried message is not a failure");
    assert!(!summary.any_failed());

    let _ = std::fs::remove_file(&archive);
}

#[test]
fn export_email_exhausted_method_retries_stay_per_message() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";

    let archive = tmp();
    seed_single_email(&archive);
    mock_reconcile_for_single_email(&mut server, &base, api);

    let _up = server
        .mock("POST", Matcher::Regex("/jmap/upload/".into()))
        .with_body(json!({"blobId":"UPLOADED"}).to_string())
        .expect(1)
        .create();
    let failing = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/import".into()))
        .with_body(method_error_body("serverUnavailable"))
        .expect(3)
        .create();

    let summary = run_export(&archive, &base, 2);
    let email = email_counts(&summary);

    failing.assert();
    assert_eq!(email.created, 0);
    assert_eq!(
        email.failed, 1,
        "exhausting the retries stays a per-message failure, not a run abort"
    );
    assert!(summary.any_failed(), "the run must still exit 5");

    let _ = std::fs::remove_file(&archive);
}

#[test]
fn export_email_fatal_method_error_is_not_retried() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let api = "/jmap/api";

    let archive = tmp();
    seed_single_email(&archive);
    mock_reconcile_for_single_email(&mut server, &base, api);

    let _up = server
        .mock("POST", Matcher::Regex("/jmap/upload/".into()))
        .with_body(json!({"blobId":"UPLOADED"}).to_string())
        .expect(1)
        .create();
    let failing = server
        .mock("POST", api)
        .match_body(Matcher::Regex("Email/import".into()))
        .with_body(method_error_body("invalidArguments"))
        .expect(1)
        .create();

    let summary = run_export(&archive, &base, 5);
    let email = email_counts(&summary);

    failing.assert();
    assert_eq!(
        email.failed, 1,
        "a permanent method error must not consume retries"
    );

    let _ = std::fs::remove_file(&archive);
}

fn session_body_vacation(base: &str) -> String {
    let mut session: Value = serde_json::from_str(&session_body_full(base)).unwrap();
    session["accounts"]["w"]["accountCapabilities"]["urn:ietf:params:jmap:vacationresponse"] =
        json!({});
    session.to_string()
}

type CallLog = std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>;

fn jmap_dispatch(
    server: &mut mockito::Server,
    log: &CallLog,
    respond: impl Fn(&str, &Value) -> Value + Send + Sync + 'static,
) -> mockito::Mock {
    let log = log.clone();
    server
        .mock("POST", "/jmap/api")
        .with_body_from_request(move |req| {
            let body: Value = serde_json::from_slice(req.body().unwrap()).unwrap();
            let mut responses = Vec::new();
            for call in body["methodCalls"].as_array().unwrap() {
                let name = call[0].as_str().unwrap();
                log.lock().unwrap().push((name.to_owned(), call[1].clone()));
                responses.push(json!([name, respond(name, &call[1]), call[2]]));
            }
            json!({ "methodResponses": responses })
                .to_string()
                .into_bytes()
        })
        .expect_at_least(1)
        .create()
}

fn vacation_row(archive: &Path) -> Option<(bool, Option<String>, Option<String>)> {
    let conn = db::init::open(archive).unwrap();
    conn.query_row(
        "SELECT is_enabled, subject, from_date FROM vacation_response",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .ok()
}

// A 0.16 source keeps the vacation script and stores the VacationResponse; catch-up refreshes it.
#[test]
fn import_vacation_response_from_stalwart_016_source() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let archive = tmp();
    let vacation = std::sync::Arc::new(std::sync::Mutex::new(json!({
        "id": "singleton", "isEnabled": true, "fromDate": "2026-10-01T00:00:00Z",
        "toDate": null, "subject": "Away", "textBody": "Back soon", "htmlBody": null
    })));

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_vacation(&base))
        .expect_at_least(1)
        .create();
    let _dl1 = server
        .mock("GET", Matcher::Regex("/jmap/dl/w/B1/.*".into()))
        .with_body("keep;\n")
        .create();
    let _dl2 = server
        .mock("GET", Matcher::Regex("/jmap/dl/w/B2/.*".into()))
        .with_body("require [\"vacation\"];\nvacation \"Back soon\";\n")
        .create();
    let log = CallLog::default();
    let current = vacation.clone();
    let _api = jmap_dispatch(&mut server, &log, move |name, args| match name {
        "SieveScript/query" if args.get("anchor").is_some() => json!({"accountId":"w","ids":[]}),
        "SieveScript/query" => json!({"accountId":"w","ids":["S1","S2"]}),
        "SieveScript/get" => json!({"accountId":"w","state":"x","notFound":[],"list":[
            {"id":"S1","name":"filters","isActive":false,"blobId":"B1"},
            {"id":"S2","name":"vacation","isActive":true,"blobId":"B2"}
        ]}),
        "VacationResponse/get" => json!({"accountId":"w","state":"x","notFound":[],
            "list":[current.lock().unwrap().clone()]}),
        other => panic!("unexpected {other}"),
    });

    let summary = sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::SieveScript]),
    )
    .expect("import");
    assert!(!summary.any_failed());
    let counts = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "VacationResponse")
        .map(|(_, c)| c.clone())
        .expect("vacation counts");
    assert_eq!(counts.fetched, 1);
    assert_eq!(
        vacation_row(&archive),
        Some((
            true,
            Some("Away".to_owned()),
            Some("2026-10-01T00:00:00Z".to_owned())
        ))
    );
    let scripts: i64 = db::init::open(&archive)
        .unwrap()
        .query_row("SELECT count(*) FROM sieve_scripts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(scripts, 2, "the vacation script is still archived");

    *vacation.lock().unwrap() = json!({"id": "singleton", "isEnabled": false, "subject": "Later"});
    sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::SieveScript]),
    )
    .expect("catch-up import");
    assert_eq!(
        vacation_row(&archive),
        Some((false, Some("Later".to_owned()), None))
    );

    *vacation.lock().unwrap() = json!({"id": "singleton", "isEnabled": false});
    sync::import_jmap::run(
        common(&archive),
        import_cfg_objects(&base, vec![ObjectType::SieveScript]),
    )
    .expect("second catch-up import");
    assert_eq!(
        vacation_row(&archive),
        None,
        "an unset vacation clears the row"
    );
    let _ = std::fs::remove_file(&archive);
}

fn seed_vacation_archive(archive: &Path) {
    let conn = db::init::open(archive).unwrap();
    let filters = db::blobs::intern_blob(&conn, b"keep;\n").unwrap();
    let vacation = db::blobs::intern_blob(&conn, b"vacation \"Back soon\";\n").unwrap();
    conn.execute(
        "INSERT INTO sources (id,kind,session_url,account_id,username)
         VALUES (1,'jmap','http://src','a','bob')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sieve_scripts (id,name,is_active,blob_id) VALUES
            (1,'filters',0,?1), (2,'vacation',1,?2)",
        rusqlite::params![filters, vacation],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO vacation_response (id,source_id,is_enabled,subject,text_body)
         VALUES (1,1,1,'Away','Back soon')",
        [],
    )
    .unwrap();
}

// The vacation script becomes a VacationResponse update, sent last and never pruned.
#[test]
fn export_vacation_script_becomes_vacation_response() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let archive = tmp();
    seed_vacation_archive(&archive);

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_vacation(&base))
        .expect_at_least(1)
        .create();
    let _up = server
        .mock("POST", Matcher::Regex("/jmap/upload/".into()))
        .with_body(json!({"blobId":"UP1"}).to_string())
        .create();
    let log = CallLog::default();
    let second_run = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let target_state = second_run.clone();
    let _api = jmap_dispatch(&mut server, &log, move |name, args| match name {
        "SieveScript/get" if target_state.load(std::sync::atomic::Ordering::Relaxed) => {
            json!({"accountId":"w","state":"x","notFound":[],"list":[
                {"id":"V1","name":"vacation","isActive":true,"blobId":"BV"},
                {"id":"S1","name":"filters","isActive":false,"blobId":"UP1"}
            ]})
        }
        "SieveScript/get" => json!({"accountId":"w","state":"x","notFound":[],"list":[
            {"id":"O1","name":"old","isActive":false,"blobId":"BO"}
        ]}),
        "SieveScript/set" if args.get("create").is_some() => {
            json!({"accountId":"w","created":{"c1":{"id":"S1"}}})
        }
        "SieveScript/set" if args.get("destroy").is_some() => {
            json!({"accountId":"w","destroyed":args["destroy"].clone()})
        }
        "SieveScript/set" => json!({"accountId":"w"}),
        "VacationResponse/set" => json!({"accountId":"w","updated":{"singleton":null}}),
        other => panic!("unexpected {other}"),
    });
    let export = || {
        let mut config = export_cfg_objects(&base, vec![ObjectType::SieveScript]);
        config.prune = true;
        let summary = sync::export::run(common(&archive), config).expect("export");
        assert!(!summary.any_failed(), "{:?}", summary.per_type);
        let calls = std::mem::take(&mut *log.lock().unwrap());
        (summary, calls)
    };
    let args_of = |calls: &[(String, Value)], key: &str| -> Vec<Value> {
        calls
            .iter()
            .filter_map(|(_, args)| args.get(key).cloned())
            .collect()
    };
    let expected_vacation = json!({"isEnabled": true, "fromDate": null, "toDate": null,
        "subject": "Away", "textBody": "Back soon", "htmlBody": null});

    let (summary, calls) = export();
    let creates = args_of(&calls, "create");
    assert_eq!(creates.len(), 1);
    assert_eq!(creates[0]["c1"]["name"], json!("filters"));
    assert!(
        creates[0].get("c2").is_none(),
        "vacation is not created as a script"
    );
    assert_eq!(args_of(&calls, "destroy"), vec![json!(["O1"])]);
    let (last_name, last_args) = calls.last().unwrap();
    assert_eq!(last_name, "VacationResponse/set");
    assert_eq!(last_args["update"]["singleton"], expected_vacation);
    let counts = |ty: &str| {
        summary
            .per_type
            .iter()
            .find(|(t, _)| *t == ty)
            .map(|(_, c)| c.clone())
            .unwrap()
    };
    assert_eq!(counts("SieveScript").created, 1);
    assert_eq!(counts("SieveScript").skipped, 1);
    assert_eq!(counts("VacationResponse").updated, 1);

    second_run.store(true, std::sync::atomic::Ordering::Relaxed);
    let (_, calls) = export();
    assert!(args_of(&calls, "create").is_empty());
    assert!(
        args_of(&calls, "destroy").is_empty(),
        "the target vacation script is not pruned"
    );
    let (last_name, last_args) = calls.last().unwrap();
    assert_eq!(last_name, "VacationResponse/set");
    assert_eq!(last_args["update"]["singleton"], expected_vacation);
    let _ = std::fs::remove_file(&archive);
}

// Without VacationResponse on the target, the vacation script is exported as before.
#[test]
fn export_vacation_script_unchanged_without_target_capability() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let archive = tmp();
    seed_vacation_archive(&archive);

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_full(&base))
        .expect_at_least(1)
        .create();
    let _up = server
        .mock("POST", Matcher::Regex("/jmap/upload/".into()))
        .with_body(json!({"blobId":"UP1"}).to_string())
        .create();
    let log = CallLog::default();
    let _api = jmap_dispatch(&mut server, &log, |name, args| match name {
        "SieveScript/get" => json!({"accountId":"w","state":"x","notFound":[],"list":[]}),
        "SieveScript/set" if args.get("create").is_some() => {
            let id = args["create"]
                .as_object()
                .unwrap()
                .keys()
                .next()
                .unwrap()
                .clone();
            json!({"accountId":"w","created":{ id.clone(): {"id": format!("T{id}")} }})
        }
        "SieveScript/set" => json!({"accountId":"w"}),
        other => panic!("unexpected {other}"),
    });

    let summary = sync::export::run(
        common(&archive),
        export_cfg_objects(&base, vec![ObjectType::SieveScript]),
    )
    .expect("export");
    assert!(!summary.any_failed(), "{:?}", summary.per_type);
    let calls = log.lock().unwrap().clone();
    let names: Vec<&Value> = calls
        .iter()
        .filter_map(|(_, args)| args.get("create"))
        .flat_map(|c| c.as_object().unwrap().values().map(|v| &v["name"]))
        .collect();
    assert_eq!(names, vec![&json!("filters"), &json!("vacation")]);
    assert!(
        calls
            .iter()
            .all(|(n, _)| !n.starts_with("VacationResponse"))
    );
    assert!(
        summary
            .per_type
            .iter()
            .all(|(t, _)| *t != "VacationResponse")
    );
    let _ = std::fs::remove_file(&archive);
}

fn session_body_admin_vacation(base: &str) -> String {
    let mut session: Value = serde_json::from_str(&session_body_full(base)).unwrap();
    let caps = &mut session["capabilities"];
    caps["urn:ietf:params:jmap:principals"] = json!({});
    caps["urn:ietf:params:jmap:sieve"] = json!({});
    caps["urn:ietf:params:jmap:vacationresponse"] = json!({});
    let mut admin = session["accounts"]["w"].take();
    admin["name"] = json!("admin");
    session["accounts"] = json!({ "a": admin });
    session.to_string()
}

fn principal_bob(name: &str) -> Option<Value> {
    match name {
        "Principal/query" => Some(json!({"accountId":"a","ids":["p2"]})),
        "Principal/get" => Some(json!({"accountId":"a","state":"x","notFound":[],"list":[
            {"id":"p2","name":"bob","accounts":{"w":{"name":"bob"}}}
        ]})),
        _ => None,
    }
}

// Admin credentials with --account-name: the target is not listed in the session, the session advertises VacationResponse.
#[test]
fn export_vacation_response_for_unlisted_account_of_admin_session() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let archive = tmp();
    seed_vacation_archive(&archive);

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_admin_vacation(&base))
        .expect_at_least(1)
        .create();
    let _up = server
        .mock("POST", Matcher::Regex("/jmap/upload/w/".into()))
        .with_body(json!({"blobId":"UP1"}).to_string())
        .create();
    let log = CallLog::default();
    let _api = jmap_dispatch(&mut server, &log, |name, args| {
        if let Some(reply) = principal_bob(name) {
            return reply;
        }
        match name {
            "SieveScript/get" => json!({"accountId":"w","state":"x","notFound":[],"list":[]}),
            "SieveScript/set" if args.get("create").is_some() => {
                json!({"accountId":"w","created":{"c1":{"id":"S1"}}})
            }
            "SieveScript/set" => json!({"accountId":"w"}),
            "VacationResponse/set" => json!({"accountId":"w","updated":{"singleton":null}}),
            other => panic!("unexpected {other}"),
        }
    });

    let mut config = export_cfg_objects(&base, vec![ObjectType::SieveScript]);
    config.connect.account = AccountSelector::Name("bob".into());
    let summary = sync::export::run(common(&archive), config).expect("export");
    assert!(!summary.any_failed(), "{:?}", summary.per_type);
    let calls = log.lock().unwrap().clone();
    let names: Vec<&Value> = calls
        .iter()
        .filter_map(|(_, args)| args.get("create"))
        .flat_map(|c| c.as_object().unwrap().values().map(|v| &v["name"]))
        .collect();
    assert_eq!(names, vec![&json!("filters")], "vacation is not created");
    let (last_name, last_args) = calls.last().unwrap();
    assert_eq!(last_name, "VacationResponse/set");
    assert_eq!(last_args["accountId"], json!("w"));
    assert_eq!(last_args["update"]["singleton"]["subject"], json!("Away"));
    let _ = std::fs::remove_file(&archive);
}

// Admin credentials with --account-name: the import stores the target account's VacationResponse.
#[test]
fn import_vacation_response_for_unlisted_account_of_admin_session() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let archive = tmp();

    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_body(session_body_admin_vacation(&base))
        .expect_at_least(1)
        .create();
    let _dl = server
        .mock("GET", Matcher::Regex("/jmap/dl/w/B1/.*".into()))
        .with_body("require [\"vacation\"];\nvacation \"Back soon\";\n")
        .create();
    let log = CallLog::default();
    let _api = jmap_dispatch(&mut server, &log, |name, args| {
        if let Some(reply) = principal_bob(name) {
            return reply;
        }
        match name {
            "SieveScript/query" if args.get("anchor").is_some() => {
                json!({"accountId":"w","ids":[]})
            }
            "SieveScript/query" => json!({"accountId":"w","ids":["S1"]}),
            "SieveScript/get" => json!({"accountId":"w","state":"x","notFound":[],"list":[
                {"id":"S1","name":"vacation","isActive":true,"blobId":"B1"}
            ]}),
            "VacationResponse/get" => json!({"accountId":"w","state":"x","notFound":[],"list":[
                {"id":"singleton","isEnabled":true,"fromDate":null,"toDate":null,
                 "subject":"Away","textBody":"Back soon","htmlBody":null}
            ]}),
            other => panic!("unexpected {other}"),
        }
    });

    let mut config = import_cfg_objects(&base, vec![ObjectType::SieveScript]);
    config.connect.account = AccountSelector::Name("bob".into());
    let summary = sync::import_jmap::run(common(&archive), config).expect("import");
    assert!(!summary.any_failed(), "{:?}", summary.per_type);
    let fetched = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "VacationResponse")
        .map(|(_, c)| c.fetched);
    assert_eq!(fetched, Some(1));
    assert_eq!(
        vacation_row(&archive),
        Some((true, Some("Away".to_owned()), None))
    );
    assert!(
        log.lock()
            .unwrap()
            .iter()
            .any(|(n, args)| n == "VacationResponse/get" && args["accountId"] == json!("w"))
    );
    let _ = std::fs::remove_file(&archive);
}
