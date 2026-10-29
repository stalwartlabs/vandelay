/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::io::BufRead;

use quick_xml::NsReader;
use quick_xml::events::{BytesRef, Event};
use quick_xml::name::ResolveResult;

use crate::dav::href::{Href, normalise};
use crate::dav::xml::{NS_APPLE_ICAL, NS_CALDAV, NS_CARDDAV, NS_DAV};

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("xml error: {0}")]
    Xml(String),
    #[error("invalid href {href:?}: {source}")]
    Href {
        href: String,
        #[source]
        source: crate::dav::href::HrefError,
    },
    #[error("missing href in response")]
    MissingHref,
}

impl From<quick_xml::Error> for ParseError {
    fn from(value: quick_xml::Error) -> Self {
        ParseError::Xml(value.to_string())
    }
}

#[derive(Debug, Clone, Default)]
pub struct ResourceProps {
    pub is_collection: bool,
    pub is_calendar: bool,
    pub is_addressbook: bool,
    pub displayname: Option<String>,
    pub current_user_principal: Option<String>,
    pub calendar_home_set: Option<String>,
    pub addressbook_home_set: Option<String>,
    pub etag: Option<String>,
    pub content_type: Option<String>,
    pub last_modified: Option<String>,
    pub creation_date: Option<String>,
    pub content_length: Option<u64>,
    pub calendar_description: Option<String>,
    pub addressbook_description: Option<String>,
    pub calendar_color: Option<String>,
    pub calendar_order: Option<i64>,
    pub calendar_timezone: Option<String>,
    pub supported_components: Vec<String>,
    pub calendar_data: Option<String>,
    pub address_data: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DavResponse {
    pub href: Href,
    pub status: Option<u16>,
    pub props: ResourceProps,
    pub propstat_errors: Vec<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NsKind {
    Dav,
    Caldav,
    Carddav,
    Apple,
    Other,
}

fn classify_ns(ns: &ResolveResult<'_>) -> NsKind {
    match ns {
        ResolveResult::Bound(prefix) => {
            let p = prefix.as_ref();
            if p == NS_DAV {
                NsKind::Dav
            } else if p == NS_CALDAV {
                NsKind::Caldav
            } else if p == NS_CARDDAV {
                NsKind::Carddav
            } else if p == NS_APPLE_ICAL {
                NsKind::Apple
            } else {
                NsKind::Other
            }
        }
        _ => NsKind::Other,
    }
}

fn is(ns: NsKind, local: &str, expected_ns: NsKind, target: &str) -> bool {
    ns == expected_ns && local.eq_ignore_ascii_case(target)
}

#[derive(Debug)]
enum Step {
    StartElement {
        ns: NsKind,
        local: String,
        attrs: Vec<(String, String)>,
    },
    EmptyElement {
        ns: NsKind,
        local: String,
        attrs: Vec<(String, String)>,
    },
    EndElement {
        ns: NsKind,
        local: String,
    },
    Text(String),
    CData(String),
    Entity(char),
    Other,
    Eof,
}

fn resolve_entity(g: &BytesRef) -> Option<char> {
    if let Ok(Some(c)) = g.resolve_char_ref() {
        return Some(c);
    }
    match g.as_ref() {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => None,
    }
}

fn next_step<R: BufRead>(xml: &mut NsReader<R>, buf: &mut Vec<u8>) -> Result<Step, ParseError> {
    buf.clear();
    let (ns, ev) = xml.read_resolved_event_into(buf)?;
    let ns_kind = classify_ns(&ns);
    Ok(match ev {
        Event::Start(e) => {
            let local = e.local_name().as_ref().to_owned();
            let attrs = e
                .attributes()
                .flatten()
                .map(|a| (a.key.as_ref().to_owned(), a.value.into_owned()))
                .collect();
            Step::StartElement {
                ns: ns_kind,
                local,
                attrs,
            }
        }
        Event::Empty(e) => {
            let local = e.local_name().as_ref().to_owned();
            let attrs = e
                .attributes()
                .flatten()
                .map(|a| (a.key.as_ref().to_owned(), a.value.into_owned()))
                .collect();
            Step::EmptyElement {
                ns: ns_kind,
                local,
                attrs,
            }
        }
        Event::End(e) => Step::EndElement {
            ns: ns_kind,
            local: e.local_name().as_ref().to_owned(),
        },
        Event::Text(t) => Step::Text(t.into_inner().into_owned()),
        Event::CData(cd) => Step::CData(cd.into_inner().into_owned()),
        Event::GeneralRef(g) => match resolve_entity(&g) {
            Some(c) => Step::Entity(c),
            None => Step::Other,
        },
        Event::Eof => Step::Eof,
        _ => Step::Other,
    })
}

pub fn parse_multistatus<R: BufRead>(
    reader: R,
    base_url: &str,
) -> Result<Vec<DavResponse>, ParseError> {
    let mut xml = NsReader::from_reader(reader);
    let mut buf = Vec::new();
    let mut out: Vec<DavResponse> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    loop {
        let step = next_step(&mut xml, &mut buf)?;
        match step {
            Step::StartElement { ns, local, .. } if is(ns, &local, NsKind::Dav, "response") => {
                if let Some(r) = parse_response(&mut xml, base_url)?
                    && seen.insert(r.href.as_str().to_owned())
                {
                    out.push(r);
                }
            }
            Step::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

fn parse_response<R: BufRead>(
    xml: &mut NsReader<R>,
    base_url: &str,
) -> Result<Option<DavResponse>, ParseError> {
    let mut buf = Vec::new();
    let mut href: Option<String> = None;
    let mut status: Option<u16> = None;
    let mut props = ResourceProps::default();
    let mut propstat_errors: Vec<u16> = Vec::new();

    loop {
        let step = next_step(xml, &mut buf)?;
        match step {
            Step::StartElement { ns, local, .. } => {
                if is(ns, &local, NsKind::Dav, "href") {
                    let text = read_token(xml)?;
                    if href.is_none() {
                        href = Some(text);
                    }
                } else if is(ns, &local, NsKind::Dav, "status") {
                    let text = read_token(xml)?;
                    status = parse_http_status(&text);
                } else if is(ns, &local, NsKind::Dav, "propstat") {
                    let (block_status, block_props) = parse_propstat(xml)?;
                    match block_status {
                        Some(s) if (200..300).contains(&s) => merge_props(&mut props, block_props),
                        Some(s) => propstat_errors.push(s),
                        None => {}
                    }
                } else {
                    skip_element(xml, &local)?;
                }
            }
            Step::EndElement { ns, local } if is(ns, &local, NsKind::Dav, "response") => break,
            Step::Eof => return Err(ParseError::Xml("unexpected EOF in <response>".into())),
            _ => {}
        }
    }

    let raw_href = href.ok_or(ParseError::MissingHref)?;
    let normalised = normalise(base_url, &raw_href).map_err(|e| ParseError::Href {
        href: raw_href,
        source: e,
    })?;
    Ok(Some(DavResponse {
        href: normalised,
        status,
        props,
        propstat_errors,
    }))
}

fn parse_propstat<R: BufRead>(
    xml: &mut NsReader<R>,
) -> Result<(Option<u16>, ResourceProps), ParseError> {
    let mut buf = Vec::new();
    let mut status: Option<u16> = None;
    let mut props = ResourceProps::default();
    loop {
        let step = next_step(xml, &mut buf)?;
        match step {
            Step::StartElement { ns, local, .. } => {
                if is(ns, &local, NsKind::Dav, "status") {
                    let text = read_token(xml)?;
                    status = parse_http_status(&text);
                } else if is(ns, &local, NsKind::Dav, "prop") {
                    parse_prop_block(xml, &mut props)?;
                } else {
                    skip_element(xml, &local)?;
                }
            }
            Step::EndElement { ns, local } if is(ns, &local, NsKind::Dav, "propstat") => break,
            Step::Eof => return Err(ParseError::Xml("unexpected EOF in <propstat>".into())),
            _ => {}
        }
    }
    Ok((status, props))
}

fn parse_prop_block<R: BufRead>(
    xml: &mut NsReader<R>,
    props: &mut ResourceProps,
) -> Result<(), ParseError> {
    let mut buf = Vec::new();
    loop {
        let step = next_step(xml, &mut buf)?;
        match step {
            Step::StartElement { ns, local, .. } => {
                handle_prop_element(xml, props, ns, &local)?;
            }
            Step::EmptyElement { .. } => {}
            Step::EndElement { ns, local } if is(ns, &local, NsKind::Dav, "prop") => break,
            Step::Eof => return Err(ParseError::Xml("unexpected EOF in <prop>".into())),
            _ => {}
        }
    }
    Ok(())
}

fn handle_prop_element<R: BufRead>(
    xml: &mut NsReader<R>,
    props: &mut ResourceProps,
    ns: NsKind,
    local: &str,
) -> Result<(), ParseError> {
    if is(ns, local, NsKind::Dav, "resourcetype") {
        consume_resourcetype(xml, props)?;
    } else if is(ns, local, NsKind::Dav, "displayname") {
        props.displayname = Some(read_token(xml)?);
    } else if is(ns, local, NsKind::Dav, "current-user-principal") {
        props.current_user_principal = read_first_href(xml, "current-user-principal")?;
    } else if is(ns, local, NsKind::Caldav, "calendar-home-set") {
        props.calendar_home_set = read_first_href(xml, "calendar-home-set")?;
    } else if is(ns, local, NsKind::Carddav, "addressbook-home-set") {
        props.addressbook_home_set = read_first_href(xml, "addressbook-home-set")?;
    } else if is(ns, local, NsKind::Dav, "getetag") {
        let raw = read_token(xml)?;
        if !raw.is_empty() {
            props.etag = Some(raw);
        }
    } else if is(ns, local, NsKind::Dav, "getcontenttype") {
        props.content_type = Some(read_token(xml)?);
    } else if is(ns, local, NsKind::Dav, "getlastmodified") {
        props.last_modified = Some(read_token(xml)?);
    } else if is(ns, local, NsKind::Dav, "creationdate") {
        props.creation_date = Some(read_token(xml)?);
    } else if is(ns, local, NsKind::Dav, "getcontentlength") {
        let raw = read_token(xml)?;
        props.content_length = raw.parse::<u64>().ok();
    } else if is(ns, local, NsKind::Caldav, "calendar-description") {
        props.calendar_description = Some(read_token(xml)?);
    } else if is(ns, local, NsKind::Carddav, "addressbook-description") {
        props.addressbook_description = Some(read_token(xml)?);
    } else if is(ns, local, NsKind::Apple, "calendar-color") {
        props.calendar_color = Some(read_token(xml)?);
    } else if is(ns, local, NsKind::Apple, "calendar-order") {
        let raw = read_token(xml)?;
        props.calendar_order = raw.parse::<i64>().ok();
    } else if is(ns, local, NsKind::Caldav, "calendar-timezone") {
        props.calendar_timezone = Some(read_token(xml)?);
    } else if is(
        ns,
        local,
        NsKind::Caldav,
        "supported-calendar-component-set",
    ) {
        consume_supported_components(xml, props)?;
    } else if is(ns, local, NsKind::Caldav, "calendar-data") {
        props.calendar_data = Some(read_text(xml)?);
    } else if is(ns, local, NsKind::Carddav, "address-data") {
        props.address_data = Some(read_text(xml)?);
    } else {
        skip_element(xml, local)?;
    }
    Ok(())
}

fn consume_resourcetype<R: BufRead>(
    xml: &mut NsReader<R>,
    props: &mut ResourceProps,
) -> Result<(), ParseError> {
    let mut buf = Vec::new();
    loop {
        let step = next_step(xml, &mut buf)?;
        match step {
            Step::StartElement { ns, local, .. } => {
                mark_resourcetype(ns, &local, props);
                skip_element(xml, &local)?;
            }
            Step::EmptyElement { ns, local, .. } => {
                mark_resourcetype(ns, &local, props);
            }
            Step::EndElement { ns, local } if is(ns, &local, NsKind::Dav, "resourcetype") => break,
            Step::Eof => {
                return Err(ParseError::Xml("unexpected EOF in <resourcetype>".into()));
            }
            _ => {}
        }
    }
    Ok(())
}

fn mark_resourcetype(ns: NsKind, local: &str, props: &mut ResourceProps) {
    if is(ns, local, NsKind::Dav, "collection") {
        props.is_collection = true;
    } else if is(ns, local, NsKind::Caldav, "calendar") {
        props.is_calendar = true;
    } else if is(ns, local, NsKind::Carddav, "addressbook") {
        props.is_addressbook = true;
    }
}

fn consume_supported_components<R: BufRead>(
    xml: &mut NsReader<R>,
    props: &mut ResourceProps,
) -> Result<(), ParseError> {
    let mut buf = Vec::new();
    loop {
        let step = next_step(xml, &mut buf)?;
        match step {
            Step::StartElement {
                ns,
                local,
                ref attrs,
            } if is(ns, &local, NsKind::Caldav, "comp") => {
                if let Some(name) = attr_lookup(attrs, "name") {
                    props.supported_components.push(name);
                }
                skip_element(xml, &local)?;
            }
            Step::EmptyElement {
                ns,
                local,
                ref attrs,
            } if is(ns, &local, NsKind::Caldav, "comp") => {
                if let Some(name) = attr_lookup(attrs, "name") {
                    props.supported_components.push(name);
                }
            }
            Step::EndElement { ns, local }
                if is(
                    ns,
                    &local,
                    NsKind::Caldav,
                    "supported-calendar-component-set",
                ) =>
            {
                break;
            }
            Step::Eof => {
                return Err(ParseError::Xml(
                    "unexpected EOF in <supported-calendar-component-set>".into(),
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn attr_lookup(attrs: &[(String, String)], key: &str) -> Option<String> {
    attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

fn read_text<R: BufRead>(xml: &mut NsReader<R>) -> Result<String, ParseError> {
    let mut buf = Vec::new();
    let mut out = String::new();
    loop {
        let step = next_step(xml, &mut buf)?;
        match step {
            Step::Text(s) | Step::CData(s) => out.push_str(&s),
            Step::Entity(c) => out.push(c),
            Step::StartElement { local, .. } => skip_element(xml, &local)?,
            Step::EndElement { .. } | Step::EmptyElement { .. } => break,
            Step::Eof => return Err(ParseError::Xml("unexpected EOF reading text".into())),
            _ => {}
        }
    }
    Ok(out)
}

fn read_token<R: BufRead>(xml: &mut NsReader<R>) -> Result<String, ParseError> {
    Ok(read_text(xml)?.trim().to_owned())
}

fn read_first_href<R: BufRead>(
    xml: &mut NsReader<R>,
    closing_local: &str,
) -> Result<Option<String>, ParseError> {
    let mut buf = Vec::new();
    let mut out: Option<String> = None;
    loop {
        let step = next_step(xml, &mut buf)?;
        match step {
            Step::StartElement { ns, local, .. } => {
                if out.is_none() && is(ns, &local, NsKind::Dav, "href") {
                    out = Some(read_token(xml)?);
                } else {
                    skip_element(xml, &local)?;
                }
            }
            Step::EmptyElement { .. } => {}
            Step::EndElement { local, .. } if local.eq_ignore_ascii_case(closing_local) => {
                break;
            }
            Step::Eof => return Err(ParseError::Xml("unexpected EOF reading href".into())),
            _ => {}
        }
    }
    Ok(out)
}

fn skip_element<R: BufRead>(xml: &mut NsReader<R>, target: &str) -> Result<(), ParseError> {
    let mut buf = Vec::new();
    let mut depth: i32 = 1;
    while depth > 0 {
        let step = next_step(xml, &mut buf)?;
        match step {
            Step::StartElement { .. } => depth += 1,
            Step::EndElement { local, .. } => {
                depth -= 1;
                if depth == 0 && !local.eq_ignore_ascii_case(target) {
                    return Ok(());
                }
            }
            Step::Eof => return Err(ParseError::Xml("unexpected EOF in skip".into())),
            _ => {}
        }
    }
    Ok(())
}

fn merge_props(target: &mut ResourceProps, src: ResourceProps) {
    if src.is_collection {
        target.is_collection = true;
    }
    if src.is_calendar {
        target.is_calendar = true;
    }
    if src.is_addressbook {
        target.is_addressbook = true;
    }
    overwrite_some(&mut target.displayname, src.displayname);
    overwrite_some(
        &mut target.current_user_principal,
        src.current_user_principal,
    );
    overwrite_some(&mut target.calendar_home_set, src.calendar_home_set);
    overwrite_some(&mut target.addressbook_home_set, src.addressbook_home_set);
    overwrite_some(&mut target.etag, src.etag);
    overwrite_some(&mut target.content_type, src.content_type);
    overwrite_some(&mut target.last_modified, src.last_modified);
    overwrite_some(&mut target.creation_date, src.creation_date);
    overwrite_some(&mut target.content_length, src.content_length);
    overwrite_some(&mut target.calendar_description, src.calendar_description);
    overwrite_some(
        &mut target.addressbook_description,
        src.addressbook_description,
    );
    overwrite_some(&mut target.calendar_color, src.calendar_color);
    overwrite_some(&mut target.calendar_order, src.calendar_order);
    overwrite_some(&mut target.calendar_timezone, src.calendar_timezone);
    overwrite_some(&mut target.calendar_data, src.calendar_data);
    overwrite_some(&mut target.address_data, src.address_data);
    if !src.supported_components.is_empty() {
        target.supported_components = src.supported_components;
    }
}

fn overwrite_some<T>(target: &mut Option<T>, src: Option<T>) {
    if src.is_some() {
        *target = src;
    }
}

fn parse_http_status(text: &str) -> Option<u16> {
    let mut parts = text.split_ascii_whitespace();
    let _version = parts.next()?;
    let code = parts.next()?;
    code.parse::<u16>().ok()
}

pub fn strip_ascii_control_chars(body: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    if body.iter().any(|&b| is_xml_illegal_control(b)) {
        let cleaned: Vec<u8> = body
            .iter()
            .copied()
            .filter(|b| !is_xml_illegal_control(*b))
            .collect();
        std::borrow::Cow::Owned(cleaned)
    } else {
        std::borrow::Cow::Borrowed(body)
    }
}

pub fn is_xml_illegal_control(b: u8) -> bool {
    matches!(b, 0x00..=0x08 | 0x0b | 0x0c | 0x0e..=0x1f)
}

pub struct ControlStrippingReader<R: std::io::Read> {
    inner: R,
}

impl<R: std::io::Read> ControlStrippingReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }
}

impl<R: std::io::Read> std::io::Read for ControlStrippingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let n = self.inner.read(buf)?;
            if n == 0 {
                return Ok(0);
            }
            let mut write = 0;
            for read in 0..n {
                let b = buf[read];
                if !is_xml_illegal_control(b) {
                    buf[write] = b;
                    write += 1;
                }
            }
            if write > 0 {
                return Ok(write);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse(body: &str) -> Vec<DavResponse> {
        parse_multistatus(Cursor::new(body), "https://x/").expect("parse")
    }

    #[test]
    fn parses_simple_principal_response() {
        let body = r#"<?xml version="1.0"?>
        <d:multistatus xmlns:d="DAV:">
          <d:response>
            <d:href>/dav/</d:href>
            <d:propstat>
              <d:prop>
                <d:current-user-principal>
                  <d:href>/dav/principals/alice/</d:href>
                </d:current-user-principal>
              </d:prop>
              <d:status>HTTP/1.1 200 OK</d:status>
            </d:propstat>
          </d:response>
        </d:multistatus>"#;
        let r = parse(body);
        assert_eq!(r.len(), 1);
        assert_eq!(
            r[0].props.current_user_principal.as_deref(),
            Some("/dav/principals/alice/")
        );
    }

    #[test]
    fn parses_calendar_collection_with_apple_color_and_order() {
        let body = r#"<?xml version="1.0"?>
        <multistatus xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:ic="http://apple.com/ns/ical/">
          <response>
            <href>/dav/cal/u/default/</href>
            <propstat>
              <prop>
                <resourcetype><collection/><c:calendar/></resourcetype>
                <displayname>Default</displayname>
                <ic:calendar-color>#3366cc</ic:calendar-color>
                <ic:calendar-order>5</ic:calendar-order>
                <c:calendar-description>Work</c:calendar-description>
              </prop>
              <status>HTTP/1.1 200 OK</status>
            </propstat>
          </response>
        </multistatus>"#;
        let r = parse(body);
        assert_eq!(r.len(), 1);
        assert!(r[0].props.is_collection);
        assert!(r[0].props.is_calendar);
        assert_eq!(r[0].props.displayname.as_deref(), Some("Default"));
        assert_eq!(r[0].props.calendar_color.as_deref(), Some("#3366cc"));
        assert_eq!(r[0].props.calendar_order, Some(5));
        assert_eq!(r[0].props.calendar_description.as_deref(), Some("Work"));
    }

    #[test]
    fn parses_item_etag_and_content_type() {
        let body = r#"<?xml version="1.0"?>
        <d:multistatus xmlns:d="DAV:">
          <d:response>
            <d:href>/dav/cal/u/d/event1.ics</d:href>
            <d:propstat>
              <d:prop>
                <d:resourcetype/>
                <d:getetag>"abc123"</d:getetag>
                <d:getcontenttype>text/calendar; charset=utf-8</d:getcontenttype>
              </d:prop>
              <d:status>HTTP/1.1 200 OK</d:status>
            </d:propstat>
          </d:response>
        </d:multistatus>"#;
        let r = parse(body);
        assert_eq!(r[0].props.etag.as_deref(), Some("\"abc123\""));
        assert!(!r[0].props.is_collection);
        assert!(
            r[0].props
                .content_type
                .as_deref()
                .unwrap()
                .starts_with("text/calendar")
        );
    }

    #[test]
    fn parses_calendar_multiget_response_with_data() {
        let body = r#"<?xml version="1.0"?>
        <d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
          <d:response>
            <d:href>/dav/cal/u/d/event1.ics</d:href>
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
        let r = parse(body);
        assert_eq!(r[0].props.etag.as_deref(), Some("\"v2\""));
        let data = r[0].props.calendar_data.as_deref().unwrap();
        assert!(data.contains("VCALENDAR"));
    }

    #[test]
    fn drops_duplicate_response_hrefs() {
        let body = r#"<?xml version="1.0"?>
        <d:multistatus xmlns:d="DAV:">
          <d:response>
            <d:href>/dav/cal/u/d/event.ics</d:href>
            <d:propstat>
              <d:prop><d:getetag>"a"</d:getetag></d:prop>
              <d:status>HTTP/1.1 200 OK</d:status>
            </d:propstat>
          </d:response>
          <d:response>
            <d:href>/dav/cal/u/d/event.ics</d:href>
            <d:propstat>
              <d:prop><d:getetag>"a"</d:getetag></d:prop>
              <d:status>HTTP/1.1 200 OK</d:status>
            </d:propstat>
          </d:response>
        </d:multistatus>"#;
        let r = parse(body);
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn captures_response_level_status_for_vanished_items() {
        let body = r#"<?xml version="1.0"?>
        <d:multistatus xmlns:d="DAV:">
          <d:response>
            <d:href>/dav/cal/u/d/gone.ics</d:href>
            <d:status>HTTP/1.1 404 Not Found</d:status>
          </d:response>
        </d:multistatus>"#;
        let r = parse(body);
        assert_eq!(r[0].status, Some(404));
    }

    #[test]
    fn collects_propstat_errors_separately() {
        let body = r#"<?xml version="1.0"?>
        <d:multistatus xmlns:d="DAV:">
          <d:response>
            <d:href>/dav/cal/u/d/event.ics</d:href>
            <d:propstat>
              <d:prop><d:getetag>"a"</d:getetag></d:prop>
              <d:status>HTTP/1.1 200 OK</d:status>
            </d:propstat>
            <d:propstat>
              <d:prop><d:custom-prop/></d:prop>
              <d:status>HTTP/1.1 404 Not Found</d:status>
            </d:propstat>
          </d:response>
        </d:multistatus>"#;
        let r = parse(body);
        assert_eq!(r[0].propstat_errors, vec![404]);
        assert_eq!(r[0].props.etag.as_deref(), Some("\"a\""));
    }

    #[test]
    fn unknown_namespace_elements_ignored() {
        let body = r#"<?xml version="1.0"?>
        <d:multistatus xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/">
          <d:response>
            <d:href>/dav/cal/u/d/</d:href>
            <d:propstat>
              <d:prop>
                <d:resourcetype><d:collection/></d:resourcetype>
                <cs:getctag>abc</cs:getctag>
              </d:prop>
              <d:status>HTTP/1.1 200 OK</d:status>
            </d:propstat>
          </d:response>
        </d:multistatus>"#;
        let r = parse(body);
        assert!(r[0].props.is_collection);
    }

    #[test]
    fn strip_control_chars_removes_low_bytes() {
        let raw = b"<?xml version=\"1.0\"?>\x01<root/>";
        let cleaned = strip_ascii_control_chars(raw);
        assert!(!cleaned.contains(&0x01));
    }

    #[test]
    fn strip_control_chars_leaves_tab_newline_cr() {
        let raw = b"<?xml ?>\t\n\r";
        let cleaned = strip_ascii_control_chars(raw);
        match cleaned {
            std::borrow::Cow::Borrowed(_) => {}
            std::borrow::Cow::Owned(_) => panic!("should not have allocated"),
        }
    }

    #[test]
    fn parses_addressbook_collection() {
        let body = r#"<?xml version="1.0"?>
        <multistatus xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav">
          <response>
            <href>/dav/card/u/default/</href>
            <propstat>
              <prop>
                <resourcetype><collection/><c:addressbook/></resourcetype>
                <displayname>Contacts</displayname>
                <c:addressbook-description>Personal</c:addressbook-description>
              </prop>
              <status>HTTP/1.1 200 OK</status>
            </propstat>
          </response>
        </multistatus>"#;
        let r = parse(body);
        assert!(r[0].props.is_addressbook);
        assert_eq!(r[0].props.displayname.as_deref(), Some("Contacts"));
        assert_eq!(
            r[0].props.addressbook_description.as_deref(),
            Some("Personal")
        );
    }

    #[test]
    fn empty_multistatus_yields_no_responses() {
        let body = r#"<?xml version="1.0"?>
        <d:multistatus xmlns:d="DAV:"/>"#;
        let r = parse(body);
        assert!(r.is_empty());
    }

    #[test]
    fn supported_calendar_component_set_captures_empty_element_name_attr() {
        let body = r#"<?xml version="1.0"?>
        <d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
          <d:response>
            <d:href>/dav/cal/u/d/</d:href>
            <d:propstat>
              <d:prop>
                <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
                <c:supported-calendar-component-set>
                  <c:comp name="VEVENT"/>
                  <c:comp name="VTODO"/>
                </c:supported-calendar-component-set>
              </d:prop>
              <d:status>HTTP/1.1 200 OK</d:status>
            </d:propstat>
          </d:response>
        </d:multistatus>"#;
        let r = parse(body);
        assert_eq!(r[0].props.supported_components, vec!["VEVENT", "VTODO"]);
    }

    #[test]
    fn merge_props_last_propstat_wins_for_duplicate_keys() {
        let body = r#"<?xml version="1.0"?>
        <d:multistatus xmlns:d="DAV:">
          <d:response>
            <d:href>/dav/cal/u/d/</d:href>
            <d:propstat>
              <d:prop><d:displayname>First</d:displayname></d:prop>
              <d:status>HTTP/1.1 200 OK</d:status>
            </d:propstat>
            <d:propstat>
              <d:prop><d:displayname>Second</d:displayname></d:prop>
              <d:status>HTTP/1.1 200 OK</d:status>
            </d:propstat>
          </d:response>
        </d:multistatus>"#;
        let r = parse(body);
        assert_eq!(r[0].props.displayname.as_deref(), Some("Second"));
    }

    #[test]
    fn content_length_parses_to_u64() {
        let body = r#"<?xml version="1.0"?>
        <d:multistatus xmlns:d="DAV:">
          <d:response>
            <d:href>/file.bin</d:href>
            <d:propstat>
              <d:prop>
                <d:getcontentlength>123456</d:getcontentlength>
                <d:resourcetype/>
              </d:prop>
              <d:status>HTTP/1.1 200 OK</d:status>
            </d:propstat>
          </d:response>
        </d:multistatus>"#;
        let r = parse(body);
        assert_eq!(r[0].props.content_length, Some(123456));
    }
}
