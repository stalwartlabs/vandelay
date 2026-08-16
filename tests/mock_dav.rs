/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::io::Cursor;

use vandelay::dav::client::DavClient;
use vandelay::dav::discover::{DavKind, discover};
use vandelay::dav::parse::{parse_multistatus, strip_ascii_control_chars};
use vandelay::dav::xml;
use vandelay::jmap::error::JmapError;
use vandelay::jmap::http::{Auth, RetryPolicy};

fn client(retries: u32) -> DavClient {
    DavClient::new(
        Auth::Basic {
            user: "u".into(),
            password: "p".into(),
        },
        RetryPolicy::new(retries),
        false,
    )
}

const MULTISTATUS_HEADERS: &[(&str, &str)] = &[("content-type", "application/xml; charset=utf-8")];

fn multistatus_response(
    server: &mut mockito::Server,
    method: &str,
    path: &str,
    body: &str,
) -> mockito::Mock {
    let mut m = server.mock(method, path).with_status(207);
    for (k, v) in MULTISTATUS_HEADERS {
        m = m.with_header(*k, v);
    }
    m.with_body(body).create()
}

#[test]
fn discovery_uses_url_as_homeset_when_collection_present() {
    let mut server = mockito::Server::new();
    let url = server.url();
    let body = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>{url}/dav/cal/u/default/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
        <d:displayname>Default</d:displayname>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let _m = multistatus_response(&mut server, "PROPFIND", "/dav/cal/", &body);
    let c = client(0);
    let url_with_path = format!("{url}/dav/cal/");
    let disc = discover(&c, DavKind::Caldav, &url_with_path).expect("discover");
    assert_eq!(disc.collections.len(), 1);
    assert!(disc.collections[0].props.is_calendar);
}

#[test]
fn discovery_resolves_per_user_principal_even_when_url_lists_collections() {
    let mut server = mockito::Server::new();
    let url = server.url();

    let collections_body = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>{url}/dav/cal/secondary@vandelay.org/default/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
        <d:displayname>Default</d:displayname>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let _listing = server
        .mock("PROPFIND", "/dav/cal/")
        .match_header("depth", "1")
        .with_status(207)
        .with_header("content-type", "application/xml; charset=utf-8")
        .with_body(&collections_body)
        .create();

    let principal_body = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>{url}/dav/cal/</d:href>
    <d:propstat>
      <d:prop>
        <d:current-user-principal><d:href>{url}/dav/principals/secondary@vandelay.org/</d:href></d:current-user-principal>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let _principal = server
        .mock("PROPFIND", "/dav/cal/")
        .match_header("depth", "0")
        .with_status(207)
        .with_header("content-type", "application/xml; charset=utf-8")
        .with_body(&principal_body)
        .create();

    let c = client(0);
    let disc = discover(&c, DavKind::Caldav, &format!("{url}/dav/cal/")).expect("discover");
    assert_eq!(disc.collections.len(), 1);
    assert_eq!(
        disc.principal_url.as_deref(),
        Some(format!("{url}/dav/principals/secondary@vandelay.org/").as_str()),
        "account identity must be the per-user principal, not the shared base DAV root"
    );
}

#[test]
fn discovery_falls_through_principal_to_home_set() {
    let mut server = mockito::Server::new();
    let url = server.url();

    let empty_collection_body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:"/>"#;
    let _m1 = multistatus_response(&mut server, "PROPFIND", "/", empty_collection_body);

    let principal_body = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>{url}/</d:href>
    <d:propstat>
      <d:prop>
        <d:current-user-principal><d:href>{url}/principals/alice/</d:href></d:current-user-principal>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let _m2 = multistatus_response(&mut server, "PROPFIND", "/", &principal_body);

    let homeset_body = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>{url}/principals/alice/</d:href>
    <d:propstat>
      <d:prop>
        <c:calendar-home-set><d:href>{url}/dav/cal/alice/</d:href></c:calendar-home-set>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let _m3 = multistatus_response(&mut server, "PROPFIND", "/principals/alice/", &homeset_body);

    let collections_body = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>{url}/dav/cal/alice/default/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
        <d:displayname>Default</d:displayname>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let _m4 = multistatus_response(
        &mut server,
        "PROPFIND",
        "/dav/cal/alice/",
        &collections_body,
    );

    let c = client(0);
    let disc = discover(&c, DavKind::Caldav, &url).expect("discover");
    assert_eq!(disc.collections.len(), 1);
    assert!(disc.principal_url.is_some());
    assert!(disc.home_set_url.ends_with("/dav/cal/alice/"));
}

#[test]
fn discovery_returns_not_found_when_no_collections() {
    let mut server = mockito::Server::new();
    let empty = r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:"/>"#;
    let _m = server
        .mock("PROPFIND", mockito::Matcher::Any)
        .with_status(207)
        .with_header("content-type", "application/xml")
        .with_body(empty)
        .expect_at_least(1)
        .create();

    let c = client(0);
    let err = discover(&c, DavKind::Caldav, &server.url()).unwrap_err();
    assert!(matches!(
        err,
        vandelay::dav::discover::DiscoveryError::NotFound { .. }
    ));
}

#[test]
fn parses_multistatus_with_default_namespace_no_prefix() {
    let body = r#"<?xml version="1.0"?>
<multistatus xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <response>
    <href>/cal/a/</href>
    <propstat>
      <prop>
        <resourcetype><collection/><c:calendar/></resourcetype>
        <displayname>A</displayname>
      </prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
  </response>
</multistatus>"#;
    let r = parse_multistatus(Cursor::new(body), "https://x/").expect("parse");
    assert_eq!(r.len(), 1);
    assert!(r[0].props.is_calendar);
    assert_eq!(r[0].props.displayname.as_deref(), Some("A"));
}

#[test]
fn ascii_control_chars_are_stripped_before_parse() {
    let dirty: Vec<u8> = b"<?xml version=\"1.0\"?>\x02<d:multistatus xmlns:d=\"DAV:\"/>".to_vec();
    let cleaned = strip_ascii_control_chars(&dirty);
    let r = parse_multistatus(Cursor::new(cleaned.as_ref()), "https://x/").expect("parse");
    assert!(r.is_empty());
}

#[test]
fn rate_limited_propfind_retries_via_shared_schedule() {
    let mut server = mockito::Server::new();
    let _m1 = server
        .mock("PROPFIND", "/dav/")
        .with_status(429)
        .with_header("retry-after", "1")
        .with_body("rate-limited")
        .create();
    let body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/dav/cal/u/d/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
    let _m2 = multistatus_response(&mut server, "PROPFIND", "/dav/", body);

    let c = client(3);
    let start = std::time::Instant::now();
    let result = c
        .propfind(&format!("{}/dav/", server.url()), 1, "<x/>")
        .expect("propfind");
    let elapsed = start.elapsed();
    assert_eq!(result.status, 207);
    assert!(
        elapsed.as_millis() >= 900,
        "expected Retry-After 1s honoured, elapsed={elapsed:?}"
    );
    assert!(c.retries_observed() >= 1);
}

#[test]
fn auth_failure_401_is_fatal() {
    let mut server = mockito::Server::new();
    let _m = server
        .mock("PROPFIND", "/")
        .with_status(401)
        .with_body("Unauthorized")
        .create();

    let c = client(0);
    let err = c
        .propfind(&format!("{}/", server.url()), 0, "<x/>")
        .unwrap_err();
    assert!(matches!(err, JmapError::Auth(_)), "got {err:?}");
}

#[test]
fn http_404_classified_as_vanished_returns_response() {
    let mut server = mockito::Server::new();
    let _m = server
        .mock("GET", "/dav/cal/u/d/missing.ics")
        .with_status(404)
        .create();

    let c = client(0);
    let result = c
        .get(&format!("{}/dav/cal/u/d/missing.ics", server.url()))
        .expect("get");
    assert_eq!(result.status, 404);
}

#[test]
fn http_503_retries_then_succeeds() {
    let mut server = mockito::Server::new();
    let _m1 = server.mock("PROPFIND", "/").with_status(503).create();
    let _m2 = server
        .mock("PROPFIND", "/")
        .with_status(207)
        .with_header("content-type", "application/xml")
        .with_body(r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:"/>"#)
        .create();

    let c = client(3);
    let result = c
        .propfind(&format!("{}/", server.url()), 0, "<x/>")
        .expect("propfind");
    assert_eq!(result.status, 207);
    assert!(c.retries_observed() >= 1);
}

#[test]
fn xml_entities_decoded_in_displayname_and_calendar_data() {
    let body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/cal/1.ics</d:href>
    <d:propstat><d:prop>
      <d:displayname>Sales &amp; Marketing &lt;Team&gt;</d:displayname>
      <c:calendar-data>BEGIN:VEVENT
SUMMARY:Q&amp;A &amp; review
END:VEVENT</c:calendar-data>
    </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#;
    let r = parse_multistatus(Cursor::new(body), "https://x/").expect("parse");
    assert_eq!(r.len(), 1);
    assert_eq!(
        r[0].props.displayname.as_deref(),
        Some("Sales & Marketing <Team>")
    );
    let cal = r[0].props.calendar_data.as_deref().unwrap();
    assert!(
        cal.contains("SUMMARY:Q&A & review"),
        "calendar-data entities must be decoded: {cal}"
    );
}

#[test]
fn duplicate_response_hrefs_collapsed_on_parse() {
    let body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/x</d:href>
    <d:propstat><d:prop><d:getetag>"a"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/x</d:href>
    <d:propstat><d:prop><d:getetag>"b"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#;
    let r = parse_multistatus(Cursor::new(body), "https://x/").expect("parse");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].props.etag.as_deref(), Some("\"a\""));
}

#[test]
fn xml_request_bodies_match_expected_shape() {
    let cup = xml::propfind_current_user_principal();
    assert!(cup.contains("<d:current-user-principal/>"));
    let chs = xml::propfind_calendar_home_set();
    assert!(chs.contains("<c:calendar-home-set/>"));
    let ahs = xml::propfind_addressbook_home_set();
    assert!(ahs.contains("<c:addressbook-home-set/>"));
    let listing = xml::propfind_webdav_listing();
    assert!(listing.contains("<d:getcontentlength/>"));
}

#[test]
fn report_multiget_against_mock_returns_calendar_data() {
    let mut server = mockito::Server::new();
    let body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/dav/cal/u/d/a.ics</d:href>
    <d:propstat>
      <d:prop>
        <d:getetag>"v1"</d:getetag>
        <c:calendar-data>BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//Test//EN
BEGIN:VEVENT
UID:e1@example.com
DTSTAMP:20260101T000000Z
DTSTART:20260101T090000Z
DTEND:20260101T100000Z
SUMMARY:Hi
END:VEVENT
END:VCALENDAR
</c:calendar-data>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
    let _m = multistatus_response(&mut server, "REPORT", "/dav/cal/u/d/", body);
    let c = client(0);
    let result = c
        .report(&format!("{}/dav/cal/u/d/", server.url()), 1, "<x/>")
        .expect("report");
    assert_eq!(result.status, 207);
    let parsed = parse_multistatus(Cursor::new(&result.body), "https://x/").expect("parse");
    assert_eq!(parsed.len(), 1);
    assert!(parsed[0].props.calendar_data.is_some());
}

#[test]
fn webdav_listing_returns_collection_and_file() {
    let mut server = mockito::Server::new();
    let url = server.url();
    let body = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>{url}/files/</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>{url}/files/sub/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/></d:resourcetype>
        <d:displayname>sub</d:displayname>
        <d:creationdate>2026-01-01T00:00:00Z</d:creationdate>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>{url}/files/hello.txt</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype/>
        <d:getcontenttype>text/plain</d:getcontenttype>
        <d:getetag>"f1"</d:getetag>
        <d:getcontentlength>5</d:getcontentlength>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let _m = multistatus_response(&mut server, "PROPFIND", "/files/", &body);
    let c = client(0);
    let result = c
        .propfind(&format!("{url}/files/"), 1, "<x/>")
        .expect("propfind");
    let parsed =
        parse_multistatus(Cursor::new(&result.body), &format!("{url}/files/")).expect("parse");
    assert!(
        parsed
            .iter()
            .any(|r| !r.props.is_collection && r.href.as_str().ends_with("hello.txt")),
        "expected a non-collection file in listing"
    );
    assert!(
        parsed
            .iter()
            .any(|r| r.props.is_collection && r.href.as_str().ends_with("/sub/")),
        "expected a sub-collection in listing"
    );
}

#[test]
fn google_usage_limits_403_classified_retryable() {
    use vandelay::dav::retry::{DavOutcome, classify};
    let body = br#"{"error":{"errors":[{"domain":"usageLimits"}]}}"#;
    assert_eq!(classify(403, body), DavOutcome::Retryable);
}

#[test]
fn non_quota_403_is_auth() {
    use vandelay::dav::retry::{DavOutcome, classify};
    assert_eq!(classify(403, b"forbidden"), DavOutcome::Auth);
}

#[test]
fn server_extra_unsolicited_href_in_multiget_is_logged_not_crashed() {
    let body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/dav/cal/u/d/requested.ics</d:href>
    <d:propstat>
      <d:prop><d:getetag>"v"</d:getetag><c:calendar-data>X</c:calendar-data></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/cal/u/d/unsolicited.ics</d:href>
    <d:propstat>
      <d:prop><d:getetag>"v"</d:getetag><c:calendar-data>Y</c:calendar-data></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
    let r = parse_multistatus(Cursor::new(body), "https://x/").expect("parse");
    assert_eq!(r.len(), 2);
}

#[test]
fn server_404_on_individual_item_in_multiget_recorded_as_vanished() {
    let body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/dav/cal/u/d/gone.ics</d:href>
    <d:status>HTTP/1.1 404 Not Found</d:status>
  </d:response>
</d:multistatus>"#;
    let r = parse_multistatus(Cursor::new(body), "https://x/").expect("parse");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].status, Some(404));
}

#[test]
fn discovery_via_well_known_redirect_307() {
    let mut server = mockito::Server::new();
    let url = server.url();

    let empty = r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:"/>"#;
    let _root = multistatus_response(&mut server, "PROPFIND", "/", empty);

    let principal_with_homeset = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>{url}/dav/principals/u/</d:href>
    <d:propstat>
      <d:prop>
        <d:current-user-principal><d:href>{url}/dav/principals/u/</d:href></d:current-user-principal>
        <c:calendar-home-set><d:href>{url}/dav/cal/u/</d:href></c:calendar-home-set>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let collections_body = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>{url}/dav/cal/u/default/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
        <d:displayname>Default</d:displayname>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );

    let _well_known = multistatus_response(
        &mut server,
        "PROPFIND",
        "/.well-known/caldav",
        &principal_with_homeset,
    );
    let _principal = multistatus_response(
        &mut server,
        "PROPFIND",
        "/dav/principals/u/",
        &principal_with_homeset,
    );
    let _collections =
        multistatus_response(&mut server, "PROPFIND", "/dav/cal/u/", &collections_body);

    let c = client(0);
    let disc = discover(&c, DavKind::Caldav, &url).expect("discover");
    assert_eq!(disc.collections.len(), 1);
    assert!(disc.home_set_url.ends_with("/dav/cal/u/"));
}

#[test]
fn discovery_when_server_omits_current_user_principal() {
    let mut server = mockito::Server::new();
    let url = server.url();

    let empty = r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:"/>"#;
    let _step1 = multistatus_response(&mut server, "PROPFIND", "/", empty);

    let no_principal_with_homeset = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>{url}/</d:href>
    <d:propstat>
      <d:prop>
        <c:calendar-home-set><d:href>{url}/dav/cal/u/</d:href></c:calendar-home-set>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let collections_body = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>{url}/dav/cal/u/default/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let _step2 = multistatus_response(&mut server, "PROPFIND", "/", &no_principal_with_homeset);
    let _collections =
        multistatus_response(&mut server, "PROPFIND", "/dav/cal/u/", &collections_body);

    let c = client(0);
    let disc = discover(&c, DavKind::Caldav, &url).expect("discover");
    assert_eq!(disc.collections.len(), 1);
    assert!(
        disc.home_set_url.ends_with("/dav/cal/u/"),
        "home-set discovered even without principal href"
    );
}

#[test]
fn propfind_depth1_self_inclusion_is_filtered_during_enumeration() {
    let body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/dav/cal/u/d/</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/><c:calendar/></d:resourcetype></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/cal/u/d/event1.ics</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype/>
        <d:getetag>"v1"</d:getetag>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
    let parsed = parse_multistatus(Cursor::new(body), "https://x/dav/cal/u/d/").expect("parse");
    let non_collection: Vec<_> = parsed.iter().filter(|r| !r.props.is_collection).collect();
    let collection: Vec<_> = parsed.iter().filter(|r| r.props.is_collection).collect();
    assert_eq!(non_collection.len(), 1);
    assert_eq!(collection.len(), 1);
}

#[test]
fn item_missing_etag_records_empty_etag() {
    let body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/dav/cal/u/d/event.ics</d:href>
    <d:propstat>
      <d:prop><d:resourcetype/></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
    let r = parse_multistatus(Cursor::new(body), "https://x/").expect("parse");
    assert!(r[0].props.etag.is_none(), "missing etag should be None");
}

#[test]
fn etag_mismatch_between_enumerate_and_multiget_uses_new_etag() {
    let multiget_body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/dav/cal/u/d/event.ics</d:href>
    <d:propstat>
      <d:prop>
        <d:getetag>"v2"</d:getetag>
        <c:calendar-data>BEGIN:VCALENDAR
END:VCALENDAR</c:calendar-data>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
    let r = parse_multistatus(Cursor::new(multiget_body), "https://x/").expect("parse");
    assert_eq!(r[0].props.etag.as_deref(), Some("\"v2\""));
}

#[test]
fn webdav_cycle_breaks_on_lex_smallest() {
    let body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/files/a/</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/files/a/b/</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
    let parsed = parse_multistatus(Cursor::new(body), "https://x/").expect("parse");
    let mut hrefs: Vec<&str> = parsed.iter().map(|r| r.href.as_str()).collect();
    hrefs.sort();
    assert_eq!(hrefs[0], "/files/a/");
}

#[test]
fn multiget_405_method_not_allowed_triggers_get_fallback() {
    use vandelay::dav::retry::{DavOutcome, classify};
    assert_eq!(classify(405, b""), DavOutcome::Fatal);
}

#[test]
fn parser_accepts_response_with_propstat_404_on_one_property() {
    let body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/dav/cal/u/d/e.ics</d:href>
    <d:propstat>
      <d:prop><d:getetag>"v"</d:getetag></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
    <d:propstat>
      <d:prop><d:custom-property/></d:prop>
      <d:status>HTTP/1.1 404 Not Found</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
    let r = parse_multistatus(Cursor::new(body), "https://x/").expect("parse");
    assert_eq!(r[0].props.etag.as_deref(), Some("\"v\""));
    assert_eq!(r[0].propstat_errors, vec![404]);
}

#[test]
fn streaming_multistatus_returns_responses_via_propfind_responses() {
    let mut server = mockito::Server::new();
    let url = server.url();
    let body = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>{url}/dav/cal/u/d/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
        <d:displayname>Default</d:displayname>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let _m = multistatus_response(&mut server, "PROPFIND", "/dav/cal/u/", &body);
    let c = client(0);
    let ms = c
        .propfind_responses(
            &format!("{url}/dav/cal/u/"),
            1,
            "<x/>",
            &format!("{url}/dav/cal/u/"),
        )
        .expect("propfind_responses");
    assert_eq!(ms.status, 207);
    assert_eq!(ms.responses.len(), 1);
    assert!(ms.responses[0].props.is_calendar);
}

#[test]
fn streaming_propfind_propagates_401_as_auth_error() {
    let mut server = mockito::Server::new();
    let _m = server.mock("PROPFIND", "/").with_status(401).create();
    let c = client(0);
    let err = c
        .propfind_responses(&format!("{}/", server.url()), 0, "<x/>", &server.url())
        .unwrap_err();
    assert!(matches!(err, JmapError::Auth(_)), "got {err:?}");
}

#[test]
fn streaming_propfind_404_returns_empty_responses() {
    let mut server = mockito::Server::new();
    let _m = server
        .mock("PROPFIND", "/missing")
        .with_status(404)
        .create();
    let c = client(0);
    let ms = c
        .propfind_responses(
            &format!("{}/missing", server.url()),
            0,
            "<x/>",
            &server.url(),
        )
        .expect("propfind_responses");
    assert_eq!(ms.status, 404);
    assert!(ms.responses.is_empty());
}

#[test]
fn source_change_protection_rejects_different_account_id() {
    use rusqlite::Connection;
    use vandelay::db::{init, sources};

    let conn = Connection::open_in_memory().unwrap();
    init::apply_schema(&conn).unwrap();
    let first = sources::SourceKey {
        kind: "caldav".to_owned(),
        session_url: "https://x".to_owned(),
        account_id: "alice".to_owned(),
    };
    sources::upsert_source(&conn, &first, Some("alice"), "alice").unwrap();

    let conflict = sources::conflicting_source(&conn, "caldav", "https://x", "bob").expect("query");
    assert!(
        conflict.is_some(),
        "different account id under same kind+url is a conflict"
    );
}

#[test]
fn discovery_follows_real_307_from_well_known_to_dav_root() {
    let mut server = mockito::Server::new();
    let url = server.url();

    let empty = r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:"/>"#;
    let _step1 = multistatus_response(&mut server, "PROPFIND", "/", empty);

    let _redirect = server
        .mock("PROPFIND", "/.well-known/caldav")
        .with_status(307)
        .with_header("location", &format!("{url}/dav/"))
        .with_body("")
        .create();

    let principal_with_homeset = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>{url}/dav/</d:href>
    <d:propstat>
      <d:prop>
        <d:current-user-principal><d:href>{url}/dav/principals/u/</d:href></d:current-user-principal>
        <c:calendar-home-set><d:href>{url}/dav/cal/u/</d:href></c:calendar-home-set>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let collections_body = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>{url}/dav/cal/u/default/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let _on_dav = multistatus_response(&mut server, "PROPFIND", "/dav/", &principal_with_homeset);
    let _on_principal = multistatus_response(
        &mut server,
        "PROPFIND",
        "/dav/principals/u/",
        &principal_with_homeset,
    );
    let _collections =
        multistatus_response(&mut server, "PROPFIND", "/dav/cal/u/", &collections_body);

    let c = client(0);
    let disc = discover(&c, DavKind::Caldav, &url).expect("discover");
    assert_eq!(disc.collections.len(), 1);
    assert!(disc.home_set_url.ends_with("/dav/cal/u/"));
}

#[test]
fn discovery_skips_vfreebusy_only_calendars() {
    let mut server = mockito::Server::new();
    let url = server.url();
    let body = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>{url}/dav/cal/u/work/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
        <d:displayname>Work</d:displayname>
        <c:supported-calendar-component-set>
          <c:comp name="VEVENT"/>
        </c:supported-calendar-component-set>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>{url}/dav/cal/u/freebusy/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
        <d:displayname>Free/Busy</d:displayname>
        <c:supported-calendar-component-set>
          <c:comp name="VFREEBUSY"/>
        </c:supported-calendar-component-set>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let _m = multistatus_response(&mut server, "PROPFIND", "/dav/cal/u/", &body);
    let c = client(0);
    let disc = discover(&c, DavKind::Caldav, &format!("{url}/dav/cal/u/")).expect("discover");
    assert_eq!(disc.collections.len(), 1);
    assert_eq!(
        disc.collections[0].props.displayname.as_deref(),
        Some("Work")
    );
}

#[test]
fn webdav_discovery_keeps_only_self_row_as_root() {
    let mut server = mockito::Server::new();
    let url = server.url();
    let body = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>{url}/files/u/sub1/</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/></d:resourcetype><d:displayname>sub1</d:displayname></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>{url}/files/u/</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/></d:resourcetype><d:displayname>root</d:displayname></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let _m = multistatus_response(&mut server, "PROPFIND", "/files/u/", &body);
    let c = client(0);
    let disc = discover(&c, DavKind::Webdav, &format!("{url}/files/u/")).expect("discover");
    assert_eq!(disc.collections.len(), 1, "only self-row returned");
    assert!(disc.collections[0].href.as_str().ends_with("/files/u/"));
}

#[test]
fn webdav_root_collection_is_not_materialised_children_map_to_target_root() {
    use rusqlite::Connection;
    use vandelay::dav::discover::DiscoveredCollection;
    use vandelay::dav::href::Href;
    use vandelay::dav::parse::ResourceProps;
    use vandelay::db;
    use vandelay::db::sources::SourceKey;
    use vandelay::logging::Logger;
    use vandelay::sync::TypeCounts;
    use vandelay::sync::import_dav::tree::{WebDavCtx, reconcile_filenodes};

    let mut server = mockito::Server::new();
    let url = server.url();

    let root_body = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>{url}/dav/file/u/</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/></d:resourcetype><d:displayname>System administrator</d:displayname></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>{url}/dav/file/u/sub/</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/></d:resourcetype><d:displayname>sub</d:displayname></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>{url}/dav/file/u/top.txt</d:href>
    <d:propstat>
      <d:prop><d:resourcetype/><d:getcontenttype>text/plain</d:getcontenttype><d:getetag>"t1"</d:getetag></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let sub_body = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>{url}/dav/file/u/sub/</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/></d:resourcetype><d:displayname>sub</d:displayname></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>{url}/dav/file/u/sub/inner.txt</d:href>
    <d:propstat>
      <d:prop><d:resourcetype/><d:getcontenttype>text/plain</d:getcontenttype><d:getetag>"i1"</d:getetag></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let _root = multistatus_response(&mut server, "PROPFIND", "/dav/file/u/", &root_body);
    let _sub = multistatus_response(&mut server, "PROPFIND", "/dav/file/u/sub/", &sub_body);
    let _f1 = server
        .mock("GET", "/dav/file/u/top.txt")
        .with_status(200)
        .with_header("content-type", "text/plain")
        .with_body("toplevel\n")
        .create();
    let _f2 = server
        .mock("GET", "/dav/file/u/sub/inner.txt")
        .with_status(200)
        .with_header("content-type", "text/plain")
        .with_body("inner\n")
        .create();

    let mut conn = Connection::open_in_memory().unwrap();
    db::init::apply_schema(&conn).unwrap();
    let source_id = db::sources::upsert_source(
        &conn,
        &SourceKey {
            kind: "webdav".to_owned(),
            session_url: url.clone(),
            account_id: format!("{url}/dav/file/u/"),
        },
        Some("u"),
        "u",
    )
    .unwrap();

    let root = DiscoveredCollection {
        url: format!("{url}/dav/file/u/"),
        href: Href::from_normalised("/dav/file/u/".to_owned()),
        props: ResourceProps {
            is_collection: true,
            displayname: Some("System administrator".to_owned()),
            ..Default::default()
        },
    };
    let c = client(0);
    let ctx = WebDavCtx {
        client: &c,
        source_id,
        base_url: &url,
        dav_connections: 2,
        logger: Logger::from_flags(false, 0),
    };
    let mut counts = TypeCounts::default();
    reconcile_filenodes(&mut conn, &ctx, &root, &mut counts).expect("reconcile");

    let total: i64 = conn
        .query_row("SELECT count(*) FROM file_nodes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        total, 3,
        "only sub/, top.txt and inner.txt land; the root collection is a virtual mount point, not a node"
    );

    let admin_named: i64 = conn
        .query_row(
            "SELECT count(*) FROM file_nodes WHERE name = 'System administrator'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        admin_named, 0,
        "the account display name must not become a directory (issue #18)"
    );

    let top_parent: Option<i64> = conn
        .query_row(
            "SELECT parent_id FROM file_nodes WHERE name = 'top.txt'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        top_parent, None,
        "a file at the root maps to the target's implicit root (NULL parent)"
    );

    let (sub_id, sub_parent): (i64, Option<i64>) = conn
        .query_row(
            "SELECT id, parent_id FROM file_nodes WHERE name = 'sub'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        sub_parent, None,
        "a directory at the root maps to the target's implicit root (NULL parent)"
    );

    let inner_parent: Option<i64> = conn
        .query_row(
            "SELECT parent_id FROM file_nodes WHERE name = 'inner.txt'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        inner_parent,
        Some(sub_id),
        "a nested file keeps its real parent directory"
    );
}

#[test]
fn streaming_propfind_strips_control_chars_before_parse() {
    let mut server = mockito::Server::new();
    let url = server.url();
    let dirty = format!(
        "<?xml version=\"1.0\"?>\
\x01<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\">\
<d:response>\
<d:href>{url}/dav/cal/u/d/</d:href>\
<d:propstat>\
<d:prop><d:resourcetype><d:collection/><c:calendar/></d:resourcetype></d:prop>\
<d:status>HTTP/1.1 200 OK</d:status>\
</d:propstat>\
</d:response>\
</d:multistatus>"
    );
    let _m = server
        .mock("PROPFIND", "/dav/cal/u/")
        .with_status(207)
        .with_header("content-type", "application/xml; charset=utf-8")
        .with_body(dirty)
        .create();
    let c = client(0);
    let ms = c
        .propfind_responses(
            &format!("{url}/dav/cal/u/"),
            1,
            "<x/>",
            &format!("{url}/dav/cal/u/"),
        )
        .expect("propfind_responses");
    assert_eq!(ms.responses.len(), 1);
    assert!(ms.responses[0].props.is_calendar);
}

#[test]
fn get_stream_404_returns_ok_with_empty_body() {
    use std::io::Read;
    let mut server = mockito::Server::new();
    let _m = server
        .mock("GET", "/dav/file/u/missing.txt")
        .with_status(404)
        .create();
    let c = client(0);
    let mut stream = c
        .get_stream(&format!("{}/dav/file/u/missing.txt", server.url()))
        .expect("get_stream Ok on 404");
    assert_eq!(stream.status, 404);
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).expect("read empty");
    assert!(buf.is_empty());
}

#[test]
fn get_stream_429_with_retry_after_honours_header() {
    let mut server = mockito::Server::new();
    let _m1 = server
        .mock("GET", "/dav/file/u/big.bin")
        .with_status(429)
        .with_header("retry-after", "1")
        .with_body("rate-limited")
        .create();
    let _m2 = server
        .mock("GET", "/dav/file/u/big.bin")
        .with_status(200)
        .with_header("etag", "\"v\"")
        .with_body(b"abc")
        .create();
    let c = client(3);
    let start = std::time::Instant::now();
    let _stream = c
        .get_stream(&format!("{}/dav/file/u/big.bin", server.url()))
        .expect("get_stream eventually succeeds");
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() >= 900,
        "expected Retry-After 1s honoured, elapsed={elapsed:?}"
    );
    assert!(c.retry_after_sleeps() >= 1);
}

#[test]
fn execute_30x_without_location_is_fatal_not_loop() {
    let mut server = mockito::Server::new();
    let _m = server
        .mock("PROPFIND", "/no-location")
        .with_status(307)
        .with_body("missing Location")
        .create();
    let c = client(0);
    let err = c
        .propfind(&format!("{}/no-location", server.url()), 0, "<x/>")
        .unwrap_err();
    assert!(matches!(err, JmapError::Connect(_)), "got {err:?}");
}

#[test]
fn webdav_display_or_basename_test_via_parser_keeps_basename_semantics() {
    let body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/files/u/Photo.jpg</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype/>
        <d:displayname>differently named.jpg</d:displayname>
        <d:getetag>"f1"</d:getetag>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
    let parsed = parse_multistatus(Cursor::new(body), "https://x/").expect("parse");
    assert_eq!(
        parsed[0].props.displayname.as_deref(),
        Some("differently named.jpg")
    );
    assert!(parsed[0].href.as_str().ends_with("/Photo.jpg"));
}

#[test]
fn dry_run_writes_nothing_but_emits_per_collection_counts() {
    use std::path::PathBuf;
    use vandelay::logging::Logger;
    use vandelay::sync::CommonConfig;
    use vandelay::sync::import_dav::{DavAuth, DavImportConfig, DavKindArg, run};

    let mut server = mockito::Server::new();
    let url = server.url();
    let collections_body = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>{url}/dav/cal/u/default/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
        <d:displayname>Default</d:displayname>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let items_body = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>{url}/dav/cal/u/default/e1.ics</d:href>
    <d:propstat>
      <d:prop><d:resourcetype/><d:getetag>"v1"</d:getetag></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>{url}/dav/cal/u/default/e2.ics</d:href>
    <d:propstat>
      <d:prop><d:resourcetype/><d:getetag>"v2"</d:getetag></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let _disc = multistatus_response(&mut server, "PROPFIND", "/dav/cal/u/", &collections_body);
    let _items = multistatus_response(&mut server, "PROPFIND", "/dav/cal/u/default/", &items_body);

    let archive: PathBuf = std::env::temp_dir().join(format!(
        "vandelay-dryrun-{}-{}.sqlite",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&archive);
    let common = CommonConfig {
        archive: archive.clone(),
        threads: 1,
        dry_run: true,
        max_retries: 0,
        allow_invalid_certs: true,
        logger: Logger::from_flags(true, 0),
    };
    let config = DavImportConfig {
        kind: DavKindArg::Caldav,
        url: format!("{url}/dav/cal/u/"),
        auth: DavAuth::Basic {
            user: "alice".to_owned(),
            password: "p".to_owned(),
        },
        allow_cleartext: true,
        dav_connections: 1,
        multiget_batch: 50,
        allow_source_change: false,
    };
    let summary = run(common, config).expect("dry-run ok");

    let conn = rusqlite::Connection::open(&archive).unwrap();
    let n_sources: i64 = conn
        .query_row("SELECT count(*) FROM sources", [], |r| r.get(0))
        .unwrap();
    let n_calendars: i64 = conn
        .query_row("SELECT count(*) FROM calendars", [], |r| r.get(0))
        .unwrap();
    let n_events: i64 = conn
        .query_row("SELECT count(*) FROM calendar_events", [], |r| r.get(0))
        .unwrap();
    let n_sync: i64 = conn
        .query_row("SELECT count(*) FROM sync_id_dav", [], |r| r.get(0))
        .unwrap();
    drop(conn);
    let _ = std::fs::remove_file(&archive);

    assert_eq!(n_sources, 0, "dry-run does not write sources");
    assert_eq!(n_calendars, 0, "dry-run does not write calendars");
    assert_eq!(n_events, 0, "dry-run does not write events");
    assert_eq!(n_sync, 0, "dry-run does not write sync_id_dav");

    let calendar_counts = summary
        .per_type
        .iter()
        .find(|(name, _)| *name == "calendar")
        .expect("calendar counts in summary");
    let event_counts = summary
        .per_type
        .iter()
        .find(|(name, _)| *name == "calendarevent")
        .expect("event counts in summary");
    assert_eq!(calendar_counts.1.created, 1);
    assert_eq!(event_counts.1.created, 2);
}

#[test]
fn dav_source_change_protection_fires_across_users_on_same_root() {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use std::path::PathBuf;
    use vandelay::logging::Logger;
    use vandelay::sync::CommonConfig;
    use vandelay::sync::import_dav::{DavAuth, DavImportConfig, DavKindArg, run};

    let mut server = mockito::Server::new();
    let url = server.url();

    let listing = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>{url}/dav/cal/shared/default/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
        <d:displayname>Default</d:displayname>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let _listing = server
        .mock("PROPFIND", "/dav/cal/")
        .match_header("depth", "1")
        .with_status(207)
        .with_header("content-type", "application/xml; charset=utf-8")
        .with_body(&listing)
        .create();

    let empty_items = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>{url}/dav/cal/shared/default/</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/><c:calendar/></d:resourcetype></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let _items = server
        .mock("PROPFIND", "/dav/cal/shared/default/")
        .with_status(207)
        .with_header("content-type", "application/xml; charset=utf-8")
        .with_body(&empty_items)
        .create();

    let auth_a = format!("Basic {}", STANDARD.encode("secondary@vandelay.org:passA"));
    let auth_b = format!("Basic {}", STANDARD.encode("tertiary@vandelay.org:passB"));
    let principal = |who: &str| {
        format!(
            r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>{url}/dav/cal/</d:href>
    <d:propstat>
      <d:prop><d:current-user-principal><d:href>{url}/dav/principals/{who}/</d:href></d:current-user-principal></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
        )
    };
    let _pa = server
        .mock("PROPFIND", "/dav/cal/")
        .match_header("depth", "0")
        .match_header("authorization", auth_a.as_str())
        .with_status(207)
        .with_header("content-type", "application/xml; charset=utf-8")
        .with_body(principal("secondary@vandelay.org"))
        .create();
    let _pb = server
        .mock("PROPFIND", "/dav/cal/")
        .match_header("depth", "0")
        .match_header("authorization", auth_b.as_str())
        .with_status(207)
        .with_header("content-type", "application/xml; charset=utf-8")
        .with_body(principal("tertiary@vandelay.org"))
        .create();

    let archive: PathBuf = std::env::temp_dir().join(format!(
        "vandelay-dav-srcchange-{}-{}.sqlite",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&archive);

    let common = |archive: &PathBuf| CommonConfig {
        archive: archive.clone(),
        threads: 1,
        dry_run: false,
        max_retries: 0,
        allow_invalid_certs: true,
        logger: Logger::from_flags(false, 0),
    };
    let config = |user: &str, pass: &str| DavImportConfig {
        kind: DavKindArg::Caldav,
        url: format!("{url}/dav/cal/"),
        auth: DavAuth::Basic {
            user: user.to_owned(),
            password: pass.to_owned(),
        },
        allow_cleartext: true,
        dav_connections: 1,
        multiget_batch: 50,
        allow_source_change: false,
    };

    run(common(&archive), config("secondary@vandelay.org", "passA")).expect("user A import ok");
    let err = run(common(&archive), config("tertiary@vandelay.org", "passB")).unwrap_err();
    let _ = std::fs::remove_file(&archive);
    assert!(
        matches!(err, vandelay::error::Error::SourceChange(_)),
        "importing a different user into the same archive must trigger source-change protection; got {err:?}"
    );
}

#[test]
fn source_change_protection_rejects_different_session_url_same_account() {
    use rusqlite::Connection;
    use vandelay::db::{init, sources};

    let conn = Connection::open_in_memory().unwrap();
    init::apply_schema(&conn).unwrap();
    let first = sources::SourceKey {
        kind: "caldav".to_owned(),
        session_url: "https://host1.example".to_owned(),
        account_id: "alice".to_owned(),
    };
    sources::upsert_source(&conn, &first, Some("alice"), "alice").unwrap();
    let conflict = sources::conflicting_source(&conn, "caldav", "https://host2.example", "alice")
        .expect("query");
    assert!(
        conflict.is_some(),
        "different session_url under same kind+account is a conflict"
    );
}

#[test]
fn etag_missing_in_enumeration_propagates_to_storage_layer() {
    let body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/dav/cal/u/d/no-etag.ics</d:href>
    <d:propstat>
      <d:prop><d:resourcetype/></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/cal/u/d/with-etag.ics</d:href>
    <d:propstat>
      <d:prop><d:resourcetype/><d:getetag>"v1"</d:getetag></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
    let parsed = parse_multistatus(Cursor::new(body), "https://x/").expect("parse");
    let no_etag = parsed
        .iter()
        .find(|r| r.href.as_str().ends_with("no-etag.ics"))
        .unwrap();
    let with_etag = parsed
        .iter()
        .find(|r| r.href.as_str().ends_with("with-etag.ics"))
        .unwrap();
    assert!(no_etag.props.etag.is_none(), "missing etag is None");
    assert_eq!(with_etag.props.etag.as_deref(), Some("\"v1\""));
}

#[test]
fn mixed_source_archive_allows_distinct_kinds_against_same_url() {
    use rusqlite::Connection;
    use vandelay::db::{init, sources};

    let conn = Connection::open_in_memory().unwrap();
    init::apply_schema(&conn).unwrap();
    let caldav = sources::SourceKey {
        kind: "caldav".to_owned(),
        session_url: "https://x".to_owned(),
        account_id: "alice".to_owned(),
    };
    let carddav = sources::SourceKey {
        kind: "carddav".to_owned(),
        session_url: "https://x".to_owned(),
        account_id: "alice".to_owned(),
    };
    let webdav = sources::SourceKey {
        kind: "webdav".to_owned(),
        session_url: "https://x".to_owned(),
        account_id: "alice".to_owned(),
    };
    let id_cal = sources::upsert_source(&conn, &caldav, Some("alice"), "alice").unwrap();
    let id_card = sources::upsert_source(&conn, &carddav, Some("alice"), "alice").unwrap();
    let id_web = sources::upsert_source(&conn, &webdav, Some("alice"), "alice").unwrap();
    assert_ne!(id_cal, id_card);
    assert_ne!(id_card, id_web);
    assert_ne!(id_cal, id_web);

    let no_conflict = sources::conflicting_source(&conn, "caldav", "https://x", "alice").unwrap();
    assert!(no_conflict.is_none());
}

fn dav_archive() -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "vandelay-mockdav-{}-{:?}-{n}.sqlite",
        std::process::id(),
        std::thread::current().id(),
    ));
    let _ = std::fs::remove_file(&p);
    p
}

fn caldav_import_config(url: &str) -> vandelay::sync::import_dav::DavImportConfig {
    vandelay::sync::import_dav::DavImportConfig {
        kind: vandelay::sync::import_dav::DavKindArg::Caldav,
        url: url.to_owned(),
        auth: vandelay::sync::import_dav::DavAuth::Basic {
            user: "u".to_owned(),
            password: "p".to_owned(),
        },
        allow_cleartext: true,
        dav_connections: 1,
        multiget_batch: 8,
        allow_source_change: false,
    }
}

fn dav_common(archive: &std::path::Path) -> vandelay::sync::CommonConfig {
    vandelay::sync::CommonConfig {
        archive: archive.to_path_buf(),
        threads: 1,
        dry_run: false,
        max_retries: 0,
        allow_invalid_certs: false,
        logger: vandelay::logging::Logger::from_flags(true, 0),
    }
}

fn row_count(archive: &std::path::Path, table: &str) -> i64 {
    let conn = rusqlite::Connection::open(archive).expect("open archive");
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .expect("count")
}

#[test]
fn forbidden_collection_is_a_per_unit_failure_and_the_others_still_import() {
    let mut server = mockito::Server::new();
    let url = server.url();

    let home_set = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>{url}/dav/cal/u/work/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
        <d:displayname>Work</d:displayname>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>{url}/dav/cal/u/shared/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
        <d:displayname>Shared</d:displayname>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let _discovery = multistatus_response(&mut server, "PROPFIND", "/dav/cal/u/", &home_set);

    let work_items = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>{url}/dav/cal/u/work/e1.ics</d:href>
    <d:propstat>
      <d:prop><d:resourcetype/><d:getetag>"v1"</d:getetag></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let _work_enumeration =
        multistatus_response(&mut server, "PROPFIND", "/dav/cal/u/work/", &work_items);

    let work_data = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>{url}/dav/cal/u/work/e1.ics</d:href>
    <d:propstat>
      <d:prop>
        <d:getetag>"v1"</d:getetag>
        <c:calendar-data>BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//Test//EN
BEGIN:VEVENT
UID:e1@example.com
DTSTAMP:20260101T000000Z
DTSTART:20260101T090000Z
DTEND:20260101T100000Z
SUMMARY:Standup
END:VEVENT
END:VCALENDAR
</c:calendar-data>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let _work_multiget =
        multistatus_response(&mut server, "REPORT", "/dav/cal/u/work/", &work_data);

    let forbidden = server
        .mock("PROPFIND", "/dav/cal/u/shared/")
        .with_status(403)
        .with_header("content-type", "text/plain")
        .with_body("you may not read this calendar")
        .expect_at_least(1)
        .create();

    let archive = dav_archive();
    let summary = vandelay::sync::import_dav::run(
        dav_common(&archive),
        caldav_import_config(&format!("{url}/dav/cal/u/")),
    )
    .expect("a forbidden collection must not abort the run");

    forbidden.assert();

    let events = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "calendarevent")
        .map(|(_, c)| c.clone())
        .expect("calendarevent counts");
    assert_eq!(events.failed, 1, "the forbidden calendar counts as failed");
    assert_eq!(events.fetched, 1, "the readable calendar still imported");
    assert!(
        summary.any_failed(),
        "a per-unit failure ends the run at exit 5"
    );

    assert_eq!(row_count(&archive, "calendars"), 2);
    assert_eq!(row_count(&archive, "calendar_events"), 1);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn credentials_rejected_during_discovery_still_aborts_the_run() {
    let mut server = mockito::Server::new();
    let url = server.url();

    let _rejected = server
        .mock("PROPFIND", mockito::Matcher::Any)
        .with_status(401)
        .with_header("content-type", "text/plain")
        .with_body("bad password")
        .expect_at_least(1)
        .create();

    let archive = dav_archive();
    let err = vandelay::sync::import_dav::run(
        dav_common(&archive),
        caldav_import_config(&format!("{url}/dav/cal/u/")),
    )
    .expect_err("credentials rejected for the principal aborts the run");

    assert!(
        matches!(err, vandelay::error::Error::Connection(_)),
        "discovery-level auth rejection is a whole-run failure, got {err:?}"
    );
    assert_eq!(err.exit_code(), 2);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn dav_import_reports_both_phases_when_every_collection_fails() {
    let mut server = mockito::Server::new();
    let url = server.url();

    let home_set = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>{url}/dav/cal/u/work/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
        <d:displayname>Work</d:displayname>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let _discovery = multistatus_response(&mut server, "PROPFIND", "/dav/cal/u/", &home_set);
    let _forbidden = server
        .mock("PROPFIND", "/dav/cal/u/work/")
        .with_status(403)
        .with_body("nope")
        .expect_at_least(1)
        .create();

    let archive = dav_archive();
    let outcome = vandelay::sync::import_dav::run_reporting(
        dav_common(&archive),
        caldav_import_config(&format!("{url}/dav/cal/u/")),
    );
    assert!(outcome.error.is_none());
    assert_eq!(
        outcome
            .summary
            .per_type
            .iter()
            .map(|(t, _)| *t)
            .collect::<Vec<_>>(),
        vec!["calendar", "calendarevent"],
        "both phases are reported even when every collection failed"
    );
    let _ = std::fs::remove_file(&archive);
}
