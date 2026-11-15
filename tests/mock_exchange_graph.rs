/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::time::Duration;

use encodify::base64::URL_SAFE_NO_PAD;
use mockito::{Matcher, Server};
use serde_json::json;
use vandelay::archive::Archive;
use vandelay::archive::sources::{SourceChange, SourceKey, SourceKind};
use vandelay::exchange::graph::api::Endpoints;
use vandelay::exchange::graph::calendar_map::{EventType, GraphEvent};
use vandelay::exchange::graph::client::{Accept, GraphClient};
use vandelay::exchange::graph::contact_map::GraphContact;
use vandelay::exchange::graph::error::GraphError;
use vandelay::exchange::graph::recurrence::GraphRecurrence;
use vandelay::exchange::graph::retry::HttpClass;
use vandelay::exchange::graph::types::Surfaces;
use vandelay::exchange::oauth::{DeviceCodeFlow, DeviceCodeResponse, Dialect, Pace, TokenResponse};
use vandelay::logging::{Level, Logger};
use vandelay::net::http::{ClientOptions, TlsPolicy};
use vandelay::net::retry::RetryPolicy;
use vandelay::sync::{RunMode, SummaryType};
use vandelay::types::ObjectType;

fn client_with_retries(retries: u32) -> GraphClient {
    GraphClient::new(
        "BEARER".to_owned(),
        ClientOptions {
            retry: RetryPolicy::new(retries),
            tls: TlsPolicy::Verify,
            logger: Logger::new(Level::Quiet),
        },
    )
}

fn url_message_collection(server_url: &str, folder: &str, top: usize) -> String {
    let e = Endpoints::for_me(server_url);
    e.folder_messages_ids(folder, top)
}

fn make_jwt(exp: u64, upn: &str) -> String {
    let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
    let claims = format!(r#"{{"tid":"tenant-1","upn":"{upn}","exp":{exp}}}"#);
    let payload = URL_SAFE_NO_PAD.encode(claims.as_bytes());
    format!("{header}.{payload}.")
}

#[test]
fn http_status_classifier_matches_spec_table() {
    assert_eq!(HttpClass::of_status(200), HttpClass::Success);
    assert_eq!(HttpClass::of_status(401), HttpClass::Auth);
    assert_eq!(HttpClass::of_status(403), HttpClass::Auth);
    assert_eq!(HttpClass::of_status(404), HttpClass::Vanished);
    assert_eq!(HttpClass::of_status(410), HttpClass::Vanished);
    assert_eq!(HttpClass::of_status(429), HttpClass::Retryable);
    assert_eq!(HttpClass::of_status(503), HttpClass::Retryable);
    assert_eq!(HttpClass::of_status(504), HttpClass::Retryable);
    assert_eq!(HttpClass::of_status(507), HttpClass::Fatal);
    assert_eq!(HttpClass::of_status(400), HttpClass::Fatal);
    assert_eq!(HttpClass::of_status(405), HttpClass::Fatal);
    assert_eq!(HttpClass::of_status(422), HttpClass::Fatal);
}

#[test]
fn device_code_response_parses_required_fields() {
    let body = json!({
        "device_code": "ABCDEF",
        "user_code": "BR4S-7XYZ",
        "verification_uri": "https://microsoft.com/devicelogin",
        "interval": 5,
        "expires_in": 900,
        "message": "Open the URL and enter the code."
    });
    let parsed = DeviceCodeResponse::parse(&body).unwrap();
    assert_eq!(parsed.device_code, "ABCDEF");
    assert_eq!(parsed.user_code, "BR4S-7XYZ");
    assert_eq!(parsed.interval, 5);
    assert_eq!(parsed.expires_in, 900);
}

#[test]
fn token_response_handles_pending_and_success() {
    let pending = TokenResponse::parse(400, &json!({"error": "authorization_pending"}));
    assert!(matches!(pending, TokenResponse::Pending));
    let success = TokenResponse::parse(
        200,
        &json!({"access_token": make_jwt(9999999999, "alice@x.com"), "expires_in": 3599}),
    );
    match success {
        TokenResponse::Ok(t) => {
            assert!(t.access_token.contains('.'));
            assert_eq!(t.expires_in, Some(3599));
            assert_eq!(t.upn.as_deref(), Some("alice@x.com"));
        }
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn token_response_declined_and_bad_code() {
    let declined = TokenResponse::parse(400, &json!({"error": "authorization_declined"}));
    assert!(matches!(declined, TokenResponse::Declined));
    let bad = TokenResponse::parse(400, &json!({"error": "bad_verification_code"}));
    assert!(matches!(bad, TokenResponse::BadCode));
}

#[test]
fn device_code_polling_loop_converges_on_success() {
    let response = DeviceCodeResponse {
        device_code: "DC".to_owned(),
        user_code: "U".to_owned(),
        verification_uri: "x".to_owned(),
        interval: 0,
        expires_in: 5,
        message: None,
    };
    let mut attempt = 0;
    let token = DeviceCodeFlow {
        response: &response,
        client_id: "client-id",
        token_endpoint: "https://login.x/common/oauth2/v2.0/token",
        pace: Pace {
            interval: Duration::from_millis(20),
            slow_down: Duration::from_millis(20),
        },
    }
    .poll(|_endpoint, _form| {
        attempt += 1;
        if attempt < 3 {
            Ok((400, json!({"error": "authorization_pending"})))
        } else {
            Ok((
                200,
                json!({
                    "access_token": make_jwt(9999999999, "alice@x"),
                    "expires_in": 3599
                }),
            ))
        }
    })
    .unwrap();
    assert_eq!(attempt, 3);
    assert_eq!(token.upn.as_deref(), Some("alice@x"));
}

#[test]
fn device_code_polling_loop_aborts_on_decline() {
    let response = DeviceCodeResponse {
        device_code: "DC".to_owned(),
        user_code: "U".to_owned(),
        verification_uri: "x".to_owned(),
        interval: 0,
        expires_in: 5,
        message: None,
    };
    let err = DeviceCodeFlow {
        response: &response,
        client_id: "client-id",
        token_endpoint: "https://login.x/common/oauth2/v2.0/token",
        pace: Pace {
            interval: Duration::from_millis(20),
            slow_down: Duration::from_millis(20),
        },
    }
    .poll(|_, _| Ok((400, json!({"error": "authorization_declined"}))))
    .unwrap_err();
    let msg = Dialect::Graph.poll_error(err).to_string();
    assert!(msg.contains("declined"));
}

#[test]
fn pagination_collects_ids_across_three_pages_with_one_empty() {
    let mut server = Server::new();
    let base = server.url();
    let next2 = format!("{base}/page2");
    let next3 = format!("{base}/page3");

    let page1 = json!({
        "value": [
            {"id": "M1"},
            {"id": "M2"}
        ],
        "@odata.nextLink": next2
    })
    .to_string();
    let page2 = json!({
        "value": [],
        "@odata.nextLink": next3
    })
    .to_string();
    let page3 = json!({
        "value": [
            {"id": "M3"}
        ]
    })
    .to_string();

    let _m1 = server
        .mock(
            "GET",
            "/me/mailFolders/F1/messages?$top=100&$select=id,isRead,isDraft,flag,categories",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(page1)
        .create();
    let _m2 = server
        .mock("GET", "/page2")
        .match_header("authorization", "Bearer BEARER")
        .match_header("prefer", "IdType=\"ImmutableId\"")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(page2)
        .create();
    let _m3 = server
        .mock("GET", "/page3")
        .match_header("authorization", "Bearer BEARER")
        .match_header("prefer", "IdType=\"ImmutableId\"")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(page3)
        .create();

    let client = client_with_retries(0);
    let url = url_message_collection(&base, "F1", 100);
    let ids = client.collect_all_ids(&url, &[]).unwrap();
    assert_eq!(ids, vec!["M1".to_owned(), "M2".to_owned(), "M3".to_owned()]);
}

#[test]
fn pagination_terminates_when_no_next_link_present() {
    let mut server = Server::new();
    let base = server.url();
    let body = json!({"value": [{"id": "A"}, {"id": "B"}]}).to_string();
    let _m = server
        .mock(
            "GET",
            "/me/mailFolders/F/messages?$top=100&$select=id,isRead,isDraft,flag,categories",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(body)
        .create();
    let client = client_with_retries(0);
    let url = url_message_collection(&base, "F", 100);
    let mut pages = 0;
    client
        .paged_collect(&url, &[], |_| {
            pages += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(pages, 1);
}

#[test]
fn header_redelivery_is_asserted_via_mockito_match() {
    let mut server = Server::new();
    let base = server.url();
    let next = format!("{base}/page-two-different-segment");
    let page1 = json!({
        "value": [{"id": "OnlyOnPageOne"}],
        "@odata.nextLink": next
    })
    .to_string();
    let page2 = json!({"value": [{"id": "OnlyOnPageTwo"}]}).to_string();
    let _m1 = server
        .mock(
            "GET",
            "/me/mailFolders/F/messages?$top=100&$select=id,isRead,isDraft,flag,categories",
        )
        .match_header("prefer", "IdType=\"ImmutableId\"")
        .match_header("authorization", "Bearer BEARER")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(page1)
        .create();
    let _m2 = server
        .mock("GET", "/page-two-different-segment")
        .match_header("prefer", "IdType=\"ImmutableId\"")
        .match_header("authorization", "Bearer BEARER")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(page2)
        .create();
    let c = client_with_retries(0);
    let url = url_message_collection(&base, "F", 100);
    let ids = c.collect_all_ids(&url, &[]).unwrap();
    assert_eq!(ids.len(), 2);
}

#[test]
fn case_sensitive_immutable_id_round_trip() {
    let mut server = Server::new();
    let id = "AAkAAGFsaWNlAAA=";
    let body = json!({"value": [{"id": id}]}).to_string();
    let _m = server
        .mock(
            "GET",
            "/me/mailFolders/F/messages?$top=100&$select=id,isRead,isDraft,flag,categories",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(body)
        .create();
    let base = server.url();
    let client = client_with_retries(0);
    let url = url_message_collection(&base, "F", 100);
    let ids = client.collect_all_ids(&url, &[]).unwrap();
    assert_eq!(ids[0], id);
    assert!(ids[0].contains('='));
}

#[test]
fn rate_limit_with_retry_after_is_honoured_then_succeeds() {
    let mut server = Server::new();
    let path = "/me?$select=id,userPrincipalName,displayName,mail";
    let _m1 = server
        .mock("GET", path)
        .with_status(429)
        .with_header("retry-after", "1")
        .with_header("content-type", "application/json")
        .with_body(r#"{"error":{"code":"TooManyRequests"}}"#)
        .expect(1)
        .create();
    let _m2 = server
        .mock("GET", path)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"uid","userPrincipalName":"alice@x.com"}"#)
        .expect(1)
        .create();
    let base = server.url();
    let client = client_with_retries(3);
    let started = std::time::Instant::now();
    let resp = client
        .get(
            &format!("{base}/me?$select=id,userPrincipalName,displayName,mail"),
            Accept::Json,
        )
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(resp.status, 200);
    assert!(elapsed >= std::time::Duration::from_millis(800));
    assert!(client.stats().retries_observed >= 1);
    assert!(client.stats().retry_after_sleeps >= 1);
}

#[test]
fn rate_limit_without_retry_after_uses_shared_escalation() {
    let mut server = Server::new();
    let path = "/me?$select=id,userPrincipalName,displayName,mail";
    let _m1 = server
        .mock("GET", path)
        .with_status(429)
        .with_header("content-type", "application/json")
        .with_body(r#"{"error":{"code":"TooManyRequests"}}"#)
        .expect(1)
        .create();
    let _m2 = server
        .mock("GET", path)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"uid","userPrincipalName":"alice@x.com"}"#)
        .expect(1)
        .create();
    let base = server.url();
    let client = client_with_retries(3);
    let r = client
        .get(
            &format!("{base}/me?$select=id,userPrincipalName,displayName,mail"),
            Accept::Json,
        )
        .unwrap();
    assert_eq!(r.status, 200);
    assert!(client.stats().retries_observed >= 1);
}

#[test]
fn service_unavailable_503_is_retried() {
    let mut server = Server::new();
    let _m1 = server
        .mock("GET", "/me")
        .with_status(503)
        .expect(1)
        .create();
    let _m2 = server
        .mock("GET", "/me")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"u","userPrincipalName":"a@b"}"#)
        .expect(1)
        .create();
    let base = server.url();
    let c = client_with_retries(3);
    let r = c.get(&format!("{base}/me"), Accept::Json).unwrap();
    assert_eq!(r.status, 200);
}

#[test]
fn http_404_on_per_item_get_is_treated_as_vanished() {
    let mut server = Server::new();
    let _m = server
        .mock("GET", "/me/messages/GONE/$value")
        .with_status(404)
        .with_body("gone")
        .create();
    let base = server.url();
    let c = client_with_retries(0);
    let url = format!("{base}/me/messages/GONE/$value");
    let err = c.get(&url, Accept::Text).unwrap_err();
    assert!(matches!(err, GraphError::Vanished));
}

#[test]
fn http_401_is_auth_error() {
    let mut server = Server::new();
    let _m = server
        .mock("GET", "/me")
        .with_status(401)
        .with_body("unauthenticated")
        .create();
    let base = server.url();
    let c = client_with_retries(0);
    let url = format!("{base}/me");
    let err = c.get(&url, Accept::Json).unwrap_err();
    assert!(matches!(err, GraphError::Auth(_)));
}

#[test]
fn malformed_4xx_is_fatal() {
    let mut server = Server::new();
    let _m = server
        .mock("GET", "/me")
        .with_status(422)
        .with_body("malformed payload")
        .create();
    let base = server.url();
    let c = client_with_retries(0);
    let err = c.get(&format!("{base}/me"), Accept::Json).unwrap_err();
    assert!(matches!(err, GraphError::HttpStatus { status: 422, .. }));
}

#[test]
fn mime_value_endpoint_returns_raw_rfc5322_bytes() {
    let mut server = Server::new();
    let mime = "From: a@x\r\nTo: b@x\r\nSubject: hi\r\nDate: Tue, 27 May 2026 10:00:00 +0000\r\nMessage-ID: <abc@x>\r\n\r\nbody";
    let _m = server
        .mock("GET", "/me/messages/MID/$value")
        .match_header("accept", "text/plain")
        .match_header("authorization", "Bearer BEARER")
        .match_header("prefer", "IdType=\"ImmutableId\"")
        .with_status(200)
        .with_header("content-type", "text/plain")
        .with_body(mime)
        .create();
    let base = server.url();
    let c = client_with_retries(0);
    let url = format!("{base}/me/messages/MID/$value");
    let resp = c.get(&url, Accept::Text).unwrap();
    assert_eq!(resp.body, mime.as_bytes());
}

#[test]
fn recurrence_daily_with_interval_round_trips() {
    let pr = json!({
        "pattern": {"type": "daily", "interval": 2},
        "range": {"type": "noEnd"}
    });
    let out = GraphRecurrence(&pr).to_rule().unwrap();
    assert_eq!(out["frequency"], "daily");
    assert_eq!(out["interval"], 2);
}

#[test]
fn recurrence_weekly_with_days_of_week_and_count() {
    let pr = json!({
        "pattern": {
            "type": "weekly",
            "interval": 1,
            "daysOfWeek": ["monday", "wednesday", "friday"]
        },
        "range": {"type": "numbered", "numberOfOccurrences": 12}
    });
    let out = GraphRecurrence(&pr).to_rule().unwrap();
    assert_eq!(out["frequency"], "weekly");
    assert_eq!(out["count"], 12);
    assert_eq!(out["byDay"][0]["day"], "mo");
    assert_eq!(out["byDay"][2]["day"], "fr");
}

#[test]
fn recurrence_relative_monthly_emits_setpos() {
    let pr = json!({
        "pattern": {
            "type": "relativeMonthly", "interval": 1,
            "daysOfWeek": ["thursday"], "index": "third"
        },
        "range": {"type": "noEnd"}
    });
    let out = GraphRecurrence(&pr).to_rule().unwrap();
    assert_eq!(out["frequency"], "monthly");
    assert_eq!(out["bySetPosition"][0], 3);
    assert_eq!(out["byDay"][0]["day"], "th");
}

#[test]
fn recurrence_yearly_with_end_date() {
    let pr = json!({
        "pattern": {"type": "absoluteYearly", "interval": 1, "month": 12, "dayOfMonth": 25},
        "range": {"type": "endDate", "endDate": "2030-12-25"}
    });
    let out = GraphRecurrence(&pr).to_rule().unwrap();
    assert_eq!(out["frequency"], "yearly");
    assert_eq!(out["until"], "2030-12-25T23:59:59");
}

#[test]
fn occurrence_event_is_classified_for_skip() {
    let occ = json!({"id": "occ-1", "type": "occurrence", "seriesMasterId": "master-1"});
    assert_eq!(GraphEvent(&occ).event_type(), EventType::Occurrence);
}

#[test]
fn exception_event_carries_series_master_id() {
    let ex = json!({
        "id": "ex-1",
        "iCalUId": "uid-1",
        "type": "exception",
        "seriesMasterId": "master-1",
        "originalStart": "2026-06-15T10:00:00Z",
        "subject": "Moved",
        "start": {"dateTime": "2026-06-15T11:00:00.0000000", "timeZone": "UTC"},
        "end": {"dateTime": "2026-06-15T12:00:00.0000000", "timeZone": "UTC"}
    });
    let conv = GraphEvent(&ex).convert(None).unwrap();
    assert_eq!(conv.event_type, EventType::Exception);
    assert_eq!(conv.series_master_id.as_deref(), Some("master-1"));
    assert_eq!(conv.original_start.as_deref(), Some("2026-06-15T10:00:00Z"));
}

#[test]
fn contact_minimal_yields_jscontact_card() {
    let c = json!({
        "id": "graph-1",
        "displayName": "Alice Liddell",
        "givenName": "Alice",
        "surname": "Liddell",
        "emailAddresses": [{"name": "Alice", "address": "alice@x.com"}]
    });
    let out = GraphContact(&c).convert().unwrap();
    assert!(out.uid.starts_with("vandelay-graph-"));
    assert_eq!(out.data["@type"], "Card");
    assert_eq!(out.data["name"]["full"], "Alice Liddell");
    let emails = out.data["emails"].as_object().unwrap();
    assert_eq!(emails["email-1"]["address"], "alice@x.com");
}

#[test]
fn integration_dry_run_against_mock_server_lists_three_surfaces() {
    let mut server = Server::new();
    let _principal = server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"uid","userPrincipalName":"alice@x.com"}"#)
        .create();
    let _folders = server
        .mock("GET", "/me/mailFolders?$top=100&includeHiddenFolders=true")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[{"id":"FMAIL","displayName":"Inbox","isHidden":false}]}"#)
        .create();
    let _children = server
        .mock(
            "GET",
            "/me/mailFolders/FMAIL/childFolders?$top=100&includeHiddenFolders=true",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();
    let _calendars = server
        .mock("GET", "/me/calendars?$top=100")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[{"id":"CAL1","name":"Calendar","isDefaultCalendar":true}]}"#)
        .create();
    let _contact_folders = server
        .mock("GET", "/me/contactFolders?$top=100")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[{"id":"CON1","displayName":"Contacts"}]}"#)
        .create();
    let base = server.url();
    let common = vandelay::sync::RunOptions {
        archive: tempfile::NamedTempFile::new().unwrap().path().to_owned(),
        threads: 2,
        mode: RunMode::DryRun,
        max_retries: 0,
        tls: TlsPolicy::Verify,
        logger: Logger::new(Level::Quiet),
    };
    let config = make_config(base.clone(), None, Surfaces::ALL);
    let summary = config.run(common).into_result().unwrap();
    let mailbox = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == ObjectType::Mailbox)
        .map(|(_, c)| c.created)
        .unwrap_or(0);
    let calendar = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == ObjectType::Calendar)
        .map(|(_, c)| c.created)
        .unwrap_or(0);
    let addressbook = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == ObjectType::AddressBook)
        .map(|(_, c)| c.created)
        .unwrap_or(0);
    assert_eq!(mailbox, 1);
    assert_eq!(calendar, 1);
    assert_eq!(addressbook, 1);
    let planned: Vec<(SummaryType, u64)> = summary.plan.iter().map(|(t, p)| (*t, p.new)).collect();
    assert_eq!(
        planned,
        [
            (ObjectType::Mailbox.into(), 1),
            (ObjectType::Calendar.into(), 1),
            (ObjectType::AddressBook.into(), 1),
        ]
    );
}

#[test]
fn integration_full_run_mail_only_imports_mime_via_value() {
    let mut server = Server::new();
    let _principal = server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"uid-1","userPrincipalName":"alice@x.com"}"#)
        .create();
    let _folders = server
        .mock("GET", "/me/mailFolders?$top=100&includeHiddenFolders=true")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[{"id":"FMAIL","displayName":"Inbox","isHidden":false}]}"#)
        .create();
    let _children = server
        .mock(
            "GET",
            "/me/mailFolders/FMAIL/childFolders?$top=100&includeHiddenFolders=true",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();
    let _well_known: Vec<mockito::Mock> = [
        "inbox",
        "drafts",
        "sentitems",
        "deleteditems",
        "junkemail",
        "archive",
    ]
    .iter()
    .map(|name| {
        let path = format!("/me/mailFolders/{name}?$select=id");
        server
            .mock("GET", path.as_str())
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(if *name == "inbox" {
                r#"{"id":"FMAIL"}"#
            } else {
                r#"{"id":"OTHER"}"#
            })
            .create()
    })
    .collect();
    let _ids = server
        .mock(
            "GET",
            "/me/mailFolders/FMAIL/messages?$top=100&$select=id,isRead,isDraft,flag,categories",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[{"id":"MSG-1"}]}"#)
        .create();
    let mime = "From: a@x\r\nTo: b@x\r\nSubject: hi\r\nDate: Tue, 27 May 2026 10:00:00 +0000\r\nMessage-ID: <abc@x>\r\n\r\nhello";
    let _value = server
        .mock("GET", "/me/messages/MSG-1/$value")
        .match_header("accept", "text/plain")
        .with_status(200)
        .with_header("content-type", "text/plain")
        .with_body(mime)
        .create();

    let tmp = tempfile::NamedTempFile::new().unwrap();
    let archive_path = tmp.path().to_owned();
    let common = vandelay::sync::RunOptions {
        archive: archive_path.clone(),
        threads: 2,
        mode: RunMode::Apply,
        max_retries: 0,
        tls: TlsPolicy::Verify,
        logger: Logger::new(Level::Quiet),
    };
    let config = make_config(server.url(), None, surfaces("mail"));
    drop(tmp);
    let summary = config.run(common).into_result().unwrap();
    let emails = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == ObjectType::Email)
        .map(|(_, c)| c.created)
        .unwrap_or(0);
    assert_eq!(emails, 1, "expected one email created, got {emails}");
}

#[test]
fn integration_duplicate_message_id_does_not_abort_run() {
    let mut server = Server::new();
    let _principal = server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"uid-1","userPrincipalName":"alice@x.com"}"#)
        .create();
    let _folders = server
        .mock("GET", "/me/mailFolders?$top=100&includeHiddenFolders=true")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[{"id":"FMAIL","displayName":"Inbox","isHidden":false}]}"#)
        .create();
    let _children = server
        .mock(
            "GET",
            "/me/mailFolders/FMAIL/childFolders?$top=100&includeHiddenFolders=true",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();
    let _well_known: Vec<mockito::Mock> = [
        "inbox",
        "drafts",
        "sentitems",
        "deleteditems",
        "junkemail",
        "archive",
    ]
    .iter()
    .map(|name| {
        let path = format!("/me/mailFolders/{name}?$select=id");
        server
            .mock("GET", path.as_str())
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(if *name == "inbox" {
                r#"{"id":"FMAIL"}"#
            } else {
                r#"{"id":"OTHER"}"#
            })
            .create()
    })
    .collect();
    let _ids = server
        .mock(
            "GET",
            "/me/mailFolders/FMAIL/messages?$top=100&$select=id,isRead,isDraft,flag,categories",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[{"id":"MSG-1"},{"id":"MSG-1"}]}"#)
        .create();
    let mime = "From: a@x\r\nTo: b@x\r\nSubject: hi\r\nDate: Tue, 27 May 2026 10:00:00 +0000\r\nMessage-ID: <abc@x>\r\n\r\nhello";
    let _value = server
        .mock("GET", "/me/messages/MSG-1/$value")
        .match_header("accept", "text/plain")
        .with_status(200)
        .with_header("content-type", "text/plain")
        .with_body(mime)
        .create();

    let tmp = tempfile::NamedTempFile::new().unwrap();
    let archive_path = tmp.path().to_owned();
    let common = vandelay::sync::RunOptions {
        archive: archive_path.clone(),
        threads: 2,
        mode: RunMode::Apply,
        max_retries: 0,
        tls: TlsPolicy::Verify,
        logger: Logger::new(Level::Quiet),
    };
    let config = make_config(server.url(), None, surfaces("mail"));
    drop(tmp);
    let summary = config.run(common).into_result().unwrap();
    let emails = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == ObjectType::Email)
        .map(|(_, c)| c.created)
        .unwrap_or(0);
    assert_eq!(emails, 1, "expected one email created, got {emails}");
}

#[test]
fn integration_full_run_is_convergent_on_second_invocation() {
    let mut server = Server::new();
    let _principal = server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"uid-2","userPrincipalName":"alice@x.com"}"#)
        .expect_at_least(2)
        .create();
    let _folders = server
        .mock("GET", "/me/mailFolders?$top=100&includeHiddenFolders=true")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[{"id":"FMAIL","displayName":"Inbox","isHidden":false}]}"#)
        .expect_at_least(2)
        .create();
    let _children = server
        .mock(
            "GET",
            "/me/mailFolders/FMAIL/childFolders?$top=100&includeHiddenFolders=true",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .expect_at_least(2)
        .create();
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
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(if name == "inbox" {
                r#"{"id":"FMAIL"}"#
            } else {
                r#"{"id":"OTHER"}"#
            })
            .expect_at_least(2)
            .create();
    }
    let _ids = server
        .mock(
            "GET",
            "/me/mailFolders/FMAIL/messages?$top=100&$select=id,isRead,isDraft,flag,categories",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[{"id":"MSG-A"}]}"#)
        .expect_at_least(2)
        .create();
    let mime = "From: a@x\r\nTo: b@x\r\nSubject: hi\r\nDate: Tue, 27 May 2026 10:00:00 +0000\r\nMessage-ID: <abc@x>\r\n\r\nbody";
    let _value = server
        .mock("GET", "/me/messages/MSG-A/$value")
        .with_status(200)
        .with_header("content-type", "text/plain")
        .with_body(mime)
        .expect(1)
        .create();

    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    let api_base = server.url();
    let make_config = || make_config(api_base.clone(), None, surfaces("mail"));
    let make_common = |path: std::path::PathBuf| vandelay::sync::RunOptions {
        archive: path,
        threads: 2,
        mode: RunMode::Apply,
        max_retries: 0,
        tls: TlsPolicy::Verify,
        logger: Logger::new(Level::Quiet),
    };

    let first = make_config()
        .run(make_common(archive.clone()))
        .into_result()
        .unwrap();
    let created_first = first
        .per_type
        .iter()
        .find(|(t, _)| *t == ObjectType::Email)
        .map(|(_, c)| c.created)
        .unwrap_or(0);
    assert_eq!(created_first, 1);

    let second = make_config()
        .run(make_common(archive))
        .into_result()
        .unwrap();
    let created_second = second
        .per_type
        .iter()
        .find(|(t, _)| *t == ObjectType::Email)
        .map(|(_, c)| c.created)
        .unwrap_or(0);
    assert_eq!(created_second, 0, "second run should not re-import");
}

#[test]
fn source_change_protection_refuses_a_different_account() {
    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    let db = Archive::open(&archive).unwrap();
    db.store()
        .sources()
        .upsert(
            &SourceKey {
                kind: SourceKind::ExchangeGraph,
                url: "https://login.microsoftonline.com/common|https://graph.microsoft.com/v1.0",
                account_id: "old-user-id",
            },
            Some("old@x.com"),
            "old@x.com",
        )
        .unwrap();
    drop(db);

    let mut server = Server::new();
    let _principal = server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"new-user-id","userPrincipalName":"new@x.com"}"#)
        .create();

    let common = vandelay::sync::RunOptions {
        archive,
        threads: 2,
        mode: RunMode::Apply,
        max_retries: 0,
        tls: TlsPolicy::Verify,
        logger: Logger::new(Level::Quiet),
    };
    let config = vandelay::import::graph::GraphImport {
        auth: vandelay::import::graph::GraphAuth::PreAcquired {
            token: make_jwt(9999999999, "new@x.com"),
        },
        ..make_config(server.url(), None, Surfaces::ALL)
    };
    let err = config.run(common).into_result().unwrap_err();
    assert!(
        matches!(err, vandelay::error::Error::SourceChange(_)),
        "expected SourceChange error, got {err:?}"
    );
}

// A dry run against an archive of another Graph account fails like the real run (R6).
#[test]
fn dry_run_reports_source_change_like_the_real_run() {
    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    let db = Archive::open(&archive).unwrap();
    db.store()
        .sources()
        .upsert(
            &SourceKey {
                kind: SourceKind::ExchangeGraph,
                url: "https://login.microsoftonline.com/common|https://graph.microsoft.com/v1.0",
                account_id: "old-user-id",
            },
            Some("old@x.com"),
            "old@x.com",
        )
        .unwrap();
    drop(db);

    let mut server = Server::new();
    let _principal = server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"new-user-id","userPrincipalName":"new@x.com"}"#)
        .create();
    let folders = server
        .mock("GET", "/me/mailFolders?$top=100&includeHiddenFolders=true")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .expect(0)
        .create();

    let mut common = make_common(archive);
    common.mode = RunMode::DryRun;
    let err = make_config(server.url(), None, Surfaces::ALL)
        .run(common)
        .into_result()
        .unwrap_err();
    assert!(
        matches!(err, vandelay::error::Error::SourceChange(_)),
        "expected SourceChange error, got {err:?}"
    );
    assert_eq!(err.exit_status().code(), 4);
    assert_eq!(
        err.to_string(),
        "source-change protection: archive already records exchange_graph source \
         (https://login.microsoftonline.com/common|https://graph.microsoft.com/v1.0, \
         account old-user-id); pass --allow-source-change to import a different account"
    );
    folders.assert();
}

// An abort on the calendar surface keeps the mail counts already gathered (R15).
#[test]
fn abort_after_mail_keeps_the_mail_counts() {
    let mut server = Server::new();
    let _principal = server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"uid-1","userPrincipalName":"alice@x.com"}"#)
        .create();
    stub_mail_surface(&mut server);
    let _calendars = server
        .mock("GET", "/me/calendars?$top=100")
        .with_status(400)
        .with_header("content-type", "application/json")
        .with_body(r#"{"error":{"code":"BadRequest","message":"nope"}}"#)
        .create();

    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    let outcome =
        make_config(server.url(), None, surfaces("mail,calendar")).run(make_common(archive));
    assert!(
        outcome.error.is_some(),
        "the calendar 400 must abort the run"
    );
    let summary = &outcome.summary;
    assert_eq!(summary.get(ObjectType::Mailbox).map(|c| c.created), Some(1));
    assert_eq!(summary.get(ObjectType::Email).map(|c| c.created), Some(1));
    assert_eq!(
        summary.get(ObjectType::Calendar).map(|c| c.created),
        Some(0)
    );
}

fn stub_well_known_folders(server: &mut Server, inbox_id: &str) {
    for name in [
        "drafts",
        "sentitems",
        "deleteditems",
        "junkemail",
        "archive",
    ] {
        let body = format!(r#"{{"id":"OTHER-{name}"}}"#);
        server
            .mock("GET", format!("/me/mailFolders/{name}?$select=id").as_str())
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body)
            .expect_at_least(0)
            .create();
    }
    server
        .mock("GET", "/me/mailFolders/inbox?$select=id")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(format!(r#"{{"id":"{inbox_id}"}}"#))
        .expect_at_least(0)
        .create();
}

fn make_common(archive: std::path::PathBuf) -> vandelay::sync::RunOptions {
    vandelay::sync::RunOptions {
        archive,
        threads: 2,
        mode: RunMode::Apply,
        max_retries: 0,
        tls: TlsPolicy::Verify,
        logger: Logger::new(Level::Quiet),
    }
}

fn surfaces(list: &str) -> vandelay::exchange::graph::types::Surfaces {
    vandelay::exchange::graph::types::Surfaces::parse_list(list).unwrap()
}

fn make_config(
    api_base: String,
    user_target: Option<String>,
    surfaces: vandelay::exchange::graph::types::Surfaces,
) -> vandelay::import::graph::GraphImport {
    vandelay::import::graph::GraphImport {
        auth: vandelay::import::graph::GraphAuth::PreAcquired {
            token: make_jwt(9999999999, "alice@x.com"),
        },
        api_base,
        user_target,
        mailbox_kind: vandelay::exchange::graph::types::MailboxKind::Primary,
        surfaces,
        event_body_format: vandelay::exchange::graph::types::EventBodyFormat::Text,
        graph_connections: 2,
        top: 100,
        exception_window_years: 5,
        contact_photos: vandelay::import::graph::ContactPhotos::Skip,
        event_attachments: vandelay::import::graph::EventAttachments::Fetch,
        source_change: SourceChange::Reject,
    }
}

#[test]
fn users_upn_routes_through_users_segment_not_me() {
    let mut server = Server::new();
    let _resolve = server
        .mock(
            "GET",
            Matcher::Regex(r"^/users/alice[@%][^/]*\?\$select=id".to_owned()),
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"user-uuid-7","userPrincipalName":"alice@x.com"}"#)
        .create();
    let _principal = server
        .mock(
            "GET",
            Matcher::Regex(r"^/users/user-uuid-7\?\$select=id".to_owned()),
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"id":"user-uuid-7","userPrincipalName":"alice@x.com","displayName":"Alice"}"#,
        )
        .create();
    let _folders = server
        .mock(
            "GET",
            "/users/user-uuid-7/mailFolders?$top=100&includeHiddenFolders=true",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .expect_at_least(1)
        .create();
    let _calendars = server
        .mock("GET", "/users/user-uuid-7/calendars?$top=100")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .expect_at_least(0)
        .create();
    let _contact_folders = server
        .mock("GET", "/users/user-uuid-7/contactFolders?$top=100")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .expect_at_least(0)
        .create();
    let base = server.url();
    let common = vandelay::sync::RunOptions {
        archive: tempfile::NamedTempFile::new().unwrap().path().to_owned(),
        threads: 2,
        mode: RunMode::DryRun,
        max_retries: 0,
        tls: TlsPolicy::Verify,
        logger: Logger::new(Level::Quiet),
    };
    let summary = make_config(base, Some("alice@x.com".to_owned()), Surfaces::ALL)
        .run(common)
        .into_result()
        .unwrap();
    let mailbox = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == ObjectType::Mailbox)
        .map(|(_, c)| c.created)
        .unwrap_or(99);
    assert_eq!(mailbox, 0);
}

#[test]
fn series_master_with_exception_merges_into_recurrence_overrides() {
    let mut server = Server::new();
    server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"uid","userPrincipalName":"alice@x.com"}"#)
        .create();
    server
        .mock("GET", "/me/mailFolders?$top=100&includeHiddenFolders=true")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();
    server
        .mock("GET", "/me/mailboxSettings?$select=timeZone")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"timeZone":"UTC"}"#)
        .create();
    server
        .mock("GET", "/me/calendars?$top=100")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[{"id":"CAL1","name":"Calendar","isDefaultCalendar":true}]}"#)
        .create();
    server
        .mock("GET", "/me/contactFolders?$top=100")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();
    server
        .mock(
            "GET",
            "/me/calendars/CAL1/events?$top=100&$select=id,type,seriesMasterId",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"value":[
                {"id":"MASTER","type":"seriesMaster","iCalUId":"uid-master"}
            ]}"#,
        )
        .create();
    server
        .mock(
            "GET",
            Matcher::Regex(r"^/me/calendars/CAL1/calendarView\?startDateTime=".to_owned()),
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"value":[{
                "id":"EX",
                "iCalUId":"uid-master",
                "type":"exception",
                "seriesMasterId":"MASTER",
                "originalStart":"2026-05-11T15:00:00Z",
                "subject":"Moved sync",
                "start":{"dateTime":"2026-05-11T16:00:00.0000000","timeZone":"UTC"},
                "end":{"dateTime":"2026-05-11T17:00:00.0000000","timeZone":"UTC"}
            }]}"#,
        )
        .expect_at_least(1)
        .create();
    server
        .mock("GET", "/me/events/MASTER")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "id":"MASTER",
                "iCalUId":"uid-master",
                "type":"seriesMaster",
                "subject":"Weekly sync",
                "start":{"dateTime":"2026-05-04T15:00:00.0000000","timeZone":"UTC"},
                "end":{"dateTime":"2026-05-04T16:00:00.0000000","timeZone":"UTC"},
                "recurrence":{
                    "pattern":{"type":"weekly","interval":1,"daysOfWeek":["monday"]},
                    "range":{"type":"noEnd"}
                }
            }"#,
        )
        .create();
    let base = server.url();
    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    let summary = make_config(base, None, surfaces("calendar"))
        .run(make_common(archive.clone()))
        .into_result()
        .unwrap();
    let events = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == ObjectType::CalendarEvent)
        .map(|(_, c)| c.created)
        .unwrap_or(0);
    assert_eq!(events, 1, "exactly one master row, exception merged in");

    let db = Archive::open(&archive).unwrap();
    let conn = db.connection();
    let row: String = conn
        .query_row("SELECT data FROM calendar_events", [], |row| row.get(0))
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&row).unwrap();
    let overrides = v
        .get("recurrenceOverrides")
        .and_then(|o| o.as_object())
        .expect("recurrenceOverrides present");
    assert!(
        overrides.contains_key("2026-05-11T15:00:00"),
        "override keyed by originalStart as LocalDateTime; got keys: {:?}",
        overrides.keys().collect::<Vec<_>>()
    );
    let override_data = &overrides["2026-05-11T15:00:00"];
    assert_eq!(override_data["title"], "Moved sync");
    assert_eq!(override_data["start"], "2026-05-11T16:00:00");
    for ignored in [
        "@type",
        "uid",
        "method",
        "organizerCalendarAddress",
        "privacy",
        "prodId",
        "recurrenceId",
        "recurrenceIdTimeZone",
        "sentBy",
        "recurrenceRule",
        "recurrenceOverrides",
        "relatedTo",
    ] {
        assert!(
            override_data.get(ignored).is_none(),
            "{ignored} must not appear in a PatchObject (jscalendarbis 3.3.4); got {override_data}"
        );
    }
}

#[test]
fn occurrence_event_is_skipped_no_row_no_id_mapping() {
    let mut server = Server::new();
    server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"uid","userPrincipalName":"alice@x.com"}"#)
        .create();
    server
        .mock("GET", "/me/mailFolders?$top=100&includeHiddenFolders=true")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();
    server
        .mock("GET", "/me/mailboxSettings?$select=timeZone")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"timeZone":"UTC"}"#)
        .create();
    server
        .mock("GET", "/me/calendars?$top=100")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[{"id":"CAL1","name":"Calendar","isDefaultCalendar":true}]}"#)
        .create();
    server
        .mock("GET", "/me/contactFolders?$top=100")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();
    server
        .mock(
            "GET",
            "/me/calendars/CAL1/events?$top=100&$select=id,type,seriesMasterId",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"value":[
                {"id":"OCC","type":"occurrence","seriesMasterId":"MASTER"}
            ]}"#,
        )
        .create();
    let occ_get = server
        .mock("GET", "/me/events/OCC")
        .with_status(500)
        .with_body("MUST NOT BE CALLED")
        .expect(0)
        .create();
    let base = server.url();
    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    let summary = make_config(base, None, surfaces("calendar"))
        .run(make_common(archive.clone()))
        .into_result()
        .unwrap();
    let events = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == ObjectType::CalendarEvent)
        .map(|(_, c)| c.created)
        .unwrap_or(99);
    assert_eq!(events, 0, "occurrence must not be fetched or stored");
    occ_get.assert();

    let db = Archive::open(&archive).unwrap();
    let conn = db.connection();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM calendar_events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn hidden_mail_folder_has_is_subscribed_zero() {
    let mut server = Server::new();
    server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"uid","userPrincipalName":"alice@x.com"}"#)
        .create();
    server
        .mock("GET", "/me/mailFolders?$top=100&includeHiddenFolders=true")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"value":[
                {"id":"FVISIBLE","displayName":"Inbox","isHidden":false},
                {"id":"FHIDDEN","displayName":"SearchFolders","isHidden":true}
            ]}"#,
        )
        .create();
    server
        .mock(
            "GET",
            "/me/mailFolders/FVISIBLE/childFolders?$top=100&includeHiddenFolders=true",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();
    server
        .mock(
            "GET",
            "/me/mailFolders/FHIDDEN/childFolders?$top=100&includeHiddenFolders=true",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();
    server
        .mock(
            "GET",
            "/me/mailFolders/FVISIBLE/messages?$top=100&$select=id,isRead,isDraft,flag,categories",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();
    server
        .mock(
            "GET",
            "/me/mailFolders/FHIDDEN/messages?$top=100&$select=id,isRead,isDraft,flag,categories",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();
    stub_well_known_folders(&mut server, "FVISIBLE");
    let base = server.url();
    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    let _ = make_config(base, None, surfaces("mail"))
        .run(make_common(archive.clone()))
        .into_result()
        .unwrap();
    let db = Archive::open(&archive).unwrap();
    let conn = db.connection();
    let visible: i64 = conn
        .query_row(
            "SELECT is_subscribed FROM mailboxes WHERE name = 'Inbox'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let hidden: i64 = conn
        .query_row(
            "SELECT is_subscribed FROM mailboxes WHERE name = 'SearchFolders'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(visible, 1);
    assert_eq!(hidden, 0);
}

#[test]
fn well_known_folder_probes_assign_jmap_roles() {
    let mut server = Server::new();
    server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"uid","userPrincipalName":"alice@x.com"}"#)
        .create();
    server
        .mock("GET", "/me/mailFolders?$top=100&includeHiddenFolders=true")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"value":[
                {"id":"INBOX-ID","displayName":"Inbox","isHidden":false},
                {"id":"DRAFTS-ID","displayName":"Drafts","isHidden":false},
                {"id":"SENT-ID","displayName":"Sent Items","isHidden":false},
                {"id":"TRASH-ID","displayName":"Deleted Items","isHidden":false},
                {"id":"JUNK-ID","displayName":"Junk Email","isHidden":false},
                {"id":"ARCHIVE-ID","displayName":"Archive","isHidden":false}
            ]}"#,
        )
        .create();
    for fid in [
        "INBOX-ID",
        "DRAFTS-ID",
        "SENT-ID",
        "TRASH-ID",
        "JUNK-ID",
        "ARCHIVE-ID",
    ] {
        server
            .mock(
                "GET",
                format!("/me/mailFolders/{fid}/childFolders?$top=100&includeHiddenFolders=true")
                    .as_str(),
            )
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"value":[]}"#)
            .create();
        server
            .mock(
                "GET",
                format!("/me/mailFolders/{fid}/messages?$top=100&$select=id,isRead,isDraft,flag,categories").as_str(),
            )
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"value":[]}"#)
            .create();
    }
    let pairs = [
        ("inbox", "INBOX-ID"),
        ("drafts", "DRAFTS-ID"),
        ("sentitems", "SENT-ID"),
        ("deleteditems", "TRASH-ID"),
        ("junkemail", "JUNK-ID"),
        ("archive", "ARCHIVE-ID"),
    ];
    for (name, id) in pairs {
        server
            .mock("GET", format!("/me/mailFolders/{name}?$select=id").as_str())
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(r#"{{"id":"{id}"}}"#))
            .create();
    }
    let base = server.url();
    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    let _ = make_config(base, None, surfaces("mail"))
        .run(make_common(archive.clone()))
        .into_result()
        .unwrap();
    let db = Archive::open(&archive).unwrap();
    let conn = db.connection();
    let mut stmt = conn
        .prepare("SELECT name, role FROM mailboxes ORDER BY name")
        .unwrap();
    let rows: Vec<(String, Option<String>)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let by_name: std::collections::HashMap<String, Option<String>> = rows.into_iter().collect();
    assert_eq!(by_name["Inbox"].as_deref(), Some("inbox"));
    assert_eq!(by_name["Drafts"].as_deref(), Some("drafts"));
    assert_eq!(by_name["Sent Items"].as_deref(), Some("sent"));
    assert_eq!(by_name["Deleted Items"].as_deref(), Some("trash"));
    assert_eq!(by_name["Junk Email"].as_deref(), Some("junk"));
    assert_eq!(by_name["Archive"].as_deref(), Some("archive"));
}

#[test]
fn attachment_message_does_not_trigger_attachments_endpoint() {
    let mut server = Server::new();
    server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"uid","userPrincipalName":"alice@x.com"}"#)
        .create();
    server
        .mock("GET", "/me/mailFolders?$top=100&includeHiddenFolders=true")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[{"id":"FMAIL","displayName":"Inbox","isHidden":false}]}"#)
        .create();
    server
        .mock(
            "GET",
            "/me/mailFolders/FMAIL/childFolders?$top=100&includeHiddenFolders=true",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();
    stub_well_known_folders(&mut server, "FMAIL");
    server
        .mock(
            "GET",
            "/me/mailFolders/FMAIL/messages?$top=100&$select=id,isRead,isDraft,flag,categories",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[{"id":"MSG-ATT"}]}"#)
        .create();
    let mime_with_attachment = "From: a@x\r\nTo: b@x\r\nSubject: with attachment\r\n\
Date: Tue, 27 May 2026 10:00:00 +0000\r\nMessage-ID: <abc@x>\r\n\
Content-Type: multipart/mixed; boundary=\"X\"\r\n\r\n\
--X\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
--X\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"a.bin\"\r\nContent-Transfer-Encoding: base64\r\n\r\n\
SGVsbG8=\r\n--X--\r\n";
    server
        .mock("GET", "/me/messages/MSG-ATT/$value")
        .with_status(200)
        .with_header("content-type", "text/plain")
        .with_body(mime_with_attachment)
        .create();
    let no_attachments = server
        .mock(
            "GET",
            Matcher::Regex(r"^/me/messages/[^/]+/attachments".to_owned()),
        )
        .with_status(500)
        .with_body("MUST NOT BE CALLED")
        .expect(0)
        .create();
    let base = server.url();
    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    let summary = make_config(base, None, surfaces("mail"))
        .run(make_common(archive))
        .into_result()
        .unwrap();
    let emails = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == ObjectType::Email)
        .map(|(_, c)| c.created)
        .unwrap_or(0);
    assert_eq!(emails, 1);
    no_attachments.assert();
}

#[test]
fn event_get_carries_outlook_timezone_and_body_content_type_prefer() {
    let mut server = Server::new();
    server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"uid","userPrincipalName":"alice@x.com"}"#)
        .create();
    server
        .mock("GET", "/me/mailFolders?$top=100&includeHiddenFolders=true")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();
    server
        .mock("GET", "/me/mailboxSettings?$select=timeZone")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"timeZone":"Pacific Standard Time"}"#)
        .create();
    server
        .mock("GET", "/me/calendars?$top=100")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[{"id":"CAL1","name":"Calendar"}]}"#)
        .create();
    server
        .mock("GET", "/me/contactFolders?$top=100")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();
    let _events_list = server
        .mock(
            "GET",
            "/me/calendars/CAL1/events?$top=100&$select=id,type,seriesMasterId",
        )
        .match_header(
            "prefer",
            Matcher::AllOf(vec![
                Matcher::Regex(r#"outlook\.timezone="UTC""#.to_owned()),
                Matcher::Regex(r#"IdType="ImmutableId""#.to_owned()),
            ]),
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[{"id":"EVT","type":"singleInstance"}]}"#)
        .expect(1)
        .create();
    let _event_get = server
        .mock("GET", "/me/events/EVT")
        .match_header(
            "prefer",
            Matcher::AllOf(vec![
                Matcher::Regex(r#"outlook\.body-content-type="text""#.to_owned()),
                Matcher::Regex(r#"outlook\.timezone="UTC""#.to_owned()),
                Matcher::Regex(r#"IdType="ImmutableId""#.to_owned()),
            ]),
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "id":"EVT","iCalUId":"uid-evt","type":"singleInstance",
                "subject":"Test","start":{"dateTime":"2026-05-27T10:00:00.0000000","timeZone":"UTC"},
                "end":{"dateTime":"2026-05-27T11:00:00.0000000","timeZone":"UTC"}
            }"#,
        )
        .expect(1)
        .create();
    let base = server.url();
    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    let _ = make_config(base, None, surfaces("calendar"))
        .run(make_common(archive))
        .into_result()
        .unwrap();
}

#[test]
fn graph_client_retries_after_401_when_bearer_is_swapped() {
    let mut server = Server::new();
    let _m1 = server
        .mock("GET", "/me")
        .match_header("authorization", "Bearer EXPIRED")
        .with_status(401)
        .with_body("expired")
        .expect(1)
        .create();
    let _m2 = server
        .mock("GET", "/me")
        .match_header("authorization", "Bearer FRESH")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"u","userPrincipalName":"a@b"}"#)
        .expect(1)
        .create();
    let base = server.url();
    let client = GraphClient::new(
        "EXPIRED".to_owned(),
        ClientOptions {
            retry: RetryPolicy::new(0),
            tls: TlsPolicy::Verify,
            logger: Logger::new(Level::Quiet),
        },
    );
    let url = format!("{base}/me");
    let err = client.get(&url, Accept::Json).unwrap_err();
    assert!(matches!(err, GraphError::Auth(_)));
    client.set_bearer("FRESH".to_owned());
    let resp = client.get(&url, Accept::Json).unwrap();
    assert_eq!(resp.status, 200);
}

#[test]
fn full_run_records_graph_id_in_sync_id_exchange_graph_with_padding() {
    let mut server = Server::new();
    let _principal = server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"uid-pad","userPrincipalName":"pad@x.com"}"#)
        .create();
    let _folders = server
        .mock("GET", "/me/mailFolders?$top=100&includeHiddenFolders=true")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[{"id":"AAkA-Padded==","displayName":"Inbox","isHidden":false}]}"#)
        .create();
    let _children = server
        .mock(
            "GET",
            "/me/mailFolders/AAkA-Padded%3D%3D/childFolders?$top=100&includeHiddenFolders=true",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();
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
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"id":"OTHER"}"#)
            .create();
    }
    let _ids = server
        .mock(
            "GET",
            "/me/mailFolders/AAkA-Padded%3D%3D/messages?$top=100&$select=id,isRead,isDraft,flag,categories",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();

    let tmp = tempfile::NamedTempFile::new().unwrap();
    let archive = tmp.path().to_owned();
    let common = vandelay::sync::RunOptions {
        archive: archive.clone(),
        threads: 2,
        mode: RunMode::Apply,
        max_retries: 0,
        tls: TlsPolicy::Verify,
        logger: Logger::new(Level::Quiet),
    };
    let config = vandelay::import::graph::GraphImport {
        auth: vandelay::import::graph::GraphAuth::PreAcquired {
            token: make_jwt(9999999999, "pad@x.com"),
        },
        ..make_config(server.url(), None, surfaces("mail"))
    };
    drop(tmp);
    let _ = config.run(common).into_result().unwrap();
    let db = Archive::open(&archive).unwrap();
    let conn = db.connection();
    let (graph_id, type_name): (String, String) = conn
        .query_row(
            "SELECT graph_id, type_name FROM sync_id_exchange_graph",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        graph_id, "AAkA-Padded==",
        "case-sensitive padded id round-trips verbatim"
    );
    assert_eq!(type_name, "mailbox");
    let account_id: String = conn
        .query_row(
            "SELECT account_id FROM sources WHERE kind = 'exchange_graph'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        account_id, "uid-pad",
        "primary kind uses directory id verbatim"
    );
}

#[test]
fn archive_mailbox_kind_encodes_synthetic_suffix_in_account_id() {
    let mut server = Server::new();
    let _principal = server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"uid-arch","userPrincipalName":"a@x.com"}"#)
        .create();
    let _root = server
        .mock(
            "GET",
            "/me/mailFolders/archive/childFolders?$top=100&includeHiddenFolders=true",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();

    let tmp = tempfile::NamedTempFile::new().unwrap();
    let archive = tmp.path().to_owned();
    let common = vandelay::sync::RunOptions {
        archive: archive.clone(),
        threads: 2,
        mode: RunMode::Apply,
        max_retries: 0,
        tls: TlsPolicy::Verify,
        logger: Logger::new(Level::Quiet),
    };
    let config = vandelay::import::graph::GraphImport {
        auth: vandelay::import::graph::GraphAuth::PreAcquired {
            token: make_jwt(9999999999, "a@x.com"),
        },
        mailbox_kind: vandelay::exchange::graph::types::MailboxKind::Archive,
        ..make_config(server.url(), None, surfaces("mail"))
    };
    drop(tmp);
    let _ = config.run(common).into_result().unwrap();
    let db = Archive::open(&archive).unwrap();
    let conn = db.connection();
    let account_id: String = conn
        .query_row(
            "SELECT account_id FROM sources WHERE kind = 'exchange_graph'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(account_id, "uid-arch#archive");
}

#[test]
fn allow_source_change_permits_overwriting_a_different_account() {
    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    let db = Archive::open(&archive).unwrap();
    db.store()
        .sources()
        .upsert(
            &SourceKey {
                kind: SourceKind::ExchangeGraph,
                url: "https://login.microsoftonline.com/common|https://x.example",
                account_id: "different-uid",
            },
            Some("old@x.com"),
            "old@x.com",
        )
        .unwrap();
    drop(db);

    let mut server = Server::new();
    let _principal = server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"new-uid","userPrincipalName":"new@x.com"}"#)
        .create();
    let _folders = server
        .mock("GET", "/me/mailFolders?$top=100&includeHiddenFolders=true")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();

    let common = vandelay::sync::RunOptions {
        archive: archive.clone(),
        threads: 2,
        mode: RunMode::Apply,
        max_retries: 0,
        tls: TlsPolicy::Verify,
        logger: Logger::new(Level::Quiet),
    };
    let config = vandelay::import::graph::GraphImport {
        auth: vandelay::import::graph::GraphAuth::PreAcquired {
            token: make_jwt(9999999999, "new@x.com"),
        },
        source_change: SourceChange::Allow,
        ..make_config(server.url(), None, surfaces("mail"))
    };
    let result = config.run(common).into_result();
    assert!(
        result.is_ok(),
        "--allow-source-change must permit a different account, got {result:?}"
    );
}

#[test]
fn dry_run_makes_no_per_item_get_and_no_sqlite_writes() {
    let mut server = Server::new();
    let _principal = server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"u","userPrincipalName":"a@x.com"}"#)
        .create();
    let _folders = server
        .mock("GET", "/me/mailFolders?$top=100&includeHiddenFolders=true")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[{"id":"FMAIL","displayName":"Inbox","isHidden":false}]}"#)
        .create();
    let _children = server
        .mock(
            "GET",
            "/me/mailFolders/FMAIL/childFolders?$top=100&includeHiddenFolders=true",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();
    let _calendars = server
        .mock("GET", "/me/calendars?$top=100")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();
    let _contacts = server
        .mock("GET", "/me/contactFolders?$top=100")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();
    let no_messages = server
        .mock(
            "GET",
            Matcher::Regex(r"^/me/mailFolders/.*/messages".to_owned()),
        )
        .expect(0)
        .create();
    let no_well_known = server
        .mock(
            "GET",
            Matcher::Regex(r"^/me/mailFolders/[a-z]+\?".to_owned()),
        )
        .expect(0)
        .create();

    let tmp = tempfile::NamedTempFile::new().unwrap();
    let archive = tmp.path().to_owned();
    let common = vandelay::sync::RunOptions {
        archive: archive.clone(),
        threads: 2,
        mode: RunMode::DryRun,
        max_retries: 0,
        tls: TlsPolicy::Verify,
        logger: Logger::new(Level::Quiet),
    };
    let config = vandelay::import::graph::GraphImport {
        auth: vandelay::import::graph::GraphAuth::PreAcquired {
            token: make_jwt(9999999999, "a@x.com"),
        },
        ..make_config(server.url(), None, Surfaces::ALL)
    };
    drop(tmp);
    let _ = config.run(common).into_result().unwrap();
    no_messages.assert();
    no_well_known.assert();
    let db = Archive::open(&archive).unwrap();
    let conn = db.connection();
    let sources: i64 = conn
        .query_row("SELECT count(*) FROM sources", [], |row| row.get(0))
        .unwrap();
    assert_eq!(sources, 0, "dry-run must not write the sources row");
    let mailboxes: i64 = conn
        .query_row("SELECT count(*) FROM mailboxes", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mailboxes, 0, "dry-run must not insert mailbox rows");
}

#[test]
fn refresh_access_token_failure_message_carries_status() {
    let other = TokenResponse::parse(401, &json!({"error": "invalid_grant"}));
    match other {
        TokenResponse::Other { error, .. } => assert_eq!(error, "invalid_grant"),
        v => panic!("expected Other for invalid_grant, got {v:?}"),
    }
}

#[test]
fn token_response_expired_recognised() {
    let r = TokenResponse::parse(400, &json!({"error": "expired_token"}));
    assert!(matches!(r, TokenResponse::Expired));
}

#[test]
fn token_response_bad_code_recognised() {
    let r = TokenResponse::parse(400, &json!({"error": "bad_verification_code"}));
    assert!(matches!(r, TokenResponse::BadCode));
}

// Vanished emails are swept; a vanished mailbox with a live child is counted failed and kept (R8).
#[test]
fn vanished_items_are_swept_and_a_parent_with_a_live_child_is_kept() {
    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    let principal = r#"{"id":"uid-1","userPrincipalName":"alice@x.com"}"#;
    let mut server = Server::new();
    let _principal = server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(principal)
        .create();
    stub_mail_surface(&mut server);
    make_config(server.url(), None, surfaces("mail"))
        .run(make_common(archive.clone()))
        .into_result()
        .unwrap();

    let db = Archive::open(&archive).unwrap();
    let conn = db.connection();
    let parent: i64 = conn
        .query_row(
            "SELECT local_id FROM sync_id_exchange_graph WHERE graph_id = 'FMAIL'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO mailboxes (name, parent_id, sort_order, is_subscribed) VALUES ('Local', ?1, 0, 1)",
        [parent],
    )
    .unwrap();
    drop(db);

    server.reset();
    let _principal = server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(principal)
        .create();
    let _folders = server
        .mock("GET", "/me/mailFolders?$top=100&includeHiddenFolders=true")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();
    stub_well_known_folders(&mut server, "GONE");
    let summary = make_config(server.url(), None, surfaces("mail"))
        .run(make_common(archive.clone()))
        .into_result()
        .unwrap();
    let email = summary.get(ObjectType::Email).unwrap();
    assert_eq!((email.deleted, email.failed), (1, 0));
    let mailbox = summary.get(ObjectType::Mailbox).unwrap();
    assert_eq!((mailbox.deleted, mailbox.failed), (0, 1));

    let db = Archive::open(&archive).unwrap();
    let conn = db.connection();
    assert_eq!(row_count(conn, "emails"), 0);
    assert_eq!(row_count(conn, "mailboxes"), 2);
    let mapped: i64 = conn
        .query_row(
            "SELECT count(*) FROM sync_id_exchange_graph WHERE graph_id = 'FMAIL'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        mapped, 1,
        "a mailbox that could not be deleted keeps its mapping"
    );
}

#[test]
fn folder_enumeration_failure_skips_vanished_deletion() {
    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    let setup_db = Archive::open(&archive).unwrap();
    let setup_conn = setup_db.connection();
    let sid = setup_db
        .store()
        .sources()
        .upsert(
            &SourceKey {
                kind: SourceKind::ExchangeGraph,
                url: "stale",
                account_id: "stale-uid",
            },
            Some("stale@x.com"),
            "stale@x.com",
        )
        .unwrap();
    setup_conn
        .execute(
            "INSERT INTO mailboxes (id, name, sort_order, is_subscribed) VALUES (42, 'Inbox', 0, 1)",
            [],
        )
        .unwrap();
    setup_conn
        .execute(
            "INSERT INTO sync_id_exchange_graph (source_id, type_name, graph_id, local_id) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![sid, "mailbox", "FMAIL", 42],
        )
        .unwrap();
    setup_conn
        .execute(
            "INSERT INTO blobs (hash, data) VALUES (x'aa', x'68656c6c6f')",
            [],
        )
        .unwrap();
    let blob_id: i64 = setup_conn
        .query_row("SELECT id FROM blobs WHERE hash = x'aa'", [], |r| r.get(0))
        .unwrap();
    setup_conn
        .execute(
            "INSERT INTO emails (id, blob_id, received_at, mailbox_ids, keywords, message_match)
             VALUES (1, ?1, '2024-01-01T00:00:00Z', '[42]', '[]', '[]')",
            rusqlite::params![blob_id],
        )
        .unwrap();
    setup_conn
        .execute(
            "INSERT INTO sync_id_exchange_graph (source_id, type_name, graph_id, local_id) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![sid, "email", "MSG-LOCAL", 1],
        )
        .unwrap();
    setup_conn
        .execute(
            "UPDATE sources SET kind = 'exchange_graph',
                                session_url = ?1,
                                account_id = 'uid-flake'
             WHERE id = ?2",
            rusqlite::params![
                "https://login.microsoftonline.com/common|".to_owned()
                    + "https://graph.microsoft.com/v1.0",
                sid
            ],
        )
        .unwrap();
    drop(setup_db);

    let mut server = Server::new();
    let _principal = server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"uid-flake","userPrincipalName":"flake@x.com"}"#)
        .create();
    let _folders = server
        .mock("GET", "/me/mailFolders?$top=100&includeHiddenFolders=true")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[{"id":"FMAIL","displayName":"Inbox","isHidden":false}]}"#)
        .create();
    let _children = server
        .mock(
            "GET",
            "/me/mailFolders/FMAIL/childFolders?$top=100&includeHiddenFolders=true",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();
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
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"id":"FMAIL"}"#)
            .create();
    }
    let _enum_fail = server
        .mock(
            "GET",
            "/me/mailFolders/FMAIL/messages?$top=100&$select=id,isRead,isDraft,flag,categories",
        )
        .with_status(503)
        .with_body("transient outage")
        .create();

    let common = vandelay::sync::RunOptions {
        archive: archive.clone(),
        threads: 2,
        mode: RunMode::Apply,
        max_retries: 0,
        tls: TlsPolicy::Verify,
        logger: Logger::new(Level::Quiet),
    };
    let config = vandelay::import::graph::GraphImport {
        auth: vandelay::import::graph::GraphAuth::PreAcquired {
            token: make_jwt(9999999999, "flake@x.com"),
        },
        source_change: SourceChange::Allow,
        ..make_config(server.url(), None, surfaces("mail"))
    };
    let _ = config.run(common).into_result().unwrap();
    let db = Archive::open(&archive).unwrap();
    let conn = db.connection();
    let remaining: i64 = conn
        .query_row("SELECT count(*) FROM emails WHERE id = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        remaining, 1,
        "transient folder-enumeration failure must NOT delete the locally-known message"
    );
}

fn row_count(conn: &rusqlite::Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .unwrap()
}

fn stub_contacts_surface(server: &mut Server) {
    server
        .mock("GET", "/me/contactFolders?$top=100")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .expect_at_least(0)
        .create();
    server
        .mock("GET", "/me/contactFolders/contacts")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"CON1","displayName":"Contacts","wellKnownName":"contacts"}"#)
        .expect_at_least(0)
        .create();
    server
        .mock("GET", "/me/contactFolders/CON1/childFolders?$top=100")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .expect_at_least(0)
        .create();
    server
        .mock(
            "GET",
            "/me/contactFolders/CON1/contacts?$top=100&$select=id",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[{"id":"CT1"}]}"#)
        .expect_at_least(0)
        .create();
    server
        .mock("GET", "/me/contacts/CT1")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"id":"CT1","displayName":"Alice Liddell","givenName":"Alice",
                "surname":"Liddell","emailAddresses":[{"name":"Alice","address":"alice@x.com"}]}"#,
        )
        .expect_at_least(0)
        .create();
}

fn stub_mail_surface(server: &mut Server) {
    server
        .mock("GET", "/me/mailFolders?$top=100&includeHiddenFolders=true")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[{"id":"FMAIL","displayName":"Inbox","isHidden":false}]}"#)
        .expect_at_least(0)
        .create();
    server
        .mock(
            "GET",
            "/me/mailFolders/FMAIL/childFolders?$top=100&includeHiddenFolders=true",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .expect_at_least(0)
        .create();
    stub_well_known_folders(server, "FMAIL");
    server
        .mock(
            "GET",
            "/me/mailFolders/FMAIL/messages?$top=100&$select=id,isRead,isDraft,flag,categories",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[{"id":"MSG-S"}]}"#)
        .expect_at_least(0)
        .create();
    server
        .mock("GET", "/me/messages/MSG-S/$value")
        .with_status(200)
        .with_header("content-type", "text/plain")
        .with_body(
            "From: a@x\r\nTo: b@x\r\nSubject: surface\r\n\
             Date: Tue, 27 May 2026 10:00:00 +0000\r\nMessage-ID: <surface@x>\r\n\r\nbody",
        )
        .expect_at_least(0)
        .create();
}

fn stub_calendar_surface(server: &mut Server) {
    server
        .mock("GET", "/me/mailboxSettings?$select=timeZone")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"timeZone":"UTC"}"#)
        .expect_at_least(0)
        .create();
    server
        .mock("GET", "/me/calendars?$top=100")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[{"id":"CAL1","name":"Calendar","isDefaultCalendar":true}]}"#)
        .expect_at_least(0)
        .create();
    server
        .mock(
            "GET",
            "/me/calendars/CAL1/events?$top=100&$select=id,type,seriesMasterId",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[{"id":"EV1","type":"singleInstance","iCalUId":"uid-ev1"}]}"#)
        .expect_at_least(0)
        .create();
    server
        .mock("GET", "/me/events/EV1")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"id":"EV1","iCalUId":"uid-ev1","type":"singleInstance","subject":"Standup",
                "start":{"dateTime":"2026-05-04T15:00:00.0000000","timeZone":"UTC"},
                "end":{"dateTime":"2026-05-04T16:00:00.0000000","timeZone":"UTC"}}"#,
        )
        .expect_at_least(0)
        .create();
}

#[test]
fn contacts_surface_imports_only_address_books_and_cards() {
    let mut server = Server::new();
    server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"uid-surface-c","userPrincipalName":"alice@x.com"}"#)
        .create();
    stub_contacts_surface(&mut server);
    stub_calendar_surface(&mut server);
    stub_mail_surface(&mut server);
    let no_mail = server
        .mock("GET", Matcher::Regex(r"^/me/mailFolders\?".to_owned()))
        .with_status(500)
        .with_body("MUST NOT BE CALLED")
        .expect(0)
        .create();
    let no_calendars = server
        .mock("GET", Matcher::Regex(r"^/me/calendars\?".to_owned()))
        .with_status(500)
        .with_body("MUST NOT BE CALLED")
        .expect(0)
        .create();

    let base = server.url();
    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    make_config(base, None, surfaces("contacts"))
        .run(make_common(archive.clone()))
        .into_result()
        .unwrap();
    no_mail.assert();
    no_calendars.assert();

    let db = Archive::open(&archive).unwrap();
    let conn = db.connection();
    assert_eq!(row_count(conn, "address_books"), 1);
    assert_eq!(row_count(conn, "contact_cards"), 1);
    assert_eq!(row_count(conn, "mailboxes"), 0);
    assert_eq!(row_count(conn, "emails"), 0);
    assert_eq!(row_count(conn, "calendars"), 0);
    assert_eq!(row_count(conn, "calendar_events"), 0);
}

#[test]
fn mail_surface_imports_only_mailboxes_and_emails() {
    let mut server = Server::new();
    server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"uid-surface-m","userPrincipalName":"alice@x.com"}"#)
        .create();
    stub_mail_surface(&mut server);
    stub_calendar_surface(&mut server);
    stub_contacts_surface(&mut server);
    let no_calendars = server
        .mock("GET", Matcher::Regex(r"^/me/calendars\?".to_owned()))
        .with_status(500)
        .with_body("MUST NOT BE CALLED")
        .expect(0)
        .create();
    let no_contact_folders = server
        .mock("GET", Matcher::Regex(r"^/me/contactFolders\?".to_owned()))
        .with_status(500)
        .with_body("MUST NOT BE CALLED")
        .expect(0)
        .create();

    let base = server.url();
    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    make_config(base, None, surfaces("mail"))
        .run(make_common(archive.clone()))
        .into_result()
        .unwrap();
    no_calendars.assert();
    no_contact_folders.assert();

    let db = Archive::open(&archive).unwrap();
    let conn = db.connection();
    assert_eq!(row_count(conn, "mailboxes"), 1);
    assert_eq!(row_count(conn, "emails"), 1);
    assert_eq!(row_count(conn, "address_books"), 0);
    assert_eq!(row_count(conn, "contact_cards"), 0);
    assert_eq!(row_count(conn, "calendars"), 0);
    assert_eq!(row_count(conn, "calendar_events"), 0);
}

fn stub_principal(server: &mut Server) {
    server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"uid","userPrincipalName":"alice@x.com"}"#)
        .expect_at_least(0)
        .create();
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

#[test]
fn default_contact_folder_is_imported_although_contactfolders_omits_it() {
    let mut server = Server::new();
    stub_principal(&mut server);
    json_mock(
        &mut server,
        "/me/contactFolders?$top=100",
        r#"{"value":[]}"#,
    );
    json_mock(
        &mut server,
        "/me/contactFolders/contacts",
        r#"{"id":"DEFAULT","displayName":"Contacts","wellKnownName":"contacts"}"#,
    );
    json_mock(
        &mut server,
        "/me/contactFolders/DEFAULT/childFolders?$top=100",
        r#"{"value":[]}"#,
    );
    json_mock(
        &mut server,
        "/me/contactFolders/DEFAULT/contacts?$top=100&$select=id",
        r#"{"value":[{"id":"C1"},{"id":"C2"}]}"#,
    );
    json_mock(
        &mut server,
        "/me/contacts/C1",
        r#"{"id":"C1","displayName":"Alice"}"#,
    );
    json_mock(
        &mut server,
        "/me/contacts/C2",
        r#"{"id":"C2","displayName":"Bob"}"#,
    );

    let base = server.url();
    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    let summary = make_config(base, None, surfaces("contacts"))
        .run(make_common(archive.clone()))
        .into_result()
        .unwrap();

    let count = |name: ObjectType| {
        summary
            .per_type
            .iter()
            .find(|(t, _)| *t == name)
            .map(|(_, c)| c.created)
            .unwrap_or(0)
    };
    assert_eq!(
        count(ObjectType::AddressBook),
        1,
        "the default Contacts folder must become an address book even though \
         /me/contactFolders never lists it"
    );
    assert_eq!(
        count(ObjectType::ContactCard),
        2,
        "both default-folder contacts import"
    );

    let db = Archive::open(&archive).unwrap();
    let conn = db.connection();
    let is_default: i64 = conn
        .query_row("SELECT is_default FROM address_books", [], |row| row.get(0))
        .unwrap();
    assert_eq!(is_default, 1, "the well-known folder is the default book");
}

#[test]
fn default_contact_folder_falls_back_to_parent_of_an_existing_contact() {
    let mut server = Server::new();
    stub_principal(&mut server);
    json_mock(
        &mut server,
        "/me/contactFolders?$top=100",
        r#"{"value":[]}"#,
    );
    server
        .mock("GET", "/me/contactFolders/contacts")
        .with_status(404)
        .with_body(r#"{"error":{"code":"ErrorItemNotFound"}}"#)
        .expect_at_least(1)
        .create();
    json_mock(
        &mut server,
        "/me/contacts?$top=1&$select=id,parentFolderId",
        r#"{"value":[{"id":"C1","parentFolderId":"DERIVED"}]}"#,
    );
    json_mock(
        &mut server,
        "/me/contactFolders/DERIVED",
        r#"{"id":"DERIVED","displayName":"Kontakte"}"#,
    );
    json_mock(
        &mut server,
        "/me/contactFolders/DERIVED/childFolders?$top=100",
        r#"{"value":[]}"#,
    );
    json_mock(
        &mut server,
        "/me/contactFolders/DERIVED/contacts?$top=100&$select=id",
        r#"{"value":[{"id":"C1"}]}"#,
    );
    json_mock(
        &mut server,
        "/me/contacts/C1",
        r#"{"id":"C1","displayName":"Alice"}"#,
    );

    let base = server.url();
    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    make_config(base, None, surfaces("contacts"))
        .run(make_common(archive.clone()))
        .into_result()
        .unwrap();

    let db = Archive::open(&archive).unwrap();
    let conn = db.connection();
    let name: String = conn
        .query_row("SELECT name FROM address_books", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        name, "Kontakte",
        "when the well-known name is unavailable the folder is derived from a contact's parent"
    );
}

#[test]
fn contact_folder_reachable_by_two_paths_is_inserted_once() {
    let mut server = Server::new();
    stub_principal(&mut server);
    json_mock(
        &mut server,
        "/me/contactFolders?$top=100",
        r#"{"value":[{"id":"CHILD","displayName":"Work","parentFolderId":"DEFAULT"}]}"#,
    );
    json_mock(
        &mut server,
        "/me/contactFolders/contacts",
        r#"{"id":"DEFAULT","displayName":"Contacts","wellKnownName":"contacts"}"#,
    );
    json_mock(
        &mut server,
        "/me/contactFolders/DEFAULT/childFolders?$top=100",
        r#"{"value":[{"id":"CHILD","displayName":"Work","parentFolderId":"DEFAULT"}]}"#,
    );
    json_mock(
        &mut server,
        "/me/contactFolders/CHILD/childFolders?$top=100",
        r#"{"value":[]}"#,
    );
    for folder in ["DEFAULT", "CHILD"] {
        json_mock(
            &mut server,
            &format!("/me/contactFolders/{folder}/contacts?$top=100&$select=id"),
            r#"{"value":[]}"#,
        );
    }

    let base = server.url();
    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    let summary = make_config(base, None, surfaces("contacts"))
        .run(make_common(archive.clone()))
        .into_result()
        .expect("a folder reachable by two paths must not violate the id-mapping uniqueness");

    let created = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == ObjectType::AddressBook)
        .map(|(_, c)| c.created)
        .unwrap_or(0);
    assert_eq!(
        created, 2,
        "the default folder and its one child, each once"
    );
}

#[test]
fn message_state_becomes_jmap_keywords() {
    let mut server = Server::new();
    stub_principal(&mut server);
    json_mock(
        &mut server,
        "/me/mailFolders?$top=100&includeHiddenFolders=true",
        r#"{"value":[{"id":"F1","displayName":"Inbox","isHidden":false}]}"#,
    );
    json_mock(
        &mut server,
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
        &mut server,
        "/me/mailFolders/F1/messages?$top=100&$select=id,isRead,isDraft,flag,categories",
        r#"{"value":[
            {"id":"M1","isRead":true,"isDraft":false,"isReadReceiptRequested":true,
             "flag":{"flagStatus":"flagged"},"categories":["Red Category","VIP"]},
            {"id":"M2","isRead":false,"isDraft":true,
             "flag":{"flagStatus":"notFlagged"},"categories":[]}
        ]}"#,
    );
    for id in ["M1", "M2"] {
        server
            .mock("GET", format!("/me/messages/{id}/$value").as_str())
            .with_status(200)
            .with_header("content-type", "text/plain")
            .with_body(format!(
                "From: a@x.com\r\nTo: b@x.com\r\nSubject: {id}\r\nMessage-ID: <{id}@x>\r\n\r\nbody\r\n"
            ))
            .expect_at_least(0)
            .create();
    }

    let base = server.url();
    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    make_config(base, None, surfaces("mail"))
        .run(make_common(archive.clone()))
        .into_result()
        .unwrap();

    let db = Archive::open(&archive).unwrap();
    let conn = db.connection();
    let mut stmt = conn
        .prepare("SELECT keywords FROM emails ORDER BY id")
        .unwrap();
    let rows: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    let all = rows.join(" ");
    for expected in ["$seen", "$flagged", "red_category", "vip", "$draft"] {
        assert!(all.contains(expected), "missing {expected} in {all}");
    }
    assert!(
        !all.contains("$notified"),
        "a read-receipt request is not a keyword: {all}"
    );
}

#[test]
fn drive_items_import_as_file_nodes_and_skip_facetless_items() {
    let mut server = Server::new();
    stub_principal(&mut server);
    json_mock(
        &mut server,
        "/me/drive/root?$select=id,name",
        r#"{"id":"ROOT","name":"root"}"#,
    );
    let select = "id,name,size,folder,file,package,remoteItem,createdDateTime,lastModifiedDateTime";
    json_mock(
        &mut server,
        &format!("/me/drive/items/ROOT/children?$top=100&$select={select}"),
        r#"{"value":[
            {"id":"D1","name":"Docs","folder":{"childCount":1},
             "createdDateTime":"2026-01-01T00:00:00Z","lastModifiedDateTime":"2026-01-02T00:00:00Z"},
            {"id":"V1","name":"Personal Vault","remoteItem":{},
             "createdDateTime":"2026-01-01T00:00:00Z"},
            {"id":"F1","name":"top.txt","size":5,"file":{"mimeType":"text/plain"},
             "createdDateTime":"2026-01-01T00:00:00Z","lastModifiedDateTime":"2026-01-02T00:00:00Z"}
        ]}"#,
    );
    json_mock(
        &mut server,
        &format!("/me/drive/items/D1/children?$top=100&$select={select}"),
        r#"{"value":[
            {"id":"F2","name":"inner.bin","size":3,"file":{"mimeType":"application/octet-stream"},
             "createdDateTime":"2026-01-01T00:00:00Z"}
        ]}"#,
    );
    for (id, body) in [("F1", "hello"), ("F2", "abc")] {
        server
            .mock("GET", format!("/me/drive/items/{id}/content").as_str())
            .with_status(200)
            .with_header("content-type", "application/octet-stream")
            .with_body(body)
            .expect_at_least(0)
            .create();
    }

    let base = server.url();
    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    let summary = make_config(base, None, surfaces("files"))
        .run(make_common(archive.clone()))
        .into_result()
        .unwrap();

    let created = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == ObjectType::FileNode)
        .map(|(_, c)| c.created)
        .unwrap_or(0);
    assert_eq!(
        created, 3,
        "one directory and two files; the remoteItem has no file or folder facet and is skipped"
    );

    let db = Archive::open(&archive).unwrap();
    let conn = db.connection();
    let vault: i64 = conn
        .query_row(
            "SELECT count(*) FROM file_nodes WHERE name = 'Personal Vault'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(vault, 0, "a facetless drive item must never become a node");

    let nested: i64 = conn
        .query_row(
            "SELECT count(*) FROM file_nodes child JOIN file_nodes parent
               ON child.parent_id = parent.id
             WHERE child.name = 'inner.bin' AND parent.name = 'Docs'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(nested, 1, "child files hang off their directory");

    let body: Vec<u8> = conn
        .query_row(
            "SELECT b.data FROM file_nodes f JOIN blobs b ON b.id = f.blob_id
             WHERE f.name = 'top.txt'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(body, b"hello", "file content is stored verbatim");
}

fn stub_empty_calendar_scaffold(server: &mut Server) {
    stub_principal(server);
    json_mock(
        server,
        "/me/mailFolders?$top=100&includeHiddenFolders=true",
        r#"{"value":[]}"#,
    );
    json_mock(
        server,
        "/me/mailboxSettings?$select=timeZone",
        r#"{"timeZone":"UTC"}"#,
    );
    json_mock(server, "/me/contactFolders?$top=100", r#"{"value":[]}"#);
}

#[test]
fn a_deleted_occurrence_becomes_an_excluded_override() {
    let mut server = Server::new();
    stub_empty_calendar_scaffold(&mut server);
    json_mock(
        &mut server,
        "/me/calendars?$top=100",
        r#"{"value":[{"id":"CAL1","name":"Calendar","isDefaultCalendar":true}]}"#,
    );
    json_mock(
        &mut server,
        "/me/calendars/CAL1/events?$top=100&$select=id,type,seriesMasterId",
        r#"{"value":[{"id":"MASTER","type":"seriesMaster","iCalUId":"uid-master"}]}"#,
    );
    let today = chrono::Utc::now().date_naive();
    let start = today + chrono::Duration::days(10);
    json_mock(
        &mut server,
        "/me/events/MASTER",
        &format!(
            r#"{{
                "id":"MASTER","iCalUId":"uid-master","type":"seriesMaster",
                "subject":"Daily standup",
                "start":{{"dateTime":"{start}T09:00:00.0000000","timeZone":"UTC"}},
                "end":{{"dateTime":"{start}T09:30:00.0000000","timeZone":"UTC"}},
                "recurrence":{{
                    "pattern":{{"type":"daily","interval":1}},
                    "range":{{"type":"numbered","startDate":"{start}",
                              "numberOfOccurrences":4}}
                }}
            }}"#,
            start = start.format("%Y-%m-%d")
        ),
    );
    server
        .mock(
            "GET",
            Matcher::Regex(r"calendarView.*type%20eq%20%27exception%27".to_owned()),
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .expect_at_least(1)
        .create();
    // Graph returns three of the four occurrences: the third was deleted, and a
    // deleted occurrence is simply absent rather than reported as cancelled.
    let occurrences = format!(
        r#"{{"value":[
            {{"id":"O1","type":"occurrence","seriesMasterId":"MASTER",
              "start":{{"dateTime":"{d0}T09:00:00.0000000","timeZone":"UTC"}}}},
            {{"id":"O2","type":"occurrence","seriesMasterId":"MASTER",
              "start":{{"dateTime":"{d1}T09:00:00.0000000","timeZone":"UTC"}}}},
            {{"id":"O4","type":"occurrence","seriesMasterId":"MASTER",
              "start":{{"dateTime":"{d3}T09:00:00.0000000","timeZone":"UTC"}}}}
        ]}}"#,
        d0 = start.format("%Y-%m-%d"),
        d1 = (start + chrono::Duration::days(1)).format("%Y-%m-%d"),
        d3 = (start + chrono::Duration::days(3)).format("%Y-%m-%d"),
    );
    server
        .mock(
            "GET",
            Matcher::Regex(r"calendarView.*type%20eq%20%27occurrence%27".to_owned()),
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(occurrences)
        .expect_at_least(1)
        .create();

    let base = server.url();
    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    make_config(base, None, surfaces("calendar"))
        .run(make_common(archive.clone()))
        .into_result()
        .unwrap();

    let db = Archive::open(&archive).unwrap();
    let conn = db.connection();
    let row: String = conn
        .query_row("SELECT data FROM calendar_events", [], |row| row.get(0))
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&row).unwrap();
    let overrides = v["recurrenceOverrides"]
        .as_object()
        .expect("the missing occurrence produces an override");
    let missing = format!(
        "{}T09:00:00",
        (start + chrono::Duration::days(2)).format("%Y-%m-%d")
    );
    assert_eq!(
        overrides.len(),
        1,
        "only the absent occurrence is excluded; got {:?}",
        overrides.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        overrides[&missing]["excluded"], true,
        "v1.0 cannot report a cancelled occurrence, so it is inferred from the gap \
         between the expanded rule and the occurrences Graph returned"
    );
}

#[test]
fn event_file_attachments_become_enclosure_links() {
    let mut server = Server::new();
    stub_empty_calendar_scaffold(&mut server);
    json_mock(
        &mut server,
        "/me/calendars?$top=100",
        r#"{"value":[{"id":"CAL1","name":"Calendar","isDefaultCalendar":true}]}"#,
    );
    json_mock(
        &mut server,
        "/me/calendars/CAL1/events?$top=100&$select=id,type,seriesMasterId",
        r#"{"value":[{"id":"EV","type":"singleInstance","iCalUId":"uid-ev"}]}"#,
    );
    json_mock(
        &mut server,
        "/me/events/EV",
        r#"{"id":"EV","iCalUId":"uid-ev","type":"singleInstance","subject":"With a file",
            "hasAttachments":true,
            "start":{"dateTime":"2026-05-04T15:00:00.0000000","timeZone":"UTC"},
            "end":{"dateTime":"2026-05-04T16:00:00.0000000","timeZone":"UTC"}}"#,
    );
    json_mock(
        &mut server,
        "/me/events/EV/attachments",
        r##"{"value":[
            {"@odata.type":"#microsoft.graph.fileAttachment","id":"A1","name":"agenda.txt",
             "contentType":"text/plain","size":5,"contentBytes":"aGVsbG8="},
            {"@odata.type":"#microsoft.graph.itemAttachment","id":"A2","name":"nested.msg"},
            {"@odata.type":"#microsoft.graph.referenceAttachment","id":"A3","name":"link"}
        ]}"##,
    );

    let base = server.url();
    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    make_config(base, None, surfaces("calendar"))
        .run(make_common(archive.clone()))
        .into_result()
        .unwrap();

    let db = Archive::open(&archive).unwrap();
    let conn = db.connection();
    let row: String = conn
        .query_row("SELECT data FROM calendar_events", [], |row| row.get(0))
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&row).unwrap();
    let links = v["links"].as_object().expect("links present");
    assert_eq!(
        links.len(),
        1,
        "only fileAttachment carries bytes; item and reference attachments are skipped"
    );
    let link = &links["1"];
    assert_eq!(link["rel"], "enclosure");
    assert_eq!(link["title"], "agenda.txt");
    assert_eq!(link["contentType"], "text/plain");
    let blob: Vec<u8> = conn
        .query_row(
            "SELECT b.data FROM blobs b WHERE b.id = ?1",
            [link["@blob"].as_i64().unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(blob, b"hello");
}

#[test]
fn contact_photo_categories_and_im_addresses_are_imported() {
    let mut server = Server::new();
    stub_principal(&mut server);
    json_mock(
        &mut server,
        "/me/contactFolders?$top=100",
        r#"{"value":[]}"#,
    );
    json_mock(
        &mut server,
        "/me/contactFolders/contacts",
        r#"{"id":"DEFAULT","displayName":"Contacts","wellKnownName":"contacts"}"#,
    );
    json_mock(
        &mut server,
        "/me/contactFolders/DEFAULT/childFolders?$top=100",
        r#"{"value":[]}"#,
    );
    json_mock(
        &mut server,
        "/me/contactFolders/DEFAULT/contacts?$top=100&$select=id",
        r#"{"value":[{"id":"C1"}]}"#,
    );
    json_mock(
        &mut server,
        "/me/contacts/C1",
        r#"{"id":"C1","displayName":"Alice","categories":["Red Category","VIP"],
            "imAddresses":["sip:alice@x.com",""]}"#,
    );
    // Graph reports image/jpeg for every contact photo regardless of the real bytes,
    // so the stored mediaType has to come from the content itself.
    server
        .mock("GET", "/me/contacts/C1/photo/$value")
        .with_status(200)
        .with_header("content-type", "image/jpeg")
        .with_body(b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR".as_slice())
        .expect_at_least(1)
        .create();

    let base = server.url();
    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    let mut config = make_config(base, None, surfaces("contacts"));
    config.contact_photos = vandelay::import::graph::ContactPhotos::Fetch;
    config
        .run(make_common(archive.clone()))
        .into_result()
        .unwrap();

    let db = Archive::open(&archive).unwrap();
    let conn = db.connection();
    let row: String = conn
        .query_row("SELECT data FROM contact_cards", [], |row| row.get(0))
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&row).unwrap();
    assert_eq!(v["keywords"]["red category"], true);
    assert_eq!(v["keywords"]["vip"], true);
    assert_eq!(
        v["onlineServices"]["1"]["uri"], "sip:alice@x.com",
        "RFC 9553 2.3.2: a URI-shaped IM address belongs in uri, not user"
    );
    assert!(
        v["onlineServices"].as_object().unwrap().len() == 1,
        "the empty IM entry is dropped"
    );
    assert_eq!(v["media"]["photo"]["kind"], "photo");
    assert_eq!(
        v["media"]["photo"]["mediaType"], "image/png",
        "the type is sniffed from the bytes, not taken from Graph's jpeg header"
    );
}

// With one connection, new messages get local ids in the server's enumeration order.
#[test]
fn single_connection_mail_import_keeps_server_order() {
    let mut server = Server::new();
    let _principal = server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"uid-order","userPrincipalName":"alice@x.com"}"#)
        .create();
    let _folders = server
        .mock("GET", "/me/mailFolders?$top=100&includeHiddenFolders=true")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[{"id":"FMAIL","displayName":"Inbox","isHidden":false}]}"#)
        .create();
    let _children = server
        .mock(
            "GET",
            "/me/mailFolders/FMAIL/childFolders?$top=100&includeHiddenFolders=true",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();
    let _well_known = server
        .mock(
            "GET",
            Matcher::Regex(r"^/me/mailFolders/[a-z]+\?\$select=id$".to_owned()),
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"OTHER"}"#)
        .create();
    let order = [
        "MSG-F", "MSG-B", "MSG-E", "MSG-A", "MSG-D", "MSG-C", "MSG-H", "MSG-G",
    ];
    let stubs: Vec<_> = order.iter().map(|id| json!({ "id": id })).collect();
    let _ids = server
        .mock(
            "GET",
            "/me/mailFolders/FMAIL/messages?$top=100&$select=id,isRead,isDraft,flag,categories",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(json!({ "value": stubs }).to_string())
        .create();
    let _value = server
        .mock(
            "GET",
            Matcher::Regex(r"^/me/messages/MSG-[A-H]/\$value$".to_owned()),
        )
        .with_status(200)
        .with_header("content-type", "text/plain")
        .with_body("From: a@x\r\nSubject: hi\r\n\r\nbody")
        .create();

    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    let config = vandelay::import::graph::GraphImport {
        graph_connections: 1,
        ..make_config(server.url(), None, surfaces("mail"))
    };
    config
        .run(vandelay::sync::RunOptions {
            archive: archive.clone(),
            threads: 1,
            mode: RunMode::Apply,
            max_retries: 0,
            tls: TlsPolicy::Verify,
            logger: Logger::new(Level::Quiet),
        })
        .into_result()
        .unwrap();

    let db = Archive::open(&archive).unwrap();
    let mut stmt = db
        .connection()
        .prepare(
            "SELECT graph_id FROM sync_id_exchange_graph WHERE type_name = 'email' ORDER BY local_id",
        )
        .unwrap();
    let imported: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(imported, order);
}

// A mail folder whose parent is not enumerated is still imported, as a root.
#[test]
fn mail_folder_with_missing_parent_is_imported_as_root() {
    let mut server = Server::new();
    let _principal = server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"uid-orphan","userPrincipalName":"alice@x.com"}"#)
        .create();
    let _folders = server
        .mock("GET", "/me/mailFolders?$top=100&includeHiddenFolders=true")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"value":[
                {"id":"ORPHAN","displayName":"Orphan","parentFolderId":"MISSING","isHidden":false},
                {"id":"ROOT","displayName":"Root","isHidden":false}
            ]}"#,
        )
        .create();
    let _children = server
        .mock(
            "GET",
            Matcher::Regex(r"^/me/mailFolders/[A-Z]+/childFolders\?".to_owned()),
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();
    let _well_known = server
        .mock(
            "GET",
            Matcher::Regex(r"^/me/mailFolders/[a-z]+\?\$select=id$".to_owned()),
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"OTHER"}"#)
        .create();
    let _messages = server
        .mock(
            "GET",
            Matcher::Regex(r"^/me/mailFolders/[A-Z]+/messages\?".to_owned()),
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();

    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    let summary = make_config(server.url(), None, surfaces("mail"))
        .run(vandelay::sync::RunOptions {
            archive: archive.clone(),
            threads: 1,
            mode: RunMode::Apply,
            max_retries: 0,
            tls: TlsPolicy::Verify,
            logger: Logger::new(Level::Quiet),
        })
        .into_result()
        .unwrap();
    let created = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == ObjectType::Mailbox)
        .map(|(_, c)| c.created);
    assert_eq!(created, Some(2));

    let db = Archive::open(&archive).unwrap();
    let parent: Option<i64> = db
        .connection()
        .query_row(
            "SELECT m.parent_id FROM mailboxes m
             JOIN sync_id_exchange_graph s ON s.local_id = m.id AND s.type_name = 'mailbox'
             WHERE s.graph_id = 'ORPHAN'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(parent, None);
}

// Moving the default calendar, or losing the old default, leaves exactly the server's default flagged.
#[test]
fn moved_default_calendar_keeps_one_default() {
    let mut server = Server::new();
    let _principal = server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"uid-default","userPrincipalName":"alice@x.com"}"#)
        .create();
    let _settings = server
        .mock("GET", "/me/mailboxSettings?$select=timeZone")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"timeZone":"UTC"}"#)
        .create();
    let _events = server
        .mock(
            "GET",
            Matcher::Regex(r"^/me/calendars/[A-Z]+/events\?".to_owned()),
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();
    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    let mut run = |calendars: &str| {
        let listing = server
            .mock("GET", "/me/calendars?$top=100")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(calendars)
            .create();
        make_config(server.url(), None, surfaces("calendar"))
            .run(vandelay::sync::RunOptions {
                archive: archive.clone(),
                threads: 1,
                mode: RunMode::Apply,
                max_retries: 0,
                tls: TlsPolicy::Verify,
                logger: Logger::new(Level::Quiet),
            })
            .into_result()
            .unwrap();
        listing.remove();
        let db = Archive::open(&archive).unwrap();
        let mut stmt = db
            .connection()
            .prepare(
                "SELECT s.graph_id, c.is_default FROM calendars c
                 JOIN sync_id_exchange_graph s ON s.local_id = c.id AND s.type_name = 'calendar'
                 ORDER BY s.graph_id",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    };
    assert_eq!(
        run(r#"{"value":[{"id":"A","name":"A","isDefaultCalendar":true},{"id":"B","name":"B"}]}"#),
        [("A".to_owned(), true), ("B".to_owned(), false)]
    );
    assert_eq!(
        run(
            r#"{"value":[{"id":"B","name":"B","isDefaultCalendar":true},{"id":"A","name":"A","isDefaultCalendar":false}]}"#
        ),
        [("A".to_owned(), false), ("B".to_owned(), true)]
    );
    assert_eq!(
        run(r#"{"value":[{"id":"C","name":"C","isDefaultCalendar":true},{"id":"A","name":"A"}]}"#),
        [("A".to_owned(), false), ("C".to_owned(), true)]
    );
}
