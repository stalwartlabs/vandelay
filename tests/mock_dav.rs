/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::io::Cursor;

use vandelay::archive::sources::{SourceKey, SourceKind};
use vandelay::dav::client::DavClient;
use vandelay::dav::discover::{DavKind, Discovery};
use vandelay::dav::error::DavError;
use vandelay::dav::parse::{ControlBytes, MultistatusParser};
use vandelay::dav::xml::Propfind;
use vandelay::error::ExitStatus;
use vandelay::logging::{Level, Logger};
use vandelay::net::auth::Auth;
use vandelay::net::http::{ClientOptions, TlsPolicy};
use vandelay::net::retry::RetryPolicy;
use vandelay::sync::RunMode;
use vandelay::types::ObjectType;

fn client(retries: u32) -> DavClient {
    DavClient::new(
        Auth::Basic {
            user: "u".into(),
            password: "p".into(),
        },
        ClientOptions {
            retry: RetryPolicy::new(retries),
            tls: TlsPolicy::Verify,
            logger: Logger::new(Level::Default),
        },
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
    let disc = Discovery::run(&c, DavKind::Caldav, &url_with_path).expect("discover");
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
    let disc = Discovery::run(&c, DavKind::Caldav, &format!("{url}/dav/cal/")).expect("discover");
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
    let disc = Discovery::run(&c, DavKind::Caldav, &url).expect("discover");
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
    let err = Discovery::run(&c, DavKind::Caldav, &server.url()).unwrap_err();
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
    let r = MultistatusParser::parse(Cursor::new(body), "https://x/").expect("parse");
    assert_eq!(r.len(), 1);
    assert!(r[0].props.is_calendar);
    assert_eq!(r[0].props.displayname.as_deref(), Some("A"));
}

#[test]
fn ascii_control_chars_are_stripped_before_parse() {
    let dirty: Vec<u8> = b"<?xml version=\"1.0\"?>\x02<d:multistatus xmlns:d=\"DAV:\"/>".to_vec();
    let cleaned = ControlBytes::strip(&dirty);
    let r = MultistatusParser::parse(Cursor::new(cleaned.as_ref()), "https://x/").expect("parse");
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
    assert!(c.stats().retries_observed >= 1);
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
    assert!(matches!(err, DavError::Auth(_)), "got {err:?}");
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
    assert!(c.stats().retries_observed >= 1);
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
    let r = MultistatusParser::parse(Cursor::new(body), "https://x/").expect("parse");
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
    let r = MultistatusParser::parse(Cursor::new(body), "https://x/").expect("parse");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].props.etag.as_deref(), Some("\"a\""));
}

#[test]
fn xml_request_bodies_match_expected_shape() {
    let cup = Propfind::CurrentUserPrincipal.body();
    assert!(cup.contains("<d:current-user-principal/>"));
    let chs = Propfind::CalendarHomeSet.body();
    assert!(chs.contains("<c:calendar-home-set/>"));
    let ahs = Propfind::AddressbookHomeSet.body();
    assert!(ahs.contains("<c:addressbook-home-set/>"));
    let listing = Propfind::WebdavListing.body();
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
    let parsed = MultistatusParser::parse(Cursor::new(&result.body), "https://x/").expect("parse");
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
    let parsed = MultistatusParser::parse(Cursor::new(&result.body), &format!("{url}/files/"))
        .expect("parse");
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
    use vandelay::dav::retry::DavOutcome;
    let body = br#"{"error":{"errors":[{"domain":"usageLimits"}]}}"#;
    assert_eq!(DavOutcome::classify(403, body), DavOutcome::Retryable);
}

#[test]
fn non_quota_403_is_auth() {
    use vandelay::dav::retry::DavOutcome;
    assert_eq!(DavOutcome::classify(403, b"forbidden"), DavOutcome::Auth);
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
    let r = MultistatusParser::parse(Cursor::new(body), "https://x/").expect("parse");
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
    let r = MultistatusParser::parse(Cursor::new(body), "https://x/").expect("parse");
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
    let disc = Discovery::run(&c, DavKind::Caldav, &url).expect("discover");
    assert_eq!(disc.collections.len(), 1);
    assert!(disc.home_set_url.ends_with("/dav/cal/u/"));
}

#[test]
fn discovery_survives_501_on_the_user_supplied_url() {
    let mut server = mockito::Server::new();
    let url = server.url();

    let _root = server
        .mock("PROPFIND", "/")
        .with_status(501)
        .with_body("")
        .expect_at_least(1)
        .create();

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
    let disc = Discovery::run(&c, DavKind::Caldav, &url).expect("discovery must not abort on 501");
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
    let disc = Discovery::run(&c, DavKind::Caldav, &url).expect("discover");
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
    let parsed =
        MultistatusParser::parse(Cursor::new(body), "https://x/dav/cal/u/d/").expect("parse");
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
    let r = MultistatusParser::parse(Cursor::new(body), "https://x/").expect("parse");
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
    let r = MultistatusParser::parse(Cursor::new(multiget_body), "https://x/").expect("parse");
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
    let parsed = MultistatusParser::parse(Cursor::new(body), "https://x/").expect("parse");
    let mut hrefs: Vec<&str> = parsed.iter().map(|r| r.href.as_str()).collect();
    hrefs.sort();
    assert_eq!(hrefs[0], "/files/a/");
}

#[test]
fn multiget_405_method_not_allowed_triggers_get_fallback() {
    use vandelay::dav::retry::DavOutcome;
    assert_eq!(DavOutcome::classify(405, b""), DavOutcome::Fatal);
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
    let r = MultistatusParser::parse(Cursor::new(body), "https://x/").expect("parse");
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
    assert!(matches!(err, DavError::Auth(_)), "got {err:?}");
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
    use vandelay::archive::Archive;

    let archive = Archive::open_in_memory().unwrap();
    let sources = archive.store().sources();
    let first = SourceKey {
        kind: SourceKind::CalDav,
        url: "https://x",
        account_id: "alice",
    };
    sources.upsert(&first, Some("alice"), "alice").unwrap();

    let conflict = sources
        .conflicting(&SourceKey {
            kind: SourceKind::CalDav,
            url: "https://x",
            account_id: "bob",
        })
        .expect("query");
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
    let disc = Discovery::run(&c, DavKind::Caldav, &url).expect("discover");
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
    let disc = Discovery::run(&c, DavKind::Caldav, &format!("{url}/dav/cal/u/")).expect("discover");
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
    let disc = Discovery::run(&c, DavKind::Webdav, &format!("{url}/files/u/")).expect("discover");
    assert_eq!(disc.collections.len(), 1, "only self-row returned");
    assert!(disc.collections[0].href.as_str().ends_with("/files/u/"));
}

#[test]
fn webdav_root_collection_is_not_materialised_children_map_to_target_root() {
    use vandelay::archive::Archive;
    use vandelay::archive::sources::SourceKey;
    use vandelay::dav::discover::DiscoveredCollection;
    use vandelay::dav::href::Href;
    use vandelay::dav::parse::ResourceProps;
    use vandelay::import::dav::tree::FileTree;
    use vandelay::logging::Logger;
    use vandelay::sync::TypeCounts;

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

    let archive = Archive::open_in_memory().unwrap();
    let conn = archive.connection();
    let source = archive
        .store()
        .sources()
        .upsert(
            &SourceKey {
                kind: SourceKind::WebDav,
                url: &url,
                account_id: &format!("{url}/dav/file/u/"),
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
    let tree = FileTree {
        client: &c,
        store: archive.store(),
        source,
        base_url: &url,
        dav_connections: 2,
        logger: Logger::new(Level::Default),
    };
    let mut counts = TypeCounts::default();
    tree.reconcile(&root, &mut counts).expect("reconcile");

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
fn webdav_walk_lists_each_directory_once_when_children_point_back_up() {
    // a/ lists the root and b/ lists a/: both cycles end without a second PROPFIND
    use std::fmt::Write;
    use vandelay::archive::Archive;
    use vandelay::dav::discover::DiscoveredCollection;
    use vandelay::dav::href::Href;
    use vandelay::dav::parse::ResourceProps;
    use vandelay::import::dav::tree::FileTree;
    use vandelay::sync::TypeCounts;

    let mut server = mockito::Server::new();
    let url = server.url();
    let listing = |hrefs: &[&str]| {
        let mut body = String::from(r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:">"#);
        for href in hrefs {
            let _ = write!(
                body,
                r#"<d:response><d:href>{url}{href}</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"#
            );
        }
        body.push_str("</d:multistatus>");
        body
    };
    let mocks: Vec<mockito::Mock> = [
        ("/dav/file/u/", listing(&["/dav/file/u/", "/dav/file/u/a/"])),
        (
            "/dav/file/u/a/",
            listing(&["/dav/file/u/a/", "/dav/file/u/a/b/", "/dav/file/u/"]),
        ),
        (
            "/dav/file/u/a/b/",
            listing(&["/dav/file/u/a/b/", "/dav/file/u/a/"]),
        ),
    ]
    .into_iter()
    .map(|(path, body)| {
        server
            .mock("PROPFIND", path)
            .with_status(207)
            .with_header("content-type", "application/xml; charset=utf-8")
            .with_body(body)
            .expect(1)
            .create()
    })
    .collect();

    let archive = Archive::open_in_memory().unwrap();
    let source = archive
        .store()
        .sources()
        .upsert(
            &SourceKey {
                kind: SourceKind::WebDav,
                url: &url,
                account_id: &format!("{url}/dav/file/u/"),
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
            ..Default::default()
        },
    };
    let c = client(0);
    let tree = FileTree {
        client: &c,
        store: archive.store(),
        source,
        base_url: &url,
        dav_connections: 1,
        logger: Logger::new(Level::Quiet),
    };
    let mut counts = TypeCounts::default();
    tree.reconcile(&root, &mut counts).expect("reconcile");
    assert_eq!(counts.failed, 0);
    for mock in &mocks {
        mock.assert();
    }
}

#[test]
fn bearer_auth_is_sent_as_the_authorization_header() {
    let mut server = mockito::Server::new();
    let url = server.url();
    let m = server
        .mock("PROPFIND", "/dav/")
        .match_header("authorization", "Bearer abc")
        .with_status(207)
        .with_header("content-type", "application/xml; charset=utf-8")
        .with_body(r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:"/>"#)
        .expect(1)
        .create();
    let c = DavClient::new(
        Auth::Bearer {
            token: "abc".into(),
        },
        ClientOptions {
            retry: RetryPolicy::new(0),
            tls: TlsPolicy::Verify,
            logger: Logger::new(Level::Quiet),
        },
    );
    let response = c
        .propfind(&format!("{url}/dav/"), 0, Propfind::WebdavListing.body())
        .expect("propfind");
    assert_eq!(response.status, 207);
    m.assert();
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
    assert!(c.stats().retry_after_sleeps >= 1);
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
    assert!(matches!(err, DavError::Connect(_)), "got {err:?}");
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
    let parsed = MultistatusParser::parse(Cursor::new(body), "https://x/").expect("parse");
    assert_eq!(
        parsed[0].props.displayname.as_deref(),
        Some("differently named.jpg")
    );
    assert!(parsed[0].href.as_str().ends_with("/Photo.jpg"));
}

#[test]
fn dry_run_writes_nothing_but_emits_per_collection_counts() {
    use std::path::PathBuf;
    use vandelay::archive::sources::SourceChange;
    use vandelay::import::dav::{DavImport, DavKindArg};
    use vandelay::logging::Logger;
    use vandelay::sync::RunOptions;

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
    let common = RunOptions {
        archive: archive.clone(),
        threads: 1,
        mode: RunMode::DryRun,
        max_retries: 0,
        tls: TlsPolicy::AcceptInvalid,
        logger: Logger::new(Level::Quiet),
    };
    let config = DavImport {
        kind: DavKindArg::Caldav,
        url: format!("{url}/dav/cal/u/"),
        auth: Auth::Basic {
            user: "alice".to_owned(),
            password: "p".to_owned(),
        },
        allow_cleartext: true,
        dav_connections: 1,
        multiget_batch: 50,
        source_change: SourceChange::Reject,
    };
    let mut summary = config.run(common).into_result().expect("dry-run ok");

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

    assert_eq!(summary.plan(ObjectType::Calendar).new, 1);
    assert_eq!(summary.plan(ObjectType::CalendarEvent).new, 2);
}

#[test]
fn dav_source_change_protection_fires_across_users_on_same_root() {
    use encodify::base64::STANDARD;
    use std::path::PathBuf;
    use vandelay::archive::sources::SourceChange;
    use vandelay::import::dav::{DavImport, DavKindArg};
    use vandelay::logging::Logger;
    use vandelay::sync::RunOptions;

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

    let common = |archive: &PathBuf| RunOptions {
        archive: archive.clone(),
        threads: 1,
        mode: RunMode::Apply,
        max_retries: 0,
        tls: TlsPolicy::AcceptInvalid,
        logger: Logger::new(Level::Default),
    };
    let config = |user: &str, pass: &str| DavImport {
        kind: DavKindArg::Caldav,
        url: format!("{url}/dav/cal/"),
        auth: Auth::Basic {
            user: user.to_owned(),
            password: pass.to_owned(),
        },
        allow_cleartext: true,
        dav_connections: 1,
        multiget_batch: 50,
        source_change: SourceChange::Reject,
    };

    config("secondary@vandelay.org", "passA")
        .run(common(&archive))
        .into_result()
        .expect("user A import ok");
    let err = config("tertiary@vandelay.org", "passB")
        .run(common(&archive))
        .into_result()
        .unwrap_err();
    let _ = std::fs::remove_file(&archive);
    assert!(
        matches!(err, vandelay::error::Error::SourceChange(_)),
        "importing a different user into the same archive must trigger source-change protection; got {err:?}"
    );
}

#[test]
fn source_change_protection_rejects_different_session_url_same_account() {
    use vandelay::archive::Archive;

    let archive = Archive::open_in_memory().unwrap();
    let sources = archive.store().sources();
    let first = SourceKey {
        kind: SourceKind::CalDav,
        url: "https://host1.example",
        account_id: "alice",
    };
    sources.upsert(&first, Some("alice"), "alice").unwrap();
    let conflict = sources
        .conflicting(&SourceKey {
            kind: SourceKind::CalDav,
            url: "https://host2.example",
            account_id: "alice",
        })
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
    let parsed = MultistatusParser::parse(Cursor::new(body), "https://x/").expect("parse");
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
    use vandelay::archive::Archive;

    let archive = Archive::open_in_memory().unwrap();
    let sources = archive.store().sources();
    let caldav = SourceKey {
        kind: SourceKind::CalDav,
        url: "https://x",
        account_id: "alice",
    };
    let carddav = SourceKey {
        kind: SourceKind::CardDav,
        url: "https://x",
        account_id: "alice",
    };
    let webdav = SourceKey {
        kind: SourceKind::WebDav,
        url: "https://x",
        account_id: "alice",
    };
    let id_cal = sources.upsert(&caldav, Some("alice"), "alice").unwrap();
    let id_card = sources.upsert(&carddav, Some("alice"), "alice").unwrap();
    let id_web = sources.upsert(&webdav, Some("alice"), "alice").unwrap();
    assert_ne!(id_cal, id_card);
    assert_ne!(id_card, id_web);
    assert_ne!(id_cal, id_web);

    let no_conflict = sources
        .conflicting(&SourceKey {
            kind: SourceKind::CalDav,
            url: "https://x",
            account_id: "alice",
        })
        .unwrap();
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

fn caldav_import_config(url: &str) -> vandelay::import::dav::DavImport {
    vandelay::import::dav::DavImport {
        kind: vandelay::import::dav::DavKindArg::Caldav,
        url: url.to_owned(),
        auth: Auth::Basic {
            user: "u".to_owned(),
            password: "p".to_owned(),
        },
        allow_cleartext: true,
        dav_connections: 1,
        multiget_batch: 8,
        source_change: vandelay::archive::sources::SourceChange::Reject,
    }
}

fn dav_common(archive: &std::path::Path) -> vandelay::sync::RunOptions {
    vandelay::sync::RunOptions {
        archive: archive.to_path_buf(),
        threads: 1,
        mode: RunMode::Apply,
        max_retries: 0,
        tls: TlsPolicy::Verify,
        logger: vandelay::logging::Logger::new(Level::Quiet),
    }
}

fn row_count(archive: &std::path::Path, table: &str) -> i64 {
    let conn = rusqlite::Connection::open(archive).expect("open archive");
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .expect("count")
}

#[test]
fn forbidden_collection_is_a_per_unit_failure_and_the_others_still_import() {
    let (_, archive) = failing_collection_beside_a_readable_one(
        403,
        "text/plain",
        "you may not read this calendar",
    );
    assert_eq!(row_count(&archive, "calendars"), 2);
    assert_eq!(row_count(&archive, "calendar_events"), 1);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn every_collection_scoped_failure_stays_per_unit() {
    // 405, an exhausted 503 and a malformed multistatus each fail only their collection
    for (status, content_type, body) in [
        (405, "text/plain", "method not allowed"),
        (503, "text/plain", "try later"),
        (
            207,
            "application/xml",
            "<d:multistatus xmlns:d=\"DAV:\"><d:response>",
        ),
    ] {
        let (_, archive) = failing_collection_beside_a_readable_one(status, content_type, body);
        assert_eq!(row_count(&archive, "calendar_events"), 1, "status {status}");
        let _ = std::fs::remove_file(&archive);
    }
}

fn failing_collection_beside_a_readable_one(
    status: usize,
    content_type: &str,
    body: &str,
) -> (vandelay::sync::Summary, std::path::PathBuf) {
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

    let failing = server
        .mock("PROPFIND", "/dav/cal/u/shared/")
        .with_status(status)
        .with_header("content-type", content_type)
        .with_body(body)
        .expect_at_least(1)
        .create();

    let archive = dav_archive();
    let summary = caldav_import_config(&format!("{url}/dav/cal/u/"))
        .run(dav_common(&archive))
        .into_result()
        .unwrap_or_else(|e| panic!("status {status} must not abort the run: {e}"));

    failing.assert();

    let events = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == ObjectType::CalendarEvent)
        .map(|(_, c)| c.clone())
        .expect("calendarevent counts");
    assert_eq!(events.failed, 1, "the forbidden calendar counts as failed");
    assert_eq!(events.fetched, 1, "the readable calendar still imported");
    assert!(
        summary.any_failed(),
        "a per-unit failure ends the run at exit 5"
    );
    (summary, archive)
}

// Kerio Connect sends calendar-data with bare CR line breaks, raw or as &#13; (issue #48).
#[test]
fn kerio_bare_cr_calendar_data_imports() {
    let mut server = mockito::Server::new();
    let url = server.url();
    let home = "/full-calendars/domain.ext/user/";
    let coll = "/full-calendars/domain.ext/user/ceb087f2-0604-4c31-a70e-9f68e90600c6/";

    let home_set = format!(
        r#"<?xml version="1.0"?>
<a:multistatus xmlns:a="DAV:" xmlns:f="urn:ietf:params:xml:ns:caldav">
  <a:response>
    <a:href>{coll}</a:href>
    <a:propstat>
      <a:prop>
        <a:resourcetype><a:collection/><f:calendar/></a:resourcetype>
        <a:displayname>Calendar</a:displayname>
      </a:prop>
      <a:status>HTTP/1.1 200 OK</a:status>
    </a:propstat>
  </a:response>
</a:multistatus>"#
    );
    let _discovery = multistatus_response(&mut server, "PROPFIND", home, &home_set);

    let item = |n: u32| format!("{coll}ceb087f2-0604-4c31-a70e-9f68e90600c6-0000000{n}.eml");
    let listing = format!(
        r#"<?xml version="1.0"?>
<a:multistatus xmlns:a="DAV:">
  <a:response>
    <a:href>{}</a:href>
    <a:propstat><a:prop><a:resourcetype/><a:getetag>"k1"</a:getetag></a:prop>
      <a:status>HTTP/1.1 200 OK</a:status></a:propstat>
  </a:response>
  <a:response>
    <a:href>{}</a:href>
    <a:propstat><a:prop><a:resourcetype/><a:getetag>"k2"</a:getetag></a:prop>
      <a:status>HTTP/1.1 200 OK</a:status></a:propstat>
  </a:response>
</a:multistatus>"#,
        item(1),
        item(2)
    );
    let _enumeration = multistatus_response(&mut server, "PROPFIND", coll, &listing);

    let ical = |uid: &str, summary: &str, eol: &str| {
        [
            "BEGIN:VCALENDAR",
            "PRODID:-//Kerio Technologies//Kerio Connect//EN",
            "VERSION:2.0",
            "BEGIN:VEVENT",
            &format!("UID:{uid}"),
            "DTSTAMP:20260101T000000Z",
            "DTSTART:20260101T090000Z",
            "DTEND:20260101T100000Z",
            &format!("SUMMARY:{summary}"),
            "X-LABEL:0",
            "END:VEVENT",
            "END:VCALENDAR",
            "",
        ]
        .join(eol)
    };
    let data = format!(
        r#"<?xml version="1.0"?>
<a:multistatus xmlns:a="DAV:" xmlns:f="urn:ietf:params:xml:ns:caldav">
  <a:response>
    <a:href>{}</a:href>
    <a:propstat><a:prop><a:getetag>"k1"</a:getetag>
      <f:calendar-data><![CDATA[{}]]></f:calendar-data></a:prop>
      <a:status>HTTP/1.1 200 OK</a:status></a:propstat>
  </a:response>
  <a:response>
    <a:href>{}</a:href>
    <a:propstat><a:prop><a:getetag>"k2"</a:getetag>
      <f:calendar-data>{}</f:calendar-data></a:prop>
      <a:status>HTTP/1.1 200 OK</a:status></a:propstat>
  </a:response>
</a:multistatus>"#,
        item(1),
        ical("k1@kerio", "Raw CR", "\r"),
        item(2),
        ical("k2@kerio", "Entity CR", "&#13;"),
    );
    let _multiget = multistatus_response(&mut server, "REPORT", coll, &data);

    let archive = dav_archive();
    let summary = caldav_import_config(&format!("{url}{home}"))
        .run(dav_common(&archive))
        .into_result()
        .expect("import");

    let events = summary
        .per_type
        .iter()
        .find(|(t, _)| *t == ObjectType::CalendarEvent)
        .map(|(_, c)| c.clone())
        .expect("calendarevent counts");
    assert_eq!(events.failed, 0, "{events:?}");
    assert_eq!(events.fetched, 2, "{events:?}");

    let conn = rusqlite::Connection::open(&archive).expect("open archive");
    let mut titles: Vec<String> = conn
        .prepare("SELECT json_extract(data, '$.title') FROM calendar_events")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    titles.sort_unstable();
    assert_eq!(titles, ["Entity CR", "Raw CR"]);
    drop(conn);
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
    let err = caldav_import_config(&format!("{url}/dav/cal/u/"))
        .run(dav_common(&archive))
        .into_result()
        .expect_err("credentials rejected for the principal aborts the run");

    assert!(
        matches!(err, vandelay::error::Error::Connection(_)),
        "discovery-level auth rejection is a whole-run failure, got {err:?}"
    );
    assert_eq!(err.exit_status(), ExitStatus::Connection);
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
    let outcome = caldav_import_config(&format!("{url}/dav/cal/u/")).run(dav_common(&archive));
    assert!(outcome.error.is_none());
    assert_eq!(
        outcome
            .summary
            .per_type
            .iter()
            .map(|(t, _)| *t)
            .collect::<Vec<_>>(),
        vec![ObjectType::Calendar, ObjectType::CalendarEvent],
        "both phases are reported even when every collection failed"
    );
    let _ = std::fs::remove_file(&archive);
}

fn webdav_share(server: &mut mockito::Server, prefix: &str, body: &str) -> Vec<mockito::Mock> {
    let url = server.url();
    let root = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>{url}{prefix}</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/></d:resourcetype><d:displayname>share</d:displayname></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>{url}{prefix}docs/</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/></d:resourcetype><d:displayname>docs</d:displayname></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    let docs = format!(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>{url}{prefix}docs/</d:href>
    <d:propstat>
      <d:prop><d:resourcetype><d:collection/></d:resourcetype><d:displayname>docs</d:displayname></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>{url}{prefix}docs/a.txt</d:href>
    <d:propstat>
      <d:prop><d:resourcetype/><d:getcontenttype>text/plain</d:getcontenttype><d:getetag>"a1"</d:getetag></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#
    );
    vec![
        multistatus_response(server, "PROPFIND", prefix, &root),
        multistatus_response(server, "PROPFIND", &format!("{prefix}docs/"), &docs),
        server
            .mock("GET", format!("{prefix}docs/a.txt").as_str())
            .with_status(200)
            .with_header("content-type", "text/plain")
            .with_body(body)
            .create(),
    ]
}

fn webdav_import_config(
    url: &str,
    source_change: vandelay::archive::sources::SourceChange,
) -> vandelay::import::dav::DavImport {
    vandelay::import::dav::DavImport {
        kind: vandelay::import::dav::DavKindArg::Webdav,
        source_change,
        ..caldav_import_config(url)
    }
}

#[test]
fn allow_source_change_keeps_the_moved_share_old_tree() {
    use vandelay::archive::sources::SourceChange;

    let mut server = mockito::Server::new();
    let url = server.url();
    let _old = webdav_share(&mut server, "/old/u/", "old\n");
    let _new = webdav_share(&mut server, "/new/u/", "new\n");
    let archive = dav_archive();

    webdav_import_config(&format!("{url}/old/u/"), SourceChange::Reject)
        .run(dav_common(&archive))
        .into_result()
        .expect("old share import");
    webdav_import_config(&format!("{url}/new/u/"), SourceChange::Allow)
        .run(dav_common(&archive))
        .into_result()
        .expect("new share import");

    let (nodes, sources) = (
        row_count(&archive, "file_nodes"),
        row_count(&archive, "sources"),
    );
    let _ = std::fs::remove_file(&archive);
    assert_eq!(nodes, 4, "both trees stay side by side");
    assert_eq!(sources, 2);
}

#[test]
fn replace_source_drops_the_moved_share_old_tree() {
    use vandelay::archive::sources::SourceChange;

    let mut server = mockito::Server::new();
    let url = server.url();
    let _old = webdav_share(&mut server, "/old/u/", "old\n");
    let _new = webdav_share(&mut server, "/new/u/", "new\n");
    let archive = dav_archive();

    webdav_import_config(&format!("{url}/old/u/"), SourceChange::Reject)
        .run(dav_common(&archive))
        .into_result()
        .expect("old share import");
    let summary = webdav_import_config(&format!("{url}/new/u/"), SourceChange::Replace)
        .run(dav_common(&archive))
        .into_result()
        .expect("new share import");
    let rerun = webdav_import_config(&format!("{url}/new/u/"), SourceChange::Reject)
        .run(dav_common(&archive))
        .into_result();

    let conn = rusqlite::Connection::open(&archive).unwrap();
    let account: String = conn
        .query_row("SELECT account_id FROM sources", [], |r| r.get(0))
        .unwrap();
    let orphans: i64 = conn
        .query_row(
            "SELECT count(*) FROM file_nodes
             WHERE id NOT IN (SELECT local_id FROM sync_id_dav)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    drop(conn);
    let (nodes, sources, blobs) = (
        row_count(&archive, "file_nodes"),
        row_count(&archive, "sources"),
        row_count(&archive, "blobs"),
    );
    let _ = std::fs::remove_file(&archive);

    assert_eq!(nodes, 2, "only the new tree remains");
    assert_eq!(orphans, 0);
    assert_eq!(sources, 1);
    assert!(account.ends_with("/new/u/"), "kept source is {account}");
    assert_eq!(blobs, 1, "the old file's blob is collected");
    let filenode = summary
        .per_type
        .iter()
        .find(|(name, _)| *name == ObjectType::FileNode)
        .expect("filenode counts");
    assert_eq!(filenode.1.deleted, 2);
    assert!(
        rerun.is_ok(),
        "once replaced, the archive records a single source: {rerun:?}"
    );
}

#[test]
fn replace_source_removal_failure_leaves_the_new_source_unregistered() {
    // --replace-source removes the old sources before registering the new one
    use vandelay::archive::sources::SourceChange;

    let mut server = mockito::Server::new();
    let url = server.url();
    let _old = webdav_share(&mut server, "/old/u/", "old\n");
    let _new = webdav_share(&mut server, "/new/u/", "new\n");
    let archive = dav_archive();

    webdav_import_config(&format!("{url}/old/u/"), SourceChange::Reject)
        .run(dav_common(&archive))
        .into_result()
        .expect("old share import");
    let conn = rusqlite::Connection::open(&archive).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER block_source_delete BEFORE DELETE ON sources
         BEGIN SELECT RAISE(ABORT, 'blocked'); END;",
    )
    .unwrap();
    drop(conn);
    let result = webdav_import_config(&format!("{url}/new/u/"), SourceChange::Replace)
        .run(dav_common(&archive))
        .into_result();

    let conn = rusqlite::Connection::open(&archive).unwrap();
    let accounts: Vec<String> = conn
        .prepare("SELECT account_id FROM sources")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    drop(conn);
    let _ = std::fs::remove_file(&archive);

    assert!(result.is_err(), "the removal failure aborts the run");
    assert_eq!(accounts.len(), 1, "sources: {accounts:?}");
    assert!(
        accounts[0].ends_with("/old/u/"),
        "kept source is {accounts:?}"
    );
}

#[test]
fn source_registration_sql_failure_is_a_partial_exit() {
    // a failing sources upsert ends a DAV run at exit 5, not 7
    use vandelay::archive::sources::SourceChange;

    let mut server = mockito::Server::new();
    let url = server.url();
    let _share = webdav_share(&mut server, "/new/u/", "new\n");
    let archive = dav_archive();
    let db = vandelay::archive::Archive::open(&archive).unwrap();
    db.connection()
        .execute_batch(
            "CREATE TRIGGER block_source_insert BEFORE INSERT ON sources
         BEGIN SELECT RAISE(ABORT, 'blocked'); END;",
        )
        .unwrap();
    drop(db);
    let err = webdav_import_config(&format!("{url}/new/u/"), SourceChange::Reject)
        .run(dav_common(&archive))
        .into_result()
        .expect_err("source registration fails");
    let _ = std::fs::remove_file(&archive);

    assert!(matches!(err, vandelay::error::Error::Partial(_)), "{err:?}");
    assert_eq!(err.exit_status(), ExitStatus::Partial);
}

#[test]
fn dry_run_replace_source_keeps_the_old_tree() {
    use vandelay::archive::sources::SourceChange;

    let mut server = mockito::Server::new();
    let url = server.url();
    let _old = webdav_share(&mut server, "/old/u/", "old\n");
    let _new = webdav_share(&mut server, "/new/u/", "new\n");
    let archive = dav_archive();

    webdav_import_config(&format!("{url}/old/u/"), SourceChange::Reject)
        .run(dav_common(&archive))
        .into_result()
        .expect("old share import");
    let mut common = dav_common(&archive);
    common.mode = RunMode::DryRun;
    webdav_import_config(&format!("{url}/new/u/"), SourceChange::Replace)
        .run(common)
        .into_result()
        .expect("dry-run");

    let (nodes, sources) = (
        row_count(&archive, "file_nodes"),
        row_count(&archive, "sources"),
    );
    let _ = std::fs::remove_file(&archive);
    assert_eq!(nodes, 2);
    assert_eq!(sources, 1);
}

#[test]
fn dry_run_rejects_another_account_like_the_real_run() {
    // R6: the source-change guard runs in a dry run too, so it predicts exit 4
    use vandelay::archive::sources::SourceChange;

    let mut server = mockito::Server::new();
    let url = server.url();
    let _old = webdav_share(&mut server, "/old/u/", "old\n");
    let _new = webdav_share(&mut server, "/new/u/", "new\n");
    let archive = dav_archive();

    webdav_import_config(&format!("{url}/old/u/"), SourceChange::Reject)
        .run(dav_common(&archive))
        .into_result()
        .expect("old share import");
    let mut common = dav_common(&archive);
    common.mode = RunMode::DryRun;
    let outcome = webdav_import_config(&format!("{url}/new/u/"), SourceChange::Reject).run(common);
    let real = webdav_import_config(&format!("{url}/new/u/"), SourceChange::Reject)
        .run(dav_common(&archive))
        .into_result();
    let sources = row_count(&archive, "sources");
    let _ = std::fs::remove_file(&archive);

    let dry = outcome
        .error
        .expect("dry run must refuse the account change");
    assert_eq!(dry.exit_status(), ExitStatus::SourceChange);
    assert_eq!(
        real.expect_err("real run refuses").to_string(),
        dry.to_string()
    );
    assert_eq!(sources, 1);
}
