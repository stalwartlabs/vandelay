/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::sync::Once;

use base64::Engine;
use mockito::{Matcher, Server};
use serde_json::json;
use vandelay::exchange_graph::api::{Endpoints, collect_all_ids, paged_collect};
use vandelay::exchange_graph::calendar_map::{EventType, classify_event_type, convert_event};
use vandelay::exchange_graph::client::{Accept, GraphClient};
use vandelay::exchange_graph::contact_map::convert_contact;
use vandelay::exchange_graph::error::GraphError;
use vandelay::exchange_graph::oauth::{
    DeviceCodeResponse, TokenResponse, parse_device_code_response, parse_token_response,
    run_device_code_polling_against,
};
use vandelay::exchange_graph::recurrence::convert_patterned_recurrence;
use vandelay::exchange_graph::retry::{HttpClass, classify_http_status};
use vandelay::exchange_graph::types::Surfaces;
use vandelay::jmap::http::RetryPolicy;

static INIT: Once = Once::new();

fn client_with_retries(retries: u32) -> GraphClient {
    INIT.call_once(|| {});
    GraphClient::new("BEARER".to_owned(), RetryPolicy::new(retries), false)
}

fn url_message_collection(server_url: &str, folder: &str, top: usize) -> String {
    let e = Endpoints::for_me(server_url);
    e.folder_messages_ids(folder, top)
}

fn make_jwt(exp: u64, upn: &str) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
    let claims = format!(r#"{{"tid":"tenant-1","upn":"{upn}","exp":{exp}}}"#);
    let payload = URL_SAFE_NO_PAD.encode(claims.as_bytes());
    format!("{header}.{payload}.")
}

#[test]
fn http_status_classifier_matches_spec_table() {
    assert_eq!(classify_http_status(200), HttpClass::Success);
    assert_eq!(classify_http_status(401), HttpClass::Auth);
    assert_eq!(classify_http_status(403), HttpClass::Auth);
    assert_eq!(classify_http_status(404), HttpClass::Vanished);
    assert_eq!(classify_http_status(410), HttpClass::Vanished);
    assert_eq!(classify_http_status(429), HttpClass::Retryable);
    assert_eq!(classify_http_status(503), HttpClass::Retryable);
    assert_eq!(classify_http_status(504), HttpClass::Retryable);
    assert_eq!(classify_http_status(507), HttpClass::Fatal);
    assert_eq!(classify_http_status(400), HttpClass::Fatal);
    assert_eq!(classify_http_status(405), HttpClass::Fatal);
    assert_eq!(classify_http_status(422), HttpClass::Fatal);
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
    let parsed = parse_device_code_response(&body).unwrap();
    assert_eq!(parsed.device_code, "ABCDEF");
    assert_eq!(parsed.user_code, "BR4S-7XYZ");
    assert_eq!(parsed.interval, 5);
    assert_eq!(parsed.expires_in, 900);
}

#[test]
fn token_response_handles_pending_and_success() {
    let pending = parse_token_response(400, &json!({"error": "authorization_pending"}));
    assert!(matches!(pending, TokenResponse::Pending));
    let success = parse_token_response(
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
    let declined = parse_token_response(400, &json!({"error": "authorization_declined"}));
    assert!(matches!(declined, TokenResponse::Declined));
    let bad = parse_token_response(400, &json!({"error": "bad_verification_code"}));
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
    let token = run_device_code_polling_against(
        "https://login.x/common",
        "client-id",
        &response,
        |_endpoint, _body| {
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
        },
    )
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
    let err = run_device_code_polling_against(
        "https://login.x/common",
        "client-id",
        &response,
        |_, _| Ok((400, json!({"error": "authorization_declined"}))),
    )
    .unwrap_err();
    let msg = err.to_string();
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
        .mock("GET", "/me/mailFolders/F1/messages?$top=100&$select=id,isRead,isDraft,isReadReceiptRequested,flag,categories")
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
    let ids = collect_all_ids(&client, &url, &[]).unwrap();
    assert_eq!(ids, vec!["M1".to_owned(), "M2".to_owned(), "M3".to_owned()]);
}

#[test]
fn pagination_terminates_when_no_next_link_present() {
    let mut server = Server::new();
    let base = server.url();
    let body = json!({"value": [{"id": "A"}, {"id": "B"}]}).to_string();
    let _m = server
        .mock("GET", "/me/mailFolders/F/messages?$top=100&$select=id,isRead,isDraft,isReadReceiptRequested,flag,categories")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(body)
        .create();
    let client = client_with_retries(0);
    let url = url_message_collection(&base, "F", 100);
    let mut pages = 0;
    paged_collect(&client, &url, &[], |_| {
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
        .mock("GET", "/me/mailFolders/F/messages?$top=100&$select=id,isRead,isDraft,isReadReceiptRequested,flag,categories")
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
    let ids = collect_all_ids(&c, &url, &[]).unwrap();
    assert_eq!(ids.len(), 2);
}

#[test]
fn case_sensitive_immutable_id_round_trip() {
    let mut server = Server::new();
    let id = "AAkAAGFsaWNlAAA=";
    let body = json!({"value": [{"id": id}]}).to_string();
    let _m = server
        .mock("GET", "/me/mailFolders/F/messages?$top=100&$select=id,isRead,isDraft,isReadReceiptRequested,flag,categories")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(body)
        .create();
    let base = server.url();
    let client = client_with_retries(0);
    let url = url_message_collection(&base, "F", 100);
    let ids = collect_all_ids(&client, &url, &[]).unwrap();
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
    assert!(client.retries_observed() >= 1);
    assert!(client.retry_after_sleeps() >= 1);
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
    assert!(client.retries_observed() >= 1);
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
    let out = convert_patterned_recurrence(&pr).unwrap();
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
    let out = convert_patterned_recurrence(&pr).unwrap();
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
    let out = convert_patterned_recurrence(&pr).unwrap();
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
    let out = convert_patterned_recurrence(&pr).unwrap();
    assert_eq!(out["frequency"], "yearly");
    assert_eq!(out["until"], "2030-12-25T23:59:59");
}

#[test]
fn occurrence_event_is_classified_for_skip() {
    let occ = json!({"id": "occ-1", "type": "occurrence", "seriesMasterId": "master-1"});
    assert_eq!(classify_event_type(&occ), EventType::Occurrence);
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
    let conv = convert_event(&ex, None).unwrap();
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
    let out = convert_contact(&c).unwrap();
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
    let common = vandelay::sync::CommonConfig {
        archive: tempfile::NamedTempFile::new().unwrap().path().to_owned(),
        threads: 2,
        dry_run: true,
        max_retries: 0,
        allow_invalid_certs: false,
        logger: vandelay::logging::Logger::new(0),
    };
    let config = vandelay::sync::import_exchange_graph::GraphImportConfig {
        auth: vandelay::sync::import_exchange_graph::GraphAuth::PreAcquired {
            token: make_jwt(9999999999, "alice@x.com"),
        },
        api_base: base.clone(),
        user_target: None,
        mailbox_kind: vandelay::exchange_graph::types::MailboxKind::Primary,
        surfaces: Surfaces::ALL,
        event_body_format: vandelay::exchange_graph::types::EventBodyFormat::Text,
        graph_connections: 2,
        top: 100,
        exception_window_years: 5,
        allow_source_change: false,
    };
    let summary = vandelay::sync::import_exchange_graph::run(common, config).unwrap();
    let mailbox = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "mailbox")
        .map(|(_, c)| c.created)
        .unwrap_or(0);
    let calendar = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "calendar")
        .map(|(_, c)| c.created)
        .unwrap_or(0);
    let addressbook = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "addressbook")
        .map(|(_, c)| c.created)
        .unwrap_or(0);
    assert_eq!(mailbox, 1);
    assert_eq!(calendar, 1);
    assert_eq!(addressbook, 1);
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
        .mock("GET", "/me/mailFolders/FMAIL/messages?$top=100&$select=id,isRead,isDraft,isReadReceiptRequested,flag,categories")
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
    let common = vandelay::sync::CommonConfig {
        archive: archive_path.clone(),
        threads: 2,
        dry_run: false,
        max_retries: 0,
        allow_invalid_certs: false,
        logger: vandelay::logging::Logger::new(0),
    };
    let config = vandelay::sync::import_exchange_graph::GraphImportConfig {
        auth: vandelay::sync::import_exchange_graph::GraphAuth::PreAcquired {
            token: make_jwt(9999999999, "alice@x.com"),
        },
        api_base: server.url(),
        user_target: None,
        mailbox_kind: vandelay::exchange_graph::types::MailboxKind::Primary,
        surfaces: surfaces("mail"),
        event_body_format: vandelay::exchange_graph::types::EventBodyFormat::Text,
        graph_connections: 2,
        top: 100,
        exception_window_years: 5,
        allow_source_change: false,
    };
    drop(tmp);
    let summary = vandelay::sync::import_exchange_graph::run(common, config).unwrap();
    let emails = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "email")
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
        .mock("GET", "/me/mailFolders/FMAIL/messages?$top=100&$select=id,isRead,isDraft,isReadReceiptRequested,flag,categories")
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
    let common = vandelay::sync::CommonConfig {
        archive: archive_path.clone(),
        threads: 2,
        dry_run: false,
        max_retries: 0,
        allow_invalid_certs: false,
        logger: vandelay::logging::Logger::new(0),
    };
    let config = vandelay::sync::import_exchange_graph::GraphImportConfig {
        auth: vandelay::sync::import_exchange_graph::GraphAuth::PreAcquired {
            token: make_jwt(9999999999, "alice@x.com"),
        },
        api_base: server.url(),
        user_target: None,
        mailbox_kind: vandelay::exchange_graph::types::MailboxKind::Primary,
        surfaces: surfaces("mail"),
        event_body_format: vandelay::exchange_graph::types::EventBodyFormat::Text,
        graph_connections: 2,
        top: 100,
        exception_window_years: 5,
        allow_source_change: false,
    };
    drop(tmp);
    let summary = vandelay::sync::import_exchange_graph::run(common, config).unwrap();
    let emails = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "email")
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
        .mock("GET", "/me/mailFolders/FMAIL/messages?$top=100&$select=id,isRead,isDraft,isReadReceiptRequested,flag,categories")
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
    let make_config = || vandelay::sync::import_exchange_graph::GraphImportConfig {
        auth: vandelay::sync::import_exchange_graph::GraphAuth::PreAcquired {
            token: make_jwt(9999999999, "alice@x.com"),
        },
        api_base: api_base.clone(),
        user_target: None,
        mailbox_kind: vandelay::exchange_graph::types::MailboxKind::Primary,
        surfaces: surfaces("mail"),
        event_body_format: vandelay::exchange_graph::types::EventBodyFormat::Text,
        graph_connections: 2,
        top: 100,
        exception_window_years: 5,
        allow_source_change: false,
    };
    let make_common = |path: std::path::PathBuf| vandelay::sync::CommonConfig {
        archive: path,
        threads: 2,
        dry_run: false,
        max_retries: 0,
        allow_invalid_certs: false,
        logger: vandelay::logging::Logger::new(0),
    };

    let first =
        vandelay::sync::import_exchange_graph::run(make_common(archive.clone()), make_config())
            .unwrap();
    let created_first = first
        .per_type
        .iter()
        .find(|(t, _)| *t == "email")
        .map(|(_, c)| c.created)
        .unwrap_or(0);
    assert_eq!(created_first, 1);

    let second =
        vandelay::sync::import_exchange_graph::run(make_common(archive), make_config()).unwrap();
    let created_second = second
        .per_type
        .iter()
        .find(|(t, _)| *t == "email")
        .map(|(_, c)| c.created)
        .unwrap_or(0);
    assert_eq!(created_second, 0, "second run should not re-import");
}

#[test]
fn source_change_protection_refuses_a_different_account() {
    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    let conn = vandelay::db::init::open(&archive).unwrap();
    vandelay::db::sources::upsert_source(
        &conn,
        &vandelay::db::sources::SourceKey {
            kind: "exchange_graph".to_owned(),
            session_url:
                "https://login.microsoftonline.com/common|https://graph.microsoft.com/v1.0"
                    .to_owned(),
            account_id: "old-user-id".to_owned(),
        },
        Some("old@x.com"),
        "old@x.com",
    )
    .unwrap();
    drop(conn);

    let mut server = Server::new();
    let _principal = server
        .mock("GET", Matcher::Regex(r"^/me\?\$select=id".to_owned()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"new-user-id","userPrincipalName":"new@x.com"}"#)
        .create();

    let common = vandelay::sync::CommonConfig {
        archive,
        threads: 2,
        dry_run: false,
        max_retries: 0,
        allow_invalid_certs: false,
        logger: vandelay::logging::Logger::new(0),
    };
    let config = vandelay::sync::import_exchange_graph::GraphImportConfig {
        auth: vandelay::sync::import_exchange_graph::GraphAuth::PreAcquired {
            token: make_jwt(9999999999, "new@x.com"),
        },
        api_base: server.url(),
        user_target: None,
        mailbox_kind: vandelay::exchange_graph::types::MailboxKind::Primary,
        surfaces: Surfaces::ALL,
        event_body_format: vandelay::exchange_graph::types::EventBodyFormat::Text,
        graph_connections: 2,
        top: 100,
        exception_window_years: 5,
        allow_source_change: false,
    };
    let err = vandelay::sync::import_exchange_graph::run(common, config).unwrap_err();
    assert!(
        matches!(err, vandelay::error::Error::SourceChange(_)),
        "expected SourceChange error, got {err:?}"
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

fn make_common(archive: std::path::PathBuf) -> vandelay::sync::CommonConfig {
    vandelay::sync::CommonConfig {
        archive,
        threads: 2,
        dry_run: false,
        max_retries: 0,
        allow_invalid_certs: false,
        logger: vandelay::logging::Logger::new(0),
    }
}

fn surfaces(list: &str) -> vandelay::exchange_graph::types::Surfaces {
    vandelay::exchange_graph::types::Surfaces::parse_list(list).unwrap()
}

fn make_config(
    api_base: String,
    user_target: Option<String>,
    surfaces: vandelay::exchange_graph::types::Surfaces,
) -> vandelay::sync::import_exchange_graph::GraphImportConfig {
    vandelay::sync::import_exchange_graph::GraphImportConfig {
        auth: vandelay::sync::import_exchange_graph::GraphAuth::PreAcquired {
            token: make_jwt(9999999999, "alice@x.com"),
        },
        api_base,
        user_target,
        mailbox_kind: vandelay::exchange_graph::types::MailboxKind::Primary,
        surfaces,
        event_body_format: vandelay::exchange_graph::types::EventBodyFormat::Text,
        graph_connections: 2,
        top: 100,
        exception_window_years: 5,
        allow_source_change: false,
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
    let common = vandelay::sync::CommonConfig {
        archive: tempfile::NamedTempFile::new().unwrap().path().to_owned(),
        threads: 2,
        dry_run: true,
        max_retries: 0,
        allow_invalid_certs: false,
        logger: vandelay::logging::Logger::new(0),
    };
    let summary = vandelay::sync::import_exchange_graph::run(
        common,
        make_config(base, Some("alice@x.com".to_owned()), Surfaces::ALL),
    )
    .unwrap();
    let mailbox = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "mailbox")
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
    let summary = vandelay::sync::import_exchange_graph::run(
        make_common(archive.clone()),
        make_config(base, None, surfaces("calendar")),
    )
    .unwrap();
    let events = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "calendarevent")
        .map(|(_, c)| c.created)
        .unwrap_or(0);
    assert_eq!(events, 1, "exactly one master row, exception merged in");

    let conn = vandelay::db::init::open(&archive).unwrap();
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
    let summary = vandelay::sync::import_exchange_graph::run(
        make_common(archive.clone()),
        make_config(base, None, surfaces("calendar")),
    )
    .unwrap();
    let events = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "calendarevent")
        .map(|(_, c)| c.created)
        .unwrap_or(99);
    assert_eq!(events, 0, "occurrence must not be fetched or stored");
    occ_get.assert();

    let conn = vandelay::db::init::open(&archive).unwrap();
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
            "/me/mailFolders/FVISIBLE/messages?$top=100&$select=id,isRead,isDraft,isReadReceiptRequested,flag,categories",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();
    server
        .mock(
            "GET",
            "/me/mailFolders/FHIDDEN/messages?$top=100&$select=id,isRead,isDraft,isReadReceiptRequested,flag,categories",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();
    stub_well_known_folders(&mut server, "FVISIBLE");
    let base = server.url();
    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    let _ = vandelay::sync::import_exchange_graph::run(
        make_common(archive.clone()),
        make_config(base, None, surfaces("mail")),
    )
    .unwrap();
    let conn = vandelay::db::init::open(&archive).unwrap();
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
                format!("/me/mailFolders/{fid}/messages?$top=100&$select=id,isRead,isDraft,isReadReceiptRequested,flag,categories").as_str(),
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
    let _ = vandelay::sync::import_exchange_graph::run(
        make_common(archive.clone()),
        make_config(base, None, surfaces("mail")),
    )
    .unwrap();
    let conn = vandelay::db::init::open(&archive).unwrap();
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
        .mock("GET", "/me/mailFolders/FMAIL/messages?$top=100&$select=id,isRead,isDraft,isReadReceiptRequested,flag,categories")
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
    let summary = vandelay::sync::import_exchange_graph::run(
        make_common(archive),
        make_config(base, None, surfaces("mail")),
    )
    .unwrap();
    let emails = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "email")
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
    let _ = vandelay::sync::import_exchange_graph::run(
        make_common(archive),
        make_config(base, None, surfaces("calendar")),
    )
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
    let client = GraphClient::new("EXPIRED".to_owned(), RetryPolicy::new(0), false);
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
            "/me/mailFolders/AAkA-Padded%3D%3D/messages?$top=100&$select=id,isRead,isDraft,isReadReceiptRequested,flag,categories",
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"value":[]}"#)
        .create();

    let tmp = tempfile::NamedTempFile::new().unwrap();
    let archive = tmp.path().to_owned();
    let common = vandelay::sync::CommonConfig {
        archive: archive.clone(),
        threads: 2,
        dry_run: false,
        max_retries: 0,
        allow_invalid_certs: false,
        logger: vandelay::logging::Logger::new(0),
    };
    let config = vandelay::sync::import_exchange_graph::GraphImportConfig {
        auth: vandelay::sync::import_exchange_graph::GraphAuth::PreAcquired {
            token: make_jwt(9999999999, "pad@x.com"),
        },
        api_base: server.url(),
        user_target: None,
        mailbox_kind: vandelay::exchange_graph::types::MailboxKind::Primary,
        surfaces: surfaces("mail"),
        event_body_format: vandelay::exchange_graph::types::EventBodyFormat::Text,
        graph_connections: 2,
        top: 100,
        exception_window_years: 5,
        allow_source_change: false,
    };
    drop(tmp);
    let _ = vandelay::sync::import_exchange_graph::run(common, config).unwrap();
    let conn = vandelay::db::init::open(&archive).unwrap();
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
    let common = vandelay::sync::CommonConfig {
        archive: archive.clone(),
        threads: 2,
        dry_run: false,
        max_retries: 0,
        allow_invalid_certs: false,
        logger: vandelay::logging::Logger::new(0),
    };
    let config = vandelay::sync::import_exchange_graph::GraphImportConfig {
        auth: vandelay::sync::import_exchange_graph::GraphAuth::PreAcquired {
            token: make_jwt(9999999999, "a@x.com"),
        },
        api_base: server.url(),
        user_target: None,
        mailbox_kind: vandelay::exchange_graph::types::MailboxKind::Archive,
        surfaces: surfaces("mail"),
        event_body_format: vandelay::exchange_graph::types::EventBodyFormat::Text,
        graph_connections: 2,
        top: 100,
        exception_window_years: 5,
        allow_source_change: false,
    };
    drop(tmp);
    let _ = vandelay::sync::import_exchange_graph::run(common, config).unwrap();
    let conn = vandelay::db::init::open(&archive).unwrap();
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
    let conn = vandelay::db::init::open(&archive).unwrap();
    vandelay::db::sources::upsert_source(
        &conn,
        &vandelay::db::sources::SourceKey {
            kind: "exchange_graph".to_owned(),
            session_url: "https://login.microsoftonline.com/common|https://x.example".to_owned(),
            account_id: "different-uid".to_owned(),
        },
        Some("old@x.com"),
        "old@x.com",
    )
    .unwrap();
    drop(conn);

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

    let common = vandelay::sync::CommonConfig {
        archive: archive.clone(),
        threads: 2,
        dry_run: false,
        max_retries: 0,
        allow_invalid_certs: false,
        logger: vandelay::logging::Logger::new(0),
    };
    let config = vandelay::sync::import_exchange_graph::GraphImportConfig {
        auth: vandelay::sync::import_exchange_graph::GraphAuth::PreAcquired {
            token: make_jwt(9999999999, "new@x.com"),
        },
        api_base: server.url(),
        user_target: None,
        mailbox_kind: vandelay::exchange_graph::types::MailboxKind::Primary,
        surfaces: surfaces("mail"),
        event_body_format: vandelay::exchange_graph::types::EventBodyFormat::Text,
        graph_connections: 2,
        top: 100,
        exception_window_years: 5,
        allow_source_change: true,
    };
    let result = vandelay::sync::import_exchange_graph::run(common, config);
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
    let common = vandelay::sync::CommonConfig {
        archive: archive.clone(),
        threads: 2,
        dry_run: true,
        max_retries: 0,
        allow_invalid_certs: false,
        logger: vandelay::logging::Logger::new(0),
    };
    let config = vandelay::sync::import_exchange_graph::GraphImportConfig {
        auth: vandelay::sync::import_exchange_graph::GraphAuth::PreAcquired {
            token: make_jwt(9999999999, "a@x.com"),
        },
        api_base: server.url(),
        user_target: None,
        mailbox_kind: vandelay::exchange_graph::types::MailboxKind::Primary,
        surfaces: Surfaces::ALL,
        event_body_format: vandelay::exchange_graph::types::EventBodyFormat::Text,
        graph_connections: 2,
        top: 100,
        exception_window_years: 5,
        allow_source_change: false,
    };
    drop(tmp);
    let _ = vandelay::sync::import_exchange_graph::run(common, config).unwrap();
    no_messages.assert();
    no_well_known.assert();
    let conn = vandelay::db::init::open(&archive).unwrap();
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
    let other = parse_token_response(401, &json!({"error": "invalid_grant"}));
    match other {
        TokenResponse::Other { error, .. } => assert_eq!(error, "invalid_grant"),
        v => panic!("expected Other for invalid_grant, got {v:?}"),
    }
}

#[test]
fn token_response_expired_recognised() {
    let r = parse_token_response(400, &json!({"error": "expired_token"}));
    assert!(matches!(r, TokenResponse::Expired));
}

#[test]
fn token_response_bad_code_recognised() {
    let r = parse_token_response(400, &json!({"error": "bad_verification_code"}));
    assert!(matches!(r, TokenResponse::BadCode));
}

#[test]
fn folder_enumeration_failure_skips_vanished_deletion() {
    let archive = tempfile::NamedTempFile::new().unwrap().path().to_owned();
    let setup_conn = vandelay::db::init::open(&archive).unwrap();
    let sid = vandelay::db::sources::upsert_source(
        &setup_conn,
        &vandelay::db::sources::SourceKey {
            kind: "exchange_graph".to_owned(),
            session_url: "stale".to_owned(),
            account_id: "stale-uid".to_owned(),
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
    vandelay::db::exchange_graph_ids::insert(
        &setup_conn,
        sid,
        vandelay::db::exchange_graph_ids::MAILBOX,
        "FMAIL",
        42,
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
    vandelay::db::exchange_graph_ids::insert(
        &setup_conn,
        sid,
        vandelay::db::exchange_graph_ids::EMAIL,
        "MSG-LOCAL",
        1,
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
    drop(setup_conn);

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
        .mock("GET", "/me/mailFolders/FMAIL/messages?$top=100&$select=id,isRead,isDraft,isReadReceiptRequested,flag,categories")
        .with_status(503)
        .with_body("transient outage")
        .create();

    let common = vandelay::sync::CommonConfig {
        archive: archive.clone(),
        threads: 2,
        dry_run: false,
        max_retries: 0,
        allow_invalid_certs: false,
        logger: vandelay::logging::Logger::new(0),
    };
    let config = vandelay::sync::import_exchange_graph::GraphImportConfig {
        auth: vandelay::sync::import_exchange_graph::GraphAuth::PreAcquired {
            token: make_jwt(9999999999, "flake@x.com"),
        },
        api_base: server.url(),
        user_target: None,
        mailbox_kind: vandelay::exchange_graph::types::MailboxKind::Primary,
        surfaces: surfaces("mail"),
        event_body_format: vandelay::exchange_graph::types::EventBodyFormat::Text,
        graph_connections: 2,
        top: 100,
        exception_window_years: 5,
        allow_source_change: true,
    };
    let _ = vandelay::sync::import_exchange_graph::run(common, config).unwrap();
    let conn = vandelay::db::init::open(&archive).unwrap();
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
        .mock("GET", "/me/mailFolders/FMAIL/messages?$top=100&$select=id,isRead,isDraft,isReadReceiptRequested,flag,categories")
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
    vandelay::sync::import_exchange_graph::run(
        make_common(archive.clone()),
        make_config(base, None, surfaces("contacts")),
    )
    .unwrap();
    no_mail.assert();
    no_calendars.assert();

    let conn = vandelay::db::init::open(&archive).unwrap();
    assert_eq!(row_count(&conn, "address_books"), 1);
    assert_eq!(row_count(&conn, "contact_cards"), 1);
    assert_eq!(row_count(&conn, "mailboxes"), 0);
    assert_eq!(row_count(&conn, "emails"), 0);
    assert_eq!(row_count(&conn, "calendars"), 0);
    assert_eq!(row_count(&conn, "calendar_events"), 0);
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
    vandelay::sync::import_exchange_graph::run(
        make_common(archive.clone()),
        make_config(base, None, surfaces("mail")),
    )
    .unwrap();
    no_calendars.assert();
    no_contact_folders.assert();

    let conn = vandelay::db::init::open(&archive).unwrap();
    assert_eq!(row_count(&conn, "mailboxes"), 1);
    assert_eq!(row_count(&conn, "emails"), 1);
    assert_eq!(row_count(&conn, "address_books"), 0);
    assert_eq!(row_count(&conn, "contact_cards"), 0);
    assert_eq!(row_count(&conn, "calendars"), 0);
    assert_eq!(row_count(&conn, "calendar_events"), 0);
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
    let summary = vandelay::sync::import_exchange_graph::run(
        make_common(archive.clone()),
        make_config(base, None, surfaces("contacts")),
    )
    .unwrap();

    let count = |name: &str| {
        summary
            .per_type
            .iter()
            .find(|(t, _)| *t == name)
            .map(|(_, c)| c.created)
            .unwrap_or(0)
    };
    assert_eq!(
        count("addressbook"),
        1,
        "the default Contacts folder must become an address book even though \
         /me/contactFolders never lists it"
    );
    assert_eq!(
        count("contactcard"),
        2,
        "both default-folder contacts import"
    );

    let conn = vandelay::db::init::open(&archive).unwrap();
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
    vandelay::sync::import_exchange_graph::run(
        make_common(archive.clone()),
        make_config(base, None, surfaces("contacts")),
    )
    .unwrap();

    let conn = vandelay::db::init::open(&archive).unwrap();
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
    let summary = vandelay::sync::import_exchange_graph::run(
        make_common(archive.clone()),
        make_config(base, None, surfaces("contacts")),
    )
    .expect("a folder reachable by two paths must not violate the id-mapping uniqueness");

    let created = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "addressbook")
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
        "/me/mailFolders/F1/messages?$top=100&$select=id,isRead,isDraft,isReadReceiptRequested,flag,categories",
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
    vandelay::sync::import_exchange_graph::run(
        make_common(archive.clone()),
        make_config(base, None, surfaces("mail")),
    )
    .unwrap();

    let conn = vandelay::db::init::open(&archive).unwrap();
    let mut stmt = conn
        .prepare("SELECT keywords FROM emails ORDER BY id")
        .unwrap();
    let rows: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    let all = rows.join(" ");
    for expected in [
        "$seen",
        "$notified",
        "$flagged",
        "red category",
        "vip",
        "$draft",
    ] {
        assert!(all.contains(expected), "missing {expected} in {all}");
    }
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
    let summary = vandelay::sync::import_exchange_graph::run(
        make_common(archive.clone()),
        make_config(base, None, surfaces("files")),
    )
    .unwrap();

    let created = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "filenode")
        .map(|(_, c)| c.created)
        .unwrap_or(0);
    assert_eq!(
        created, 3,
        "one directory and two files; the remoteItem has no file or folder facet and is skipped"
    );

    let conn = vandelay::db::init::open(&archive).unwrap();
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
