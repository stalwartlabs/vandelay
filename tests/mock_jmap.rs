/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::time::Instant;

use serde_json::json;
use vandelay::jmap::account::{self, AccountSelector};
use vandelay::jmap::error::JmapError;
use vandelay::jmap::http::{Auth, HttpClient, RetryPolicy};
use vandelay::jmap::request::{
    self, SetRequest, get_all, get_changes, get_objects, get_state, set_call,
};
use vandelay::jmap::session::{Limits, Session};
use vandelay::jmap::wire::JmapId;
use vandelay::jmap::wire::identity::Identity;
use vandelay::jmap::wire::mailbox::Mailbox;

fn client(retries: u32) -> HttpClient {
    HttpClient::new(
        Auth::Basic {
            user: "u".into(),
            password: "p".into(),
        },
        RetryPolicy::new(retries),
        false,
    )
}

fn limits(get: u64) -> Limits {
    Limits {
        max_objects_in_get: get,
        max_objects_in_set: get,
        max_calls_in_request: 16,
        max_concurrent_requests: 4,
        max_concurrent_upload: 4,
        max_size_request: 10_000_000,
        max_size_upload: 50_000_000,
    }
}

fn limits_concurrency(req: u64, up: u64, max_upload: u64) -> Limits {
    Limits {
        max_objects_in_get: 500,
        max_objects_in_set: 500,
        max_calls_in_request: 16,
        max_concurrent_requests: req,
        max_concurrent_upload: up,
        max_size_request: 10_000_000,
        max_size_upload: max_upload,
    }
}

fn session_json(base: &str) -> String {
    json!({
        "apiUrl": format!("{base}/jmap/api"),
        "uploadUrl": format!("{base}/jmap/upload/{{accountId}}/"),
        "downloadUrl": format!("{base}/jmap/dl/{{accountId}}/{{blobId}}/{{type}}/{{name}}"),
        "capabilities": {
            "urn:ietf:params:jmap:core": {
                "maxObjectsInGet": 500, "maxObjectsInSet": 500, "maxCallsInRequest": 16,
                "maxConcurrentRequests": 4, "maxConcurrentUpload": 4,
                "maxSizeRequest": 10000000, "maxSizeUpload": 50000000
            },
            "urn:ietf:params:jmap:principals": {}
        },
        "accounts": { "w": { "name": "alice@example.org",
            "accountCapabilities": { "urn:ietf:params:jmap:mail": {} } } }
    })
    .to_string()
}

#[test]
fn discovery_follows_well_known_redirect_chain() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let root = server
        .mock("GET", "/")
        .with_status(404)
        .with_body("not a session")
        .expect(1)
        .create();
    let wk = server
        .mock("GET", "/.well-known/jmap")
        .with_status(308)
        .with_header("Location", "/jmap/session")
        .expect(1)
        .create();
    let sess = server
        .mock("GET", "/jmap/session")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(session_json(&base))
        .expect(1)
        .create();

    let session = Session::discover(&client(2), &base).expect("session discovered");
    assert_eq!(session.accounts["w"].name, "alice@example.org");
    assert!(session.supports("w", "urn:ietf:params:jmap:mail"));
    root.assert();
    wk.assert();
    sess.assert();
}

#[test]
fn anonymous_session_is_authentication_failure() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let body = json!({
        "apiUrl": "x", "uploadUrl": "u", "downloadUrl": "d",
        "capabilities": {}, "accounts": {}
    })
    .to_string();
    server
        .mock("GET", "/")
        .with_status(200)
        .with_body(body)
        .create();
    let err = Session::discover(&client(0), &base).unwrap_err();
    assert!(matches!(err, JmapError::Auth(_)), "got {err:?}");
}

fn page(ids: &[&str]) -> String {
    json!({ "methodResponses": [["Mailbox/query",
        { "accountId": "w", "ids": ids, "position": 0 }, "q"]] })
    .to_string()
}

fn trailing_empty_page(server: &mut mockito::Server, api: &str) {
    server
        .mock("POST", api)
        .with_status(200)
        .with_body(page(&[]))
        .create();
}

#[test]
fn query_paginates_with_clamped_limit_and_terminates_structurally() {
    let mut server = mockito::Server::new();
    let api = "/jmap/api";
    server
        .mock("POST", api)
        .with_status(200)
        .with_body(page(&["a", "b"]))
        .expect(1)
        .create();
    server
        .mock("POST", api)
        .with_status(200)
        .with_body(page(&["c", "d"]))
        .expect(1)
        .create();
    server
        .mock("POST", api)
        .with_status(200)
        .with_body(page(&["e"]))
        .expect(1)
        .create();
    trailing_empty_page(&mut server, api);

    let url = format!("{}{}", server.url(), api);
    let ids =
        request::query_all_ids(&client(2), &url, "w", "Mailbox", &limits(3)).expect("query ok");
    let got: Vec<String> = ids.into_iter().map(|i| i.0).collect();
    assert_eq!(got, vec!["a", "b", "c", "d", "e"]);
}

#[test]
fn anchor_not_found_restarts_pagination() {
    let mut server = mockito::Server::new();
    let api = "/jmap/api";
    server
        .mock("POST", api)
        .with_status(200)
        .with_body(page(&["a", "b"]))
        .expect(1)
        .create();
    server
        .mock("POST", api)
        .with_status(200)
        .with_body(
            json!({ "methodResponses": [["error", { "type": "anchorNotFound" }, "q"]] })
                .to_string(),
        )
        .expect(1)
        .create();
    server
        .mock("POST", api)
        .with_status(200)
        .with_body(page(&["a"]))
        .expect(1)
        .create();
    trailing_empty_page(&mut server, api);

    let url = format!("{}{}", server.url(), api);
    let ids =
        request::query_all_ids(&client(2), &url, "w", "Mailbox", &limits(2)).expect("query ok");
    assert_eq!(ids, vec![JmapId("a".into())]);
}

#[test]
fn rate_limit_with_retry_after_is_honoured_then_succeeds() {
    let mut server = mockito::Server::new();
    let api = "/jmap/api";
    server
        .mock("POST", api)
        .with_status(429)
        .with_header("Retry-After", "1")
        .with_body("{}")
        .expect(1)
        .create();
    server
        .mock("POST", api)
        .with_status(200)
        .with_body(page(&[]))
        .expect(1)
        .create();

    let url = format!("{}{}", server.url(), api);
    let started = Instant::now();
    let ids =
        request::query_all_ids(&client(3), &url, "w", "Mailbox", &limits(2)).expect("query ok");
    assert!(ids.is_empty());
    assert!(
        started.elapsed().as_millis() >= 900,
        "Retry-After of 1s should have delayed the retry"
    );
}

#[test]
fn server_unavailable_503_is_retried() {
    let mut server = mockito::Server::new();
    let api = "/jmap/api";
    server
        .mock("POST", api)
        .with_status(503)
        .with_body("upstream down")
        .expect(1)
        .create();
    server
        .mock("POST", api)
        .with_status(200)
        .with_body(page(&["only"]))
        .expect(1)
        .create();
    trailing_empty_page(&mut server, api);
    let url = format!("{}{}", server.url(), api);
    let ids =
        request::query_all_ids(&client(3), &url, "w", "Mailbox", &limits(2)).expect("query ok");
    assert_eq!(ids, vec![JmapId("only".into())]);
}

#[test]
fn request_too_large_halves_and_resplits_without_consuming_retries() {
    let mut server = mockito::Server::new();
    let api = "/jmap/api";
    server
        .mock("POST", api)
        .with_status(400)
        .with_body(
            json!({ "type": "urn:ietf:params:jmap:error:limit", "limit": "maxSizeRequest" })
                .to_string(),
        )
        .expect(1)
        .create();
    server
        .mock("POST", api)
        .with_status(200)
        .with_body(
            json!({ "methodResponses": [["Mailbox/get",
                { "list": [{ "id": "a", "name": "A" }], "notFound": [] }, "g"]] })
            .to_string(),
        )
        .expect(1)
        .create();
    server
        .mock("POST", api)
        .with_status(200)
        .with_body(
            json!({ "methodResponses": [["Mailbox/get",
                { "list": [{ "id": "b", "name": "B" }], "notFound": [] }, "g"]] })
            .to_string(),
        )
        .expect(1)
        .create();

    let url = format!("{}{}", server.url(), api);
    let ids = vec![JmapId("a".into()), JmapId("b".into())];
    let result: request::GetResult<Mailbox> = get_objects(
        &client(0),
        &url,
        "w",
        "Mailbox",
        &ids,
        Some(&["name"]),
        &limits(500),
    )
    .expect("resplit succeeds");
    let mut names: Vec<String> = result.list.into_iter().map(|m| m.name).collect();
    names.sort();
    assert_eq!(names, vec!["A".to_owned(), "B".to_owned()]);
}

#[test]
fn malformed_response_is_classified_not_retried_forever() {
    let mut server = mockito::Server::new();
    let api = "/jmap/api";
    server
        .mock("POST", api)
        .with_status(200)
        .with_body("{\"unexpected\":true}")
        .create();
    let url = format!("{}{}", server.url(), api);
    let err = request::query_all_ids(&client(2), &url, "w", "Mailbox", &limits(2)).unwrap_err();
    assert!(matches!(err, JmapError::Malformed(_)), "got {err:?}");
}

#[test]
fn principal_substring_prefilter_rejects_near_matches() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let session: Session = serde_json::from_str(&session_json(&base)).unwrap();
    server
        .mock("POST", "/jmap/api")
        .with_status(200)
        .with_body(
            json!({ "methodResponses": [
                ["Principal/query", { "ids": ["p1", "p2"] }, "q"],
                ["Principal/get", { "list": [
                    { "id": "p1", "name": "alice2", "accounts": {} },
                    { "id": "p2", "name": "alice", "accounts": {
                        "w": { "urn:ietf:params:jmap:principals:owner":
                               { "accountIdForPrincipal": "w" } } } }
                ] }, "g"]
            ] })
            .to_string(),
        )
        .create();

    let id = account::resolve(&AccountSelector::Name("alice".into()), &session, &client(1))
        .expect("resolved");
    assert_eq!(id, "w");
}

#[test]
fn principal_ambiguous_exact_match_is_rejected() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let session: Session = serde_json::from_str(&session_json(&base)).unwrap();
    server
        .mock("POST", "/jmap/api")
        .with_status(200)
        .with_body(
            json!({ "methodResponses": [
                ["Principal/query", { "ids": ["p1", "p2"] }, "q"],
                ["Principal/get", { "list": [
                    { "id": "p1", "name": "dup", "accounts": {} },
                    { "id": "p2", "name": "dup", "accounts": {} }
                ] }, "g"]
            ] })
            .to_string(),
        )
        .create();
    let err =
        account::resolve(&AccountSelector::Name("dup".into()), &session, &client(1)).unwrap_err();
    assert_eq!(err.exit_code(), 3);
}

#[test]
fn principal_unknown_capability_400_is_actionable() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let session: Session = serde_json::from_str(&session_json(&base)).unwrap();
    server
        .mock("POST", "/jmap/api")
        .with_status(400)
        .with_body(
            json!({
                "type": "urn:ietf:params:jmap:error:unknownCapability",
                "status": 400,
                "detail": "The Request object used capability \
                           'urn:ietf:params:jmap:principals', which is not supported \
                           by this server."
            })
            .to_string(),
        )
        .create();

    let err =
        account::resolve(&AccountSelector::Name("ghost".into()), &session, &client(0)).unwrap_err();
    assert_eq!(err.exit_code(), 3);
    let msg = err.to_string();
    assert!(msg.contains("urn:ietf:params:jmap:principals"));
    assert!(msg.contains("--account-id"));
    assert!(msg.contains("alice@example.org (w)"));
}

#[test]
fn get_reports_not_found_ids() {
    let mut server = mockito::Server::new();
    let api = "/jmap/api";
    server
        .mock("POST", api)
        .with_status(200)
        .with_body(
            json!({ "methodResponses": [["Mailbox/get",
                { "list": [{ "id": "a", "name": "A" }], "notFound": ["b"] }, "g"]] })
            .to_string(),
        )
        .create();
    let url = format!("{}{}", server.url(), api);
    let ids = vec![JmapId("a".into()), JmapId("b".into())];
    let got: request::GetResult<Mailbox> = get_objects(
        &client(1),
        &url,
        "w",
        "Mailbox",
        &ids,
        Some(&["name"]),
        &limits(500),
    )
    .expect("get ok");
    assert_eq!(got.list.len(), 1);
    assert_eq!(got.not_found, vec![JmapId("b".into())]);
}

#[test]
fn queryless_get_all_enumerates_full_list() {
    let mut server = mockito::Server::new();
    let api = "/jmap/api";
    server
        .mock("POST", api)
        .with_status(200)
        .with_body(
            json!({ "methodResponses": [["Identity/get", { "list": [
                { "id": "i1", "name": "A", "email": "a@x.test" },
                { "id": "i2", "name": "B", "email": "b@x.test" }
            ], "notFound": [] }, "g"]] })
            .to_string(),
        )
        .create();
    let url = format!("{}{}", server.url(), api);
    let got: request::GetResult<Identity> =
        get_all(&client(1), &url, "w", "Identity").expect("get_all ok");
    assert_eq!(got.list.len(), 2);
    assert_eq!(got.list[1].email, "b@x.test");
}

#[test]
fn persistent_anchor_not_found_surfaces_after_restarts() {
    let mut server = mockito::Server::new();
    let api = "/jmap/api";
    server
        .mock("POST", api)
        .with_status(200)
        .with_body(
            json!({ "methodResponses": [["error", { "type": "anchorNotFound" }, "q"]] })
                .to_string(),
        )
        .create();
    let url = format!("{}{}", server.url(), api);
    let err = request::query_all_ids(&client(2), &url, "w", "Mailbox", &limits(2)).unwrap_err();
    assert!(matches!(err, JmapError::AnchorNotFound), "got {err:?}");
}

#[test]
fn single_object_exceeding_size_is_unsplittable() {
    let mut server = mockito::Server::new();
    let api = "/jmap/api";
    server
        .mock("POST", api)
        .with_status(400)
        .with_body(
            json!({ "type": "urn:ietf:params:jmap:error:limit", "limit": "maxSizeRequest" })
                .to_string(),
        )
        .create();
    let url = format!("{}{}", server.url(), api);
    let ids = vec![JmapId("huge".into())];
    let err: JmapError = get_objects::<Mailbox>(
        &client(2),
        &url,
        "w",
        "Mailbox",
        &ids,
        Some(&["name"]),
        &limits(500),
    )
    .unwrap_err();
    assert!(
        matches!(err, JmapError::SingleObjectTooLarge(_)),
        "got {err:?}"
    );
}

#[test]
fn set_call_decodes_created_and_per_object_set_errors() {
    let mut server = mockito::Server::new();
    let api = "/jmap/api";
    server
        .mock("POST", api)
        .with_status(200)
        .with_body(
            json!({ "methodResponses": [["Mailbox/set", {
                "created": { "c1": { "id": "S1" } },
                "notCreated": { "c2": { "type": "invalidProperties",
                                        "properties": ["name"] } }
            }, "s"]] })
            .to_string(),
        )
        .create();
    let url = format!("{}{}", server.url(), api);
    let outcome = set_call(
        &client(1),
        &url,
        "w",
        "Mailbox",
        SetRequest {
            create: Some(json!({ "c1": { "name": "Ok" }, "c2": { "name": "" } })),
            ..Default::default()
        },
        &limits(500),
    )
    .expect("set envelope ok despite per-object failure");
    assert_eq!(outcome.created.len(), 1);
    assert_eq!(outcome.created[0].0, "c1");
    assert_eq!(outcome.not_created.len(), 1);
    assert_eq!(outcome.not_created[0].0, "c2");
}

#[test]
fn set_call_resplits_on_request_too_large() {
    let mut server = mockito::Server::new();
    let api = "/jmap/api";
    server
        .mock("POST", api)
        .with_status(400)
        .with_body(
            json!({ "type": "urn:ietf:params:jmap:error:limit", "limit": "maxSizeRequest" })
                .to_string(),
        )
        .expect(1)
        .create();
    server
        .mock("POST", api)
        .with_status(200)
        .with_body(
            json!({ "methodResponses": [["Mailbox/set",
                { "created": { "c1": { "id": "S1" } } }, "s"]] })
            .to_string(),
        )
        .expect(1)
        .create();
    server
        .mock("POST", api)
        .with_status(200)
        .with_body(
            json!({ "methodResponses": [["Mailbox/set",
                { "created": { "c2": { "id": "S2" } } }, "s"]] })
            .to_string(),
        )
        .expect(1)
        .create();
    let url = format!("{}{}", server.url(), api);
    let outcome = set_call(
        &client(0),
        &url,
        "w",
        "Mailbox",
        SetRequest {
            create: Some(json!({ "c1": { "name": "A" }, "c2": { "name": "B" } })),
            ..Default::default()
        },
        &limits(500),
    )
    .expect("resplit succeeds with max_retries=0");
    let mut created: Vec<String> = outcome.created.into_iter().map(|(k, _)| k).collect();
    created.sort();
    assert_eq!(created, vec!["c1".to_owned(), "c2".to_owned()]);
}

#[test]
fn shared_cooldown_pauses_other_workers() {
    let mut server = mockito::Server::new();
    let limiter = server
        .mock("POST", "/limited")
        .with_status(429)
        .with_header("Retry-After", "1")
        .with_body("{}")
        .expect(1)
        .create();
    server
        .mock("POST", "/limited")
        .with_status(200)
        .with_body("{}")
        .create();
    let other = server
        .mock("GET", "/other")
        .with_status(200)
        .with_body("ok")
        .create();

    let shared = client(3);
    let limited_url = format!("{}/limited", server.url());
    let other_url = format!("{}/other", server.url());

    let a = shared.clone();
    let worker_a = std::thread::spawn(move || {
        let _ = a.post_json(&limited_url, &json!({}));
    });
    std::thread::sleep(std::time::Duration::from_millis(150));
    let started = Instant::now();
    shared.get(&other_url).expect("other request ok");
    let waited = started.elapsed();
    worker_a.join().unwrap();

    assert!(
        waited.as_millis() >= 600,
        "second worker should have been paused by the shared 429 cooldown, waited {waited:?}"
    );
    limiter.assert();
    other.assert();
}

fn upload_session(base: &str) -> Session {
    serde_json::from_str(
        &json!({
            "apiUrl": format!("{base}/jmap/api"),
            "uploadUrl": format!("{base}/upload/{{accountId}}/"),
            "downloadUrl": format!("{base}/dl/{{accountId}}/{{blobId}}/{{type}}/{{name}}"),
            "capabilities": { "urn:ietf:params:jmap:core": {
                "maxObjectsInGet": 500, "maxObjectsInSet": 500, "maxCallsInRequest": 16,
                "maxConcurrentRequests": 4, "maxConcurrentUpload": 4,
                "maxSizeRequest": 10000000, "maxSizeUpload": 50000000 } },
            "accounts": { "w": { "name": "n" } }
        })
        .to_string(),
    )
    .unwrap()
}

#[test]
fn retries_counter_increments_on_429() {
    let mut server = mockito::Server::new();
    let api = "/jmap/api";
    server
        .mock("POST", api)
        .with_status(429)
        .with_header("Retry-After", "1")
        .with_body("{}")
        .expect(1)
        .create();
    server
        .mock("POST", api)
        .with_status(200)
        .with_body(page(&[]))
        .expect(1)
        .create();

    let c = client(3);
    let url = format!("{}{}", server.url(), api);
    let _ = request::query_all_ids(&c, &url, "w", "Mailbox", &limits(2)).expect("query ok");
    assert_eq!(c.retries_observed(), 1, "retry counter must bump on 429");
    assert_eq!(
        c.retry_after_sleeps(),
        1,
        "retry-after sleep counter must bump"
    );
}

#[test]
fn upload_size_precheck_blocks_before_contacting_server() {
    use vandelay::jmap::blobxfer;

    let mut server = mockito::Server::new();
    let no_hits = server
        .mock("POST", mockito::Matcher::Any)
        .expect(0)
        .create();
    let session = upload_session(&server.url());

    let c = client(0);
    c.set_limits(&limits_concurrency(4, 4, 10));
    let err = blobxfer::upload_bytes(&c, &session, "w", "application/octet-stream", &[0u8; 11])
        .unwrap_err();
    assert!(
        matches!(err, JmapError::SingleObjectTooLarge(_)),
        "got {err:?}"
    );
    no_hits.assert();
}

#[test]
fn concurrent_requests_400_problem_is_retried_then_succeeds() {
    let mut server = mockito::Server::new();
    let api = "/jmap/api";
    server
        .mock("POST", api)
        .with_status(400)
        .with_header(
            "RateLimit-Policy",
            "\"concurrent-requests\";q=4;qu=\"concurrent-requests\"",
        )
        .with_header("RateLimit", "\"concurrent-requests\";r=0")
        .with_body(
            json!({
                "type": "urn:ietf:params:jmap:error:limit",
                "status": 400,
                "limit": "maxConcurrentRequests"
            })
            .to_string(),
        )
        .expect(1)
        .create();
    server
        .mock("POST", api)
        .with_status(200)
        .with_body(page(&[]))
        .expect(1)
        .create();

    let c = client(3);
    let url = format!("{}{}", server.url(), api);
    let ids =
        request::query_all_ids(&c, &url, "w", "Mailbox", &limits(2)).expect("eventual success");
    assert!(ids.is_empty());
    assert!(
        c.retries_observed() >= 1,
        "concurrent-requests 400 must count as a retry"
    );
    assert_eq!(
        c.retry_after_sleeps(),
        0,
        "concurrent-* errors carry no Retry-After"
    );
}

#[test]
fn concurrent_uploads_400_problem_is_retried_then_succeeds() {
    use vandelay::jmap::blobxfer;

    let mut server = mockito::Server::new();
    server
        .mock("POST", "/upload/w/")
        .with_status(400)
        .with_body(
            json!({
                "type": "urn:ietf:params:jmap:error:limit",
                "status": 400,
                "limit": "maxConcurrentUpload"
            })
            .to_string(),
        )
        .expect(1)
        .create();
    server
        .mock("POST", "/upload/w/")
        .with_status(200)
        .with_body(json!({ "accountId": "w", "blobId": "G7", "type": "x", "size": 3 }).to_string())
        .expect(1)
        .create();

    let session = upload_session(&server.url());
    let c = client(3);
    c.set_limits(&limits_concurrency(4, 4, 1_000_000));
    let id = blobxfer::upload_bytes(&c, &session, "w", "text/plain", b"abc").expect("upload ok");
    assert_eq!(id, JmapId("G7".into()));
    assert!(c.retries_observed() >= 1);
}

#[test]
fn blob_upload_quota_429_with_retry_after_is_honoured() {
    use vandelay::jmap::blobxfer;

    let mut server = mockito::Server::new();
    server
        .mock("POST", "/upload/w/")
        .with_status(429)
        .with_header("Retry-After", "1")
        .with_header(
            "RateLimit-Policy",
            "\"blob-upload-files\";q=1, \"blob-upload-bytes\";q=1000;qu=\"content-bytes\"",
        )
        .with_header(
            "RateLimit",
            "\"blob-upload-files\";r=0;t=1, \"blob-upload-bytes\";r=0;t=1",
        )
        .with_body(
            json!({ "type": "about:blank", "status": 429, "title": "Quota exceeded" }).to_string(),
        )
        .expect(1)
        .create();
    server
        .mock("POST", "/upload/w/")
        .with_status(200)
        .with_body(json!({ "accountId": "w", "blobId": "Q1", "type": "x", "size": 3 }).to_string())
        .expect(1)
        .create();

    let session = upload_session(&server.url());
    let c = client(3);
    c.set_limits(&limits_concurrency(4, 4, 1_000_000));
    let started = Instant::now();
    let id = blobxfer::upload_bytes(&c, &session, "w", "text/plain", b"abc").expect("upload ok");
    assert_eq!(id, JmapId("Q1".into()));
    assert!(
        started.elapsed().as_millis() >= 900,
        "Retry-After of 1s should have delayed the upload retry"
    );
    assert_eq!(c.retry_after_sleeps(), 1);
}

#[test]
#[ignore = "exercises the long-retry warning path; sleeps ~11s"]
fn long_retry_after_warns_at_default_verbosity() {
    let mut server = mockito::Server::new();
    let api = "/jmap/api";
    server
        .mock("POST", api)
        .with_status(429)
        .with_header("Retry-After", "11")
        .with_body(
            json!({
                "type": "about:blank",
                "status": 429,
                "title": "Quota exceeded",
                "detail": "You have exceeded the blob upload quota of 1000 files or 50000000 bytes."
            })
            .to_string(),
        )
        .expect(1)
        .create();
    server
        .mock("POST", api)
        .with_status(200)
        .with_body(page(&[]))
        .expect(1)
        .create();

    let c = client(3);
    let url = format!("{}{}", server.url(), api);
    let started = Instant::now();
    let _ = request::query_all_ids(&c, &url, "w", "Mailbox", &limits(2)).expect("query ok");
    assert!(
        started.elapsed().as_secs() >= 10,
        "should have honored Retry-After: 11"
    );
    assert_eq!(c.retries_observed(), 1);
    assert_eq!(c.retry_after_sleeps(), 1);
}

#[test]
fn ratelimit_headers_without_retry_after_fall_back_to_backoff() {
    let mut server = mockito::Server::new();
    let api = "/jmap/api";
    server
        .mock("POST", api)
        .with_status(429)
        .with_header("RateLimit-Policy", "\"requests\";q=100")
        .with_header("RateLimit", "\"requests\";r=0")
        .with_body("{}")
        .expect(1)
        .create();
    server
        .mock("POST", api)
        .with_status(200)
        .with_body(page(&[]))
        .expect(1)
        .create();

    let c = client(3);
    let url = format!("{}{}", server.url(), api);
    let _ = request::query_all_ids(&c, &url, "w", "Mailbox", &limits(2)).expect("query ok");
    assert_eq!(c.retries_observed(), 1);
    assert_eq!(
        c.retry_after_sleeps(),
        0,
        "no Retry-After means we use the backoff schedule, not a header delay"
    );
}

#[test]
fn upload_extracts_blob_id_and_download_streams_via_download_url() {
    use vandelay::jmap::blobxfer;

    let mut server = mockito::Server::new();
    let base = server.url();
    let session: Session = serde_json::from_str(
        &json!({
            "apiUrl": format!("{base}/jmap/api"),
            "uploadUrl": format!("{base}/upload/{{accountId}}/"),
            "downloadUrl": format!("{base}/dl/{{accountId}}/{{blobId}}/{{type}}/{{name}}"),
            "capabilities": { "urn:ietf:params:jmap:core": {
                "maxObjectsInGet": 500, "maxObjectsInSet": 500, "maxCallsInRequest": 16,
                "maxConcurrentRequests": 4, "maxConcurrentUpload": 4,
                "maxSizeRequest": 10000000, "maxSizeUpload": 50000000 } },
            "accounts": { "w": { "name": "n" } }
        })
        .to_string(),
    )
    .unwrap();

    server
        .mock("POST", "/upload/w/")
        .with_status(200)
        .with_body(json!({ "accountId": "w", "blobId": "G99", "type": "x", "size": 3 }).to_string())
        .create();
    let blob_id =
        blobxfer::upload_bytes(&client(1), &session, "w", "text/plain", b"abc").expect("upload ok");
    assert_eq!(blob_id, JmapId("G99".into()));

    server
        .mock("GET", "/dl/w/G99/x/n")
        .with_status(200)
        .with_body(b"hello")
        .create();
    let bytes =
        blobxfer::download_bytes(&client(1), &session, "w", "G99", "x", "n").expect("download ok");
    assert_eq!(bytes, b"hello");
}

#[test]
fn persistent_problem_json_429_without_retry_after_eventually_succeeds_via_shared_throttle() {
    let mut server = mockito::Server::new();
    let api = "/jmap/api";
    let limiter = server
        .mock("POST", api)
        .with_status(429)
        .with_header("content-type", "application/problem+json")
        .with_body(
            json!({
                "type": "about:blank",
                "status": 429,
                "title": "Too Many Requests",
                "detail": "Your request has been rate limited. Please try again in a few seconds."
            })
            .to_string(),
        )
        .expect(2)
        .create();
    let ok = server
        .mock("POST", api)
        .with_status(200)
        .with_body(page(&[]))
        .expect(1)
        .create();

    let c = client(5);
    let url = format!("{}{}", server.url(), api);
    let started = Instant::now();
    let _ = request::query_all_ids(&c, &url, "w", "Mailbox", &limits(2)).expect("eventual success");
    let waited = started.elapsed();

    limiter.assert();
    ok.assert();
    assert_eq!(c.retries_observed(), 2, "two 429s consumed two retry slots");
    assert_eq!(
        c.retry_after_sleeps(),
        0,
        "no Retry-After header means no Retry-After sleep counts"
    );
    assert_eq!(
        c.throttle_level(),
        0,
        "successful response resets shared level"
    );
    assert!(
        waited.as_millis() >= 1_400,
        "shared-throttle base + escalation should hold us at least ~1.5s (got {waited:?})"
    );
}

#[test]
fn persistent_429_on_download_uses_shared_throttle() {
    use vandelay::jmap::blobxfer;

    let mut server = mockito::Server::new();
    let session = upload_session(&server.url());

    let limiter = server
        .mock("GET", "/dl/w/B1/text%2Fplain/blob")
        .with_status(429)
        .with_header("content-type", "application/problem+json")
        .with_body(
            json!({
                "type": "about:blank",
                "status": 429,
                "title": "Too Many Requests",
                "detail": "Your request has been rate limited. Please try again in a few seconds."
            })
            .to_string(),
        )
        .expect(1)
        .create();
    let ok = server
        .mock("GET", "/dl/w/B1/text%2Fplain/blob")
        .with_status(200)
        .with_body(b"payload")
        .expect(1)
        .create();

    let c = client(3);
    let started = Instant::now();
    let bytes = blobxfer::download_bytes(&c, &session, "w", "B1", "text/plain", "blob")
        .expect("eventual success");
    let waited = started.elapsed();
    assert_eq!(bytes, b"payload");
    limiter.assert();
    ok.assert();
    assert!(
        c.retries_observed() >= 1,
        "the 429 must be counted as a retry"
    );
    assert_eq!(
        c.throttle_level(),
        0,
        "success after a 429 resets the shared level"
    );
    assert!(
        waited.as_millis() >= 400,
        "level-1 throttle backoff should be at least ~0.5s (got {waited:?})"
    );
}

#[test]
fn shared_throttle_level_grows_across_concurrent_workers() {
    let mut server = mockito::Server::new();
    let api = "/limited";
    server
        .mock("POST", api)
        .with_status(429)
        .with_header("content-type", "application/problem+json")
        .with_body(
            json!({
                "type": "about:blank",
                "status": 429,
                "title": "Too Many Requests",
                "detail": "rate limited"
            })
            .to_string(),
        )
        .expect(2)
        .create();
    let ok = server
        .mock("POST", api)
        .with_status(200)
        .with_body("{}")
        .expect(2)
        .create();

    let shared = client(1);
    let url = format!("{}{}", server.url(), api);

    let a = shared.clone();
    let url_a = url.clone();
    let worker_a = std::thread::spawn(move || a.post_json(&url_a, &json!({})));
    let b = shared.clone();
    let url_b = url.clone();
    let worker_b = std::thread::spawn(move || b.post_json(&url_b, &json!({})));

    worker_a.join().unwrap().expect("worker A eventually 200");
    worker_b.join().unwrap().expect("worker B eventually 200");

    ok.assert();
    assert_eq!(
        shared.throttle_level(),
        0,
        "shared level must reset to 0 once the storm clears (a 2xx landed)"
    );
    assert!(
        shared.retries_observed() >= 2,
        "both workers saw a 429 (or more) before recovery"
    );
}

#[test]
fn get_changes_paginates_until_no_more() {
    let mut server = mockito::Server::new();
    let api = "/jmap/api";
    let _p1 = server
        .mock("POST", api)
        .match_body(mockito::Matcher::Regex("\"sinceState\":\"s1\"".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/changes",{"accountId":"w","oldState":"s1",
                "newState":"s2","hasMoreChanges":true,"created":["A"],"updated":["U1"],
                "destroyed":["D1"]},"c"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _p2 = server
        .mock("POST", api)
        .match_body(mockito::Matcher::Regex("\"sinceState\":\"s2\"".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/changes",{"accountId":"w","oldState":"s2",
                "newState":"s3","hasMoreChanges":false,"created":[],"updated":["U2"],
                "destroyed":[]},"c"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let url = format!("{}{}", server.url(), api);
    let r = get_changes(&client(0), &url, "w", "Mailbox", "s1", &limits(500)).expect("changes");
    let updated: Vec<String> = r.updated.iter().map(|i| i.0.clone()).collect();
    assert_eq!(updated, vec!["U1".to_owned(), "U2".to_owned()]);
    let created: Vec<String> = r.created.iter().map(|i| i.0.clone()).collect();
    assert_eq!(created, vec!["A".to_owned()]);
    let destroyed: Vec<String> = r.destroyed.iter().map(|i| i.0.clone()).collect();
    assert_eq!(destroyed, vec!["D1".to_owned()]);
    assert_eq!(r.new_state, "s3", "cursor advances to the final newState");
}

#[test]
fn get_changes_cannot_calculate_changes_is_typed_error() {
    let mut server = mockito::Server::new();
    let api = "/jmap/api";
    let _m = server
        .mock("POST", api)
        .with_body(
            json!({"methodResponses":[["error",{"type":"cannotCalculateChanges"},"c"]]})
                .to_string(),
        )
        .create();
    let url = format!("{}{}", server.url(), api);
    let err =
        get_changes(&client(0), &url, "w", "Email", "stale", &limits(500)).expect_err("must error");
    assert!(
        matches!(err, JmapError::CannotCalculateChanges),
        "got {err:?}"
    );
}

#[test]
fn get_changes_dedups_repeated_ids_across_pages() {
    let mut server = mockito::Server::new();
    let api = "/jmap/api";
    let _p1 = server
        .mock("POST", api)
        .match_body(mockito::Matcher::Regex("\"sinceState\":\"s1\"".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/changes",{"accountId":"w","oldState":"s1",
                "newState":"s2","hasMoreChanges":true,"created":[],"updated":["U1","U2"],
                "destroyed":[]},"c"]]})
            .to_string(),
        )
        .expect(1)
        .create();
    let _p2 = server
        .mock("POST", api)
        .match_body(mockito::Matcher::Regex("\"sinceState\":\"s2\"".into()))
        .with_body(
            json!({"methodResponses":[["Mailbox/changes",{"accountId":"w","oldState":"s2",
                "newState":"s3","hasMoreChanges":false,"created":[],"updated":["U1","U3"],
                "destroyed":[]},"c"]]})
            .to_string(),
        )
        .expect(1)
        .create();

    let url = format!("{}{}", server.url(), api);
    let r = get_changes(&client(0), &url, "w", "Mailbox", "s1", &limits(500)).expect("changes");
    let updated: Vec<String> = r.updated.iter().map(|i| i.0.clone()).collect();
    assert_eq!(
        updated,
        vec!["U1".to_owned(), "U2".to_owned(), "U3".to_owned()],
        "an id repeated across pages is fetched once, first-seen order preserved"
    );
}

#[test]
fn get_changes_unknown_method_is_typed_error() {
    let mut server = mockito::Server::new();
    let api = "/jmap/api";
    let _m = server
        .mock("POST", api)
        .with_body(json!({"methodResponses":[["error",{"type":"unknownMethod"},"c"]]}).to_string())
        .create();
    let url = format!("{}{}", server.url(), api);
    let err =
        get_changes(&client(0), &url, "w", "Email", "s1", &limits(500)).expect_err("must error");
    assert!(matches!(err, JmapError::UnknownMethod), "got {err:?}");
}

#[test]
fn get_state_reads_state_from_empty_get() {
    let mut server = mockito::Server::new();
    let api = "/jmap/api";
    let _m = server
        .mock("POST", api)
        .match_body(mockito::Matcher::Regex("Email/get".into()))
        .with_body(
            json!({"methodResponses":[["Email/get",{"accountId":"w","state":"snap-1",
                "list":[],"notFound":[]},"g"]]})
            .to_string(),
        )
        .create();
    let url = format!("{}{}", server.url(), api);
    let st = get_state(&client(0), &url, "w", "Email").expect("get_state");
    assert_eq!(st.as_deref(), Some("snap-1"));
}

fn archive_path() -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "vandelay-mockjmap-{}-{n}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

#[test]
fn session_urls_on_a_foreign_origin_are_reported_as_a_mismatch() {
    let mut server = mockito::Server::new();
    let base = server.url();
    let elsewhere = "https://mail.example".to_owned();
    let _root = server.mock("GET", "/").with_status(404).create();
    let _wk = server
        .mock("GET", "/.well-known/jmap")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(session_json(&elsewhere))
        .create();

    let session = Session::discover(&client(0), &base).expect("session discovered");
    let mismatches = session.origin_mismatches(&base);
    assert_eq!(mismatches.len(), 1, "one warning per distinct origin");
    assert_eq!(
        mismatches[0].fields,
        vec!["apiUrl", "uploadUrl", "downloadUrl"]
    );
    let warning = mismatches[0].to_string();
    assert!(warning.contains("session advertises apiUrl"), "{warning}");
    assert!(warning.contains("connected to "), "{warning}");
    assert!(warning.contains("advertised HTTP URL setting"), "{warning}");
}

#[test]
fn import_aborts_with_exit_two_when_the_advertised_api_url_is_unreachable() {
    let mut api_host = mockito::Server::new();
    let mut session_host = mockito::Server::new();
    let session_base = session_host.url();
    let api_base = api_host.url();

    let _root = session_host.mock("GET", "/").with_status(404).create();
    let _wk = session_host
        .mock("GET", "/.well-known/jmap")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(session_json(&api_base))
        .create();
    let _nginx = api_host
        .mock("POST", "/jmap/api")
        .with_status(404)
        .with_header("content-type", "text/html")
        .with_body(
            "<html><head><title>404 Not Found</title></head><body>404 Not Found</body></html>",
        )
        .create();

    let archive = archive_path();
    let err = vandelay::sync::import_jmap::run(
        vandelay::sync::CommonConfig {
            archive: archive.clone(),
            threads: 1,
            dry_run: false,
            max_retries: 0,
            allow_invalid_certs: false,
            logger: vandelay::logging::Logger::from_flags(true, 0),
        },
        vandelay::sync::ImportConfig {
            connect: vandelay::sync::ConnectConfig {
                url: session_base.clone(),
                auth: Auth::Basic {
                    user: "u".into(),
                    password: "p".into(),
                },
                account: AccountSelector::Id("w".into()),
            },
            objects: None,
            allow_source_change: false,
        },
    )
    .expect_err("an unreachable apiUrl must abort the run");

    assert!(
        matches!(err, vandelay::error::Error::Connection(_)),
        "a 404 from the advertised apiUrl is a whole-run connection failure, got {err:?}"
    );
    assert_eq!(err.exit_code(), 2, "must not report a partial failure");
    let _ = std::fs::remove_file(&archive);
}
