/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod integration;
mod seeder;

use std::path::PathBuf;

use integration::stalwart::shared as shared_stalwart;
use mockito::{Matcher, Server};
use serde_json::{Value, json};
use vandelay::exchange_graph::types::{EventBodyFormat, MailboxKind, Surfaces};
use vandelay::jmap::account::AccountSelector;
use vandelay::jmap::http::Auth;
use vandelay::logging::Logger;
use vandelay::sync::import_exchange_graph::{GraphAuth, GraphImportConfig};
use vandelay::sync::{self, CommonConfig, ConnectConfig, ExportConfig};

fn base_url() -> &'static str {
    shared_stalwart().base_url()
}

fn tmp_archive(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "vandelay-{tag}-{}-{}.sqlite",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

fn json_mock(server: &mut Server, path: &str, body: &str) {
    server
        .mock("GET", path)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(body)
        .expect_at_least(0)
        .create();
}

fn graph_fixture(server: &mut Server) {
    server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"uid","userPrincipalName":"alice@x.com"}"#)
        .expect_at_least(0)
        .create();
    json_mock(
        server,
        "/me/mailboxSettings?$select=timeZone",
        r#"{"timeZone":"UTC"}"#,
    );

    json_mock(
        server,
        "/me/mailFolders?$top=100&includeHiddenFolders=true",
        r#"{"value":[{"id":"F1","displayName":"Graph Inbox","isHidden":false}]}"#,
    );
    json_mock(
        server,
        "/me/mailFolders/F1/childFolders?$top=100&includeHiddenFolders=true",
        r#"{"value":[]}"#,
    );
    for name in [
        "inbox",
        "drafts",
        "sentitems",
        "deleteditems",
        "junkemail",
        "archive",
    ] {
        server
            .mock("GET", format!("/me/mailFolders/{name}?$select=id").as_str())
            .with_status(404)
            .expect_at_least(0)
            .create();
    }
    json_mock(
        server,
        "/me/mailFolders/F1/messages?$top=100&$select=id,isRead,isDraft,isReadReceiptRequested,flag,categories",
        r#"{"value":[{"id":"M1","isRead":true,"isDraft":false,
                      "flag":{"flagStatus":"flagged"},"categories":["Work"]}]}"#,
    );
    server
        .mock("GET", "/me/messages/M1/$value")
        .with_status(200)
        .with_header("content-type", "text/plain")
        .with_body(
            "From: art@vandelay.example\r\nTo: alice@x.com\r\n\
             Subject: Graph round trip\r\nDate: Tue, 01 Sep 2026 10:00:00 +0000\r\n\
             Message-ID: <graph-rt-1@vandelay.example>\r\n\
             MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n\
             Grüße aus Köln 🎉\r\n",
        )
        .expect_at_least(0)
        .create();

    json_mock(
        server,
        "/me/calendars?$top=100",
        r##"{"value":[{"id":"CAL1","name":"Graph Calendar","isDefaultCalendar":false,
                      "hexColor":"#FF0000"}]}"##,
    );
    json_mock(
        server,
        "/me/calendars/CAL1/events?$top=100&$select=id,type,seriesMasterId",
        r#"{"value":[
            {"id":"EV","type":"singleInstance","iCalUId":"uid-ev"},
            {"id":"MASTER","type":"seriesMaster","iCalUId":"uid-master"}
        ]}"#,
    );
    json_mock(
        server,
        "/me/events/EV",
        r#"{"id":"EV","iCalUId":"uid-ev","type":"singleInstance",
            "subject":"Event with an enclosure","hasAttachments":true,
            "start":{"dateTime":"2026-05-04T15:00:00.0000000","timeZone":"UTC"},
            "end":{"dateTime":"2026-05-04T16:00:00.0000000","timeZone":"UTC"}}"#,
    );
    server
        .mock("GET", "/me/events/EV/attachments")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r##"{"value":[{"@odata.type":"#microsoft.graph.fileAttachment","id":"A1",
                 "name":"agenda.txt","contentType":"text/plain","size":5,
                 "contentBytes":"aGVsbG8="}]}"##,
        )
        .expect_at_least(0)
        .create();

    let start = (chrono::Utc::now().date_naive() + chrono::Duration::days(10))
        .format("%Y-%m-%d")
        .to_string();
    let d = |n: i64| {
        (chrono::Utc::now().date_naive() + chrono::Duration::days(10 + n))
            .format("%Y-%m-%d")
            .to_string()
    };
    json_mock(
        server,
        "/me/events/MASTER",
        &format!(
            r#"{{"id":"MASTER","iCalUId":"uid-master","type":"seriesMaster",
                 "subject":"Daily standup","hasAttachments":false,
                 "start":{{"dateTime":"{start}T09:00:00.0000000","timeZone":"UTC"}},
                 "end":{{"dateTime":"{start}T09:30:00.0000000","timeZone":"UTC"}},
                 "recurrence":{{"pattern":{{"type":"daily","interval":1}},
                   "range":{{"type":"numbered","startDate":"{start}",
                             "numberOfOccurrences":4}}}}}}"#
        ),
    );
    server
        .mock(
            "GET",
            Matcher::Regex(r"calendarView.*type%20eq%20%27exception%27".to_owned()),
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(format!(
            r#"{{"value":[{{"id":"EX","iCalUId":"uid-master","type":"exception",
                 "seriesMasterId":"MASTER","originalStart":"{}T09:00:00Z",
                 "subject":"Moved standup",
                 "start":{{"dateTime":"{}T14:00:00.0000000","timeZone":"UTC"}},
                 "end":{{"dateTime":"{}T14:30:00.0000000","timeZone":"UTC"}}}}]}}"#,
            d(1),
            d(1),
            d(1)
        ))
        .expect_at_least(1)
        .create();
    server
        .mock(
            "GET",
            Matcher::Regex(r"calendarView.*type%20eq%20%27occurrence%27".to_owned()),
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(format!(
            r#"{{"value":[
                {{"id":"O1","type":"occurrence","seriesMasterId":"MASTER",
                  "start":{{"dateTime":"{}T09:00:00.0000000","timeZone":"UTC"}}}},
                {{"id":"O4","type":"occurrence","seriesMasterId":"MASTER",
                  "start":{{"dateTime":"{}T09:00:00.0000000","timeZone":"UTC"}}}}
            ]}}"#,
            d(0),
            d(3)
        ))
        .expect_at_least(1)
        .create();

    json_mock(server, "/me/contactFolders?$top=100", r#"{"value":[]}"#);
    json_mock(
        server,
        "/me/contactFolders/contacts",
        r#"{"id":"DEFAULT","displayName":"Contacts","wellKnownName":"contacts"}"#,
    );
    json_mock(
        server,
        "/me/contactFolders/DEFAULT/childFolders?$top=100",
        r#"{"value":[]}"#,
    );
    json_mock(
        server,
        "/me/contactFolders/DEFAULT/contacts?$top=100&$select=id",
        r#"{"value":[{"id":"C1"}]}"#,
    );
    json_mock(
        server,
        "/me/contacts/C1",
        r#"{"id":"C1","displayName":"Graph Contact","givenName":"Graph","surname":"Contact",
            "categories":["Work","VIP"],"imAddresses":["sip:graph@vandelay.example"],
            "emailAddresses":[{"address":"graph@vandelay.example"}]}"#,
    );
    server
        .mock("GET", "/me/contacts/C1/photo/$value")
        .with_status(200)
        .with_header("content-type", "image/jpeg")
        .with_body(
            [
                0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49,
                0x48, 0x44, 0x52,
            ]
            .as_slice(),
        )
        .expect_at_least(1)
        .create();

    json_mock(server, "/me/drive/root?$select=id,name", r#"{"id":"ROOT"}"#);
    let sel = "id,name,size,folder,file,package,remoteItem,createdDateTime,lastModifiedDateTime";
    json_mock(
        server,
        &format!("/me/drive/items/ROOT/children?$top=100&$select={sel}"),
        r#"{"value":[
            {"id":"D1","name":"Graph Files","folder":{"childCount":1},
             "createdDateTime":"2026-01-01T00:00:00Z"},
            {"id":"V1","name":"Personal Vault","remoteItem":{},
             "createdDateTime":"2026-01-01T00:00:00Z"}
        ]}"#,
    );
    json_mock(
        server,
        &format!("/me/drive/items/D1/children?$top=100&$select={sel}"),
        r#"{"value":[{"id":"FL1","name":"report.txt","size":6,
                      "file":{"mimeType":"text/plain"},
                      "createdDateTime":"2026-01-01T00:00:00Z"}]}"#,
    );
    server
        .mock("GET", "/me/drive/items/FL1/content")
        .with_status(200)
        .with_header("content-type", "text/plain")
        .with_body("hello\n")
        .expect_at_least(0)
        .create();
}

#[test]
#[ignore = "requires Docker"]
fn graph_import_survives_a_full_export_to_stalwart() {
    let mut graph = Server::new();
    graph_fixture(&mut graph);
    let archive = tmp_archive("graph-roundtrip");

    let import = sync::import_exchange_graph::run(
        CommonConfig {
            archive: archive.clone(),
            threads: 4,
            dry_run: false,
            max_retries: 2,
            allow_invalid_certs: true,
            logger: Logger::from_flags(true, 0),
        },
        GraphImportConfig {
            auth: GraphAuth::PreAcquired {
                token: "TOKEN".to_owned(),
            },
            api_base: graph.url(),
            user_target: None,
            mailbox_kind: MailboxKind::Primary,
            surfaces: Surfaces::ALL,
            event_body_format: EventBodyFormat::Text,
            graph_connections: 2,
            top: 100,
            exception_window_years: 5,
            contact_photos: true,
            event_attachments: true,
            allow_source_change: false,
        },
    )
    .expect("graph import");
    assert!(!import.any_failed(), "import reported failures");

    let fx = seeder::provision(base_url()).expect("provision");
    assert_eq!(fx.domain, seeder::DOMAIN);
    assert!(!fx.domain_id.is_empty(), "domain id resolved");
    assert_eq!(
        fx.admin_login,
        (
            seeder::ADMIN_USER.to_owned(),
            seeder::ADMIN_PASSWORD.to_owned()
        )
    );
    let reference = fx
        .account(seeder::SYNC_IN[0])
        .expect("a seeded reference account");
    let seeded = reference.seeded.as_ref().expect("reference seed stats");
    assert!(seeded.emails > 0, "provisioning seeded mail");
    assert!(
        seeded.mailboxes_created > 0,
        "provisioning seeded a mailbox tree"
    );
    assert!(seeded.file_nodes > 0, "provisioning seeded file nodes");
    assert!(seeded.contacts > 0, "provisioning seeded contacts");
    assert!(seeded.events > 0, "provisioning seeded events");
    assert!(
        seeded.address_books > 0,
        "provisioning seeded address books"
    );
    assert!(seeded.calendars > 0, "provisioning seeded calendars");
    assert!(seeded.identity, "provisioning seeded an identity");
    assert!(
        seeded.sieve_active.is_some(),
        "provisioning seeded a sieve script"
    );

    let account = fx.account(seeder::SYNC_OUT[0]).expect("a sync-out target");
    assert!(!account.admin_role, "the export target is a regular user");
    assert!(
        account.seeded.is_none(),
        "the export target must start empty so every assertion below is ours"
    );

    let export = sync::export::run(
        CommonConfig {
            archive: archive.clone(),
            threads: 4,
            dry_run: false,
            max_retries: 5,
            allow_invalid_certs: true,
            logger: Logger::from_flags(true, 0),
        },
        ExportConfig {
            connect: ConnectConfig {
                url: base_url().to_owned(),
                auth: Auth::Basic {
                    user: account.email.clone(),
                    password: account.password.clone(),
                },
                account: AccountSelector::Id(account.account_id.clone()),
            },
            objects: None,
            prune: false,
            yes: true,
        },
    )
    .expect("export");
    assert!(!export.any_failed(), "export reported failures");

    let jmap = seeder::jmap::Jmap::connect(&fx.base_url, &account.email, &account.password)
        .expect("session");
    let acc = account.account_id.clone();

    let cards = query_all(&jmap, &acc, "urn:ietf:params:jmap:contacts", "ContactCard");
    let card = cards
        .iter()
        .find(|c| c["name"]["full"] == "Graph Contact")
        .expect("the imported card reached the target");

    let uri = card["media"]
        .as_object()
        .and_then(|m| m.values().next())
        .and_then(|m| m["uri"].as_str())
        .expect("the contact photo survived as a Media resource");
    assert!(
        uri.starts_with("data:image/png;base64,"),
        "Graph reports image/jpeg for every contact photo, so the media type must come \
         from the bytes; got {}",
        &uri[..uri.len().min(40)]
    );
    assert_eq!(card["keywords"]["work"], true, "categories became keywords");
    assert_eq!(
        card["onlineServices"]
            .as_object()
            .and_then(|m| m.values().next())
            .and_then(|s| s["uri"].as_str()),
        Some("sip:graph@vandelay.example"),
        "a URI-shaped IM address must land in uri: Stalwart drops an OnlineService \
         that carries only user, even though RFC 9553 permits it"
    );

    let events = query_all(
        &jmap,
        &acc,
        "urn:ietf:params:jmap:calendars",
        "CalendarEvent",
    );
    let enclosure = events
        .iter()
        .find(|e| e["title"] == "Event with an enclosure")
        .expect("the event with an attachment reached the target");
    let href = enclosure["links"]
        .as_object()
        .and_then(|m| m.values().next())
        .and_then(|l| l["href"].as_str())
        .expect("the attachment survived as a Link");
    assert!(
        href.starts_with("data:text/plain;base64,"),
        "got {}",
        &href[..href.len().min(40)]
    );

    let series = events
        .iter()
        .find(|e| e["title"] == "Daily standup")
        .expect("the series reached the target");
    let overrides = series["recurrenceOverrides"]
        .as_object()
        .expect("recurrenceOverrides survived");
    assert_eq!(
        overrides.len(),
        2,
        "one modified occurrence and one deleted one; got {:?}",
        overrides.keys().collect::<Vec<_>>()
    );
    assert!(
        overrides
            .values()
            .any(|v| v["excluded"] == json!(true) && v.as_object().unwrap().len() == 1),
        "the deleted occurrence is a single-member excluded patch \
         (jscalendarbis 3.3.4); got {overrides:?}"
    );
    assert!(
        overrides.values().any(|v| v["title"] == "Moved standup"),
        "the modified occurrence kept its patch; got {overrides:?}"
    );

    let files = query_all(&jmap, &acc, "urn:ietf:params:jmap:filenode", "FileNode");
    assert!(
        files
            .iter()
            .any(|f| f["name"] == "report.txt" && f["blobId"].is_string()),
        "the drive file reached the target with a blob"
    );
    assert!(
        !files.iter().any(|f| f["name"] == "Personal Vault"),
        "a facetless drive item must never reach the target"
    );

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

fn query_all(jmap: &seeder::jmap::Jmap, account: &str, using: &str, ty: &str) -> Vec<Value> {
    let resp = jmap
        .request(
            &["urn:ietf:params:jmap:core", using],
            json!([
                [format!("{ty}/query"), {"accountId": account}, "q"],
                [
                    format!("{ty}/get"),
                    {
                        "accountId": account,
                        "#ids": {"resultOf": "q", "name": format!("{ty}/query"), "path": "/ids"}
                    },
                    "g"
                ]
            ]),
        )
        .expect("query and get");
    resp["methodResponses"]
        .as_array()
        .and_then(|calls| calls.iter().find(|c| c[0] == format!("{ty}/get")))
        .and_then(|c| c[1]["list"].as_array())
        .cloned()
        .unwrap_or_default()
}
