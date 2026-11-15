/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::borrow::Cow;
use std::collections::HashSet;
use std::io::{self, BufRead, Read};

use quick_xml::NsReader;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;

use crate::dav::href::{Href, HrefError};
use crate::dav::retry::DavOutcome;
use crate::net::xml::Entity;

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("xml error: {0}")]
    Xml(String),
    #[error("invalid href {href:?}: {source}")]
    Href {
        href: String,
        #[source]
        source: HrefError,
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

impl ResourceProps {
    fn merge(&mut self, src: ResourceProps) {
        self.is_collection |= src.is_collection;
        self.is_calendar |= src.is_calendar;
        self.is_addressbook |= src.is_addressbook;
        ResourceProps::overwrite_some(&mut self.displayname, src.displayname);
        ResourceProps::overwrite_some(&mut self.current_user_principal, src.current_user_principal);
        ResourceProps::overwrite_some(&mut self.calendar_home_set, src.calendar_home_set);
        ResourceProps::overwrite_some(&mut self.addressbook_home_set, src.addressbook_home_set);
        ResourceProps::overwrite_some(&mut self.etag, src.etag);
        ResourceProps::overwrite_some(&mut self.content_type, src.content_type);
        ResourceProps::overwrite_some(&mut self.last_modified, src.last_modified);
        ResourceProps::overwrite_some(&mut self.creation_date, src.creation_date);
        ResourceProps::overwrite_some(&mut self.content_length, src.content_length);
        ResourceProps::overwrite_some(&mut self.calendar_description, src.calendar_description);
        ResourceProps::overwrite_some(
            &mut self.addressbook_description,
            src.addressbook_description,
        );
        ResourceProps::overwrite_some(&mut self.calendar_color, src.calendar_color);
        ResourceProps::overwrite_some(&mut self.calendar_order, src.calendar_order);
        ResourceProps::overwrite_some(&mut self.calendar_timezone, src.calendar_timezone);
        ResourceProps::overwrite_some(&mut self.calendar_data, src.calendar_data);
        ResourceProps::overwrite_some(&mut self.address_data, src.address_data);
        if !src.supported_components.is_empty() {
            self.supported_components = src.supported_components;
        }
    }

    fn overwrite_some<T>(target: &mut Option<T>, src: Option<T>) {
        if src.is_some() {
            *target = src;
        }
    }

    fn mark_resourcetype(&mut self, element: Element) {
        match element {
            Element::Collection => self.is_collection = true,
            Element::Calendar => self.is_calendar = true,
            Element::Addressbook => self.is_addressbook = true,
            _ => {}
        }
    }
}

#[derive(Debug, Clone)]
pub struct DavResponse {
    pub href: Href,
    pub status: Option<u16>,
    pub props: ResourceProps,
    pub propstat_errors: Vec<u16>,
}

impl DavResponse {
    pub fn is_gone(&self) -> bool {
        self.status.is_some_and(DavOutcome::is_gone)
            || self
                .propstat_errors
                .iter()
                .copied()
                .any(DavOutcome::is_gone)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Namespace {
    Dav,
    Caldav,
    Carddav,
    Apple,
    Other,
}

impl Namespace {
    fn of(resolved: &ResolveResult<'_>) -> Namespace {
        let ResolveResult::Bound(ns) = resolved else {
            return Namespace::Other;
        };
        hashify::map!(ns.as_ref().as_bytes(), Namespace,
            "DAV:" => Namespace::Dav,
            "urn:ietf:params:xml:ns:caldav" => Namespace::Caldav,
            "urn:ietf:params:xml:ns:carddav" => Namespace::Carddav,
            "http://apple.com/ns/ical/" => Namespace::Apple,
        )
        .copied()
        .unwrap_or(Namespace::Other)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Element {
    Response,
    Href,
    Status,
    Propstat,
    Prop,
    ResourceType,
    Collection,
    DisplayName,
    CurrentUserPrincipal,
    GetEtag,
    GetContentType,
    GetLastModified,
    CreationDate,
    GetContentLength,
    Calendar,
    CalendarHomeSet,
    CalendarDescription,
    CalendarTimezone,
    SupportedCalendarComponentSet,
    Comp,
    CalendarData,
    Addressbook,
    AddressbookHomeSet,
    AddressbookDescription,
    AddressData,
    CalendarColor,
    CalendarOrder,
    Other,
}

impl Element {
    fn of(ns: Namespace, local: &str) -> Element {
        let Some(&(expected, element)) = hashify::map_ignore_case!(local.as_bytes(), (Namespace, Element),
            "response" => (Namespace::Dav, Element::Response),
            "href" => (Namespace::Dav, Element::Href),
            "status" => (Namespace::Dav, Element::Status),
            "propstat" => (Namespace::Dav, Element::Propstat),
            "prop" => (Namespace::Dav, Element::Prop),
            "resourcetype" => (Namespace::Dav, Element::ResourceType),
            "collection" => (Namespace::Dav, Element::Collection),
            "displayname" => (Namespace::Dav, Element::DisplayName),
            "current-user-principal" => (Namespace::Dav, Element::CurrentUserPrincipal),
            "getetag" => (Namespace::Dav, Element::GetEtag),
            "getcontenttype" => (Namespace::Dav, Element::GetContentType),
            "getlastmodified" => (Namespace::Dav, Element::GetLastModified),
            "creationdate" => (Namespace::Dav, Element::CreationDate),
            "getcontentlength" => (Namespace::Dav, Element::GetContentLength),
            "calendar" => (Namespace::Caldav, Element::Calendar),
            "calendar-home-set" => (Namespace::Caldav, Element::CalendarHomeSet),
            "calendar-description" => (Namespace::Caldav, Element::CalendarDescription),
            "calendar-timezone" => (Namespace::Caldav, Element::CalendarTimezone),
            "supported-calendar-component-set" => (Namespace::Caldav, Element::SupportedCalendarComponentSet),
            "comp" => (Namespace::Caldav, Element::Comp),
            "calendar-data" => (Namespace::Caldav, Element::CalendarData),
            "addressbook" => (Namespace::Carddav, Element::Addressbook),
            "addressbook-home-set" => (Namespace::Carddav, Element::AddressbookHomeSet),
            "addressbook-description" => (Namespace::Carddav, Element::AddressbookDescription),
            "address-data" => (Namespace::Carddav, Element::AddressData),
            "calendar-color" => (Namespace::Apple, Element::CalendarColor),
            "calendar-order" => (Namespace::Apple, Element::CalendarOrder),
        ) else {
            return Element::Other;
        };
        if expected == ns {
            element
        } else {
            Element::Other
        }
    }
}

#[derive(Debug)]
enum Step {
    Start(Element, Option<String>),
    Empty(Element, Option<String>),
    End(Element),
    Text(String),
    Char(char),
    Other,
    Eof,
}

impl Step {
    fn opening(ns: Namespace, e: &BytesStart<'_>) -> (Element, Option<String>) {
        let element = Element::of(ns, e.local_name().as_ref());
        let name = match element {
            Element::Comp => e
                .attributes()
                .flatten()
                .find(|a| a.key.as_ref() == "name")
                .map(|a| a.value.into_owned()),
            _ => None,
        };
        (element, name)
    }
}

pub struct MultistatusParser<'b, R> {
    xml: NsReader<R>,
    buf: Vec<u8>,
    base_url: &'b str,
}

impl<'b, R: BufRead> MultistatusParser<'b, R> {
    pub fn parse(reader: R, base_url: &'b str) -> Result<Vec<DavResponse>, ParseError> {
        let mut parser = MultistatusParser {
            xml: NsReader::from_reader(reader),
            buf: Vec::new(),
            base_url,
        };
        let mut out: Vec<DavResponse> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        loop {
            match parser.next_step()? {
                Step::Start(Element::Response, _) => {
                    let response = parser.response()?;
                    if seen.insert(response.href.as_str().to_owned()) {
                        out.push(response);
                    }
                }
                Step::Eof => break,
                _ => {}
            }
        }
        Ok(out)
    }

    fn next_step(&mut self) -> Result<Step, ParseError> {
        self.buf.clear();
        let (resolved, event) = self.xml.read_resolved_event_into(&mut self.buf)?;
        let ns = Namespace::of(&resolved);
        Ok(match event {
            Event::Start(e) => {
                let (element, name) = Step::opening(ns, &e);
                Step::Start(element, name)
            }
            Event::Empty(e) => {
                let (element, name) = Step::opening(ns, &e);
                Step::Empty(element, name)
            }
            Event::End(e) => Step::End(Element::of(ns, e.local_name().as_ref())),
            Event::Text(t) => Step::Text(t.into_inner().into_owned()),
            Event::CData(cd) => Step::Text(cd.into_inner().into_owned()),
            Event::GeneralRef(g) => Entity::from_ref(&g).map_or(Step::Other, Step::Char),
            Event::Eof => Step::Eof,
            _ => Step::Other,
        })
    }

    fn unexpected_eof(context: &str) -> ParseError {
        ParseError::Xml(format!("unexpected EOF {context}"))
    }

    fn response(&mut self) -> Result<DavResponse, ParseError> {
        let mut href: Option<String> = None;
        let mut status: Option<u16> = None;
        let mut props = ResourceProps::default();
        let mut propstat_errors: Vec<u16> = Vec::new();
        loop {
            match self.next_step()? {
                Step::Start(Element::Href, _) => {
                    let text = self.token()?;
                    if href.is_none() {
                        href = Some(text);
                    }
                }
                Step::Start(Element::Status, _) => {
                    status = MultistatusParser::<R>::http_status(&self.token()?);
                }
                Step::Start(Element::Propstat, _) => match self.propstat()? {
                    (Some(s), block) if (200..300).contains(&s) => props.merge(block),
                    (Some(s), _) => propstat_errors.push(s),
                    (None, _) => {}
                },
                Step::Start(..) => self.skip()?,
                Step::End(Element::Response) => break,
                Step::Eof => return Err(MultistatusParser::<R>::unexpected_eof("in <response>")),
                _ => {}
            }
        }
        let raw_href = href.ok_or(ParseError::MissingHref)?;
        let href = Href::parse(self.base_url, &raw_href).map_err(|source| ParseError::Href {
            href: raw_href,
            source,
        })?;
        Ok(DavResponse {
            href,
            status,
            props,
            propstat_errors,
        })
    }

    fn propstat(&mut self) -> Result<(Option<u16>, ResourceProps), ParseError> {
        let mut status: Option<u16> = None;
        let mut props = ResourceProps::default();
        loop {
            match self.next_step()? {
                Step::Start(Element::Status, _) => {
                    status = MultistatusParser::<R>::http_status(&self.token()?);
                }
                Step::Start(Element::Prop, _) => self.prop_block(&mut props)?,
                Step::Start(..) => self.skip()?,
                Step::End(Element::Propstat) => break,
                Step::Eof => return Err(MultistatusParser::<R>::unexpected_eof("in <propstat>")),
                _ => {}
            }
        }
        Ok((status, props))
    }

    fn prop_block(&mut self, props: &mut ResourceProps) -> Result<(), ParseError> {
        loop {
            match self.next_step()? {
                Step::Start(element, _) => self.prop_element(props, element)?,
                Step::End(Element::Prop) => break,
                Step::Eof => return Err(MultistatusParser::<R>::unexpected_eof("in <prop>")),
                _ => {}
            }
        }
        Ok(())
    }

    fn prop_element(
        &mut self,
        props: &mut ResourceProps,
        element: Element,
    ) -> Result<(), ParseError> {
        match element {
            Element::ResourceType => self.resourcetype(props)?,
            Element::DisplayName => props.displayname = Some(self.token()?),
            Element::CurrentUserPrincipal => {
                props.current_user_principal = self.first_href(element)?;
            }
            Element::CalendarHomeSet => props.calendar_home_set = self.first_href(element)?,
            Element::AddressbookHomeSet => props.addressbook_home_set = self.first_href(element)?,
            Element::GetEtag => {
                let raw = self.token()?;
                if !raw.is_empty() {
                    props.etag = Some(raw);
                }
            }
            Element::GetContentType => props.content_type = Some(self.token()?),
            Element::GetLastModified => props.last_modified = Some(self.token()?),
            Element::CreationDate => props.creation_date = Some(self.token()?),
            Element::GetContentLength => props.content_length = self.token()?.parse::<u64>().ok(),
            Element::CalendarDescription => props.calendar_description = Some(self.token()?),
            Element::AddressbookDescription => props.addressbook_description = Some(self.token()?),
            Element::CalendarColor => props.calendar_color = Some(self.token()?),
            Element::CalendarOrder => props.calendar_order = self.token()?.parse::<i64>().ok(),
            Element::CalendarTimezone => props.calendar_timezone = Some(self.token()?),
            Element::SupportedCalendarComponentSet => self.supported_components(props)?,
            Element::CalendarData => props.calendar_data = Some(self.text()?),
            Element::AddressData => props.address_data = Some(self.text()?),
            _ => self.skip()?,
        }
        Ok(())
    }

    fn resourcetype(&mut self, props: &mut ResourceProps) -> Result<(), ParseError> {
        loop {
            match self.next_step()? {
                Step::Start(element, _) => {
                    props.mark_resourcetype(element);
                    self.skip()?;
                }
                Step::Empty(element, _) => props.mark_resourcetype(element),
                Step::End(Element::ResourceType) => break,
                Step::Eof => {
                    return Err(MultistatusParser::<R>::unexpected_eof("in <resourcetype>"));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn supported_components(&mut self, props: &mut ResourceProps) -> Result<(), ParseError> {
        loop {
            match self.next_step()? {
                Step::Start(Element::Comp, name) => {
                    props.supported_components.extend(name);
                    self.skip()?;
                }
                Step::Empty(Element::Comp, name) => props.supported_components.extend(name),
                Step::End(Element::SupportedCalendarComponentSet) => break,
                Step::Eof => {
                    return Err(MultistatusParser::<R>::unexpected_eof(
                        "in <supported-calendar-component-set>",
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn text(&mut self) -> Result<String, ParseError> {
        let mut out = String::new();
        loop {
            match self.next_step()? {
                Step::Text(s) => out.push_str(&s),
                Step::Char(c) => out.push(c),
                Step::Start(..) => self.skip()?,
                Step::End(_) | Step::Empty(..) => break,
                Step::Eof => return Err(MultistatusParser::<R>::unexpected_eof("reading text")),
                Step::Other => {}
            }
        }
        Ok(out)
    }

    fn token(&mut self) -> Result<String, ParseError> {
        let text = self.text()?;
        let trimmed = text.trim();
        Ok(if trimmed.len() == text.len() {
            text
        } else {
            trimmed.to_owned()
        })
    }

    fn first_href(&mut self, closing: Element) -> Result<Option<String>, ParseError> {
        let mut out: Option<String> = None;
        loop {
            match self.next_step()? {
                Step::Start(Element::Href, _) if out.is_none() => out = Some(self.token()?),
                Step::Start(..) => self.skip()?,
                Step::End(element) if element == closing => break,
                Step::Eof => return Err(MultistatusParser::<R>::unexpected_eof("reading href")),
                _ => {}
            }
        }
        Ok(out)
    }

    fn skip(&mut self) -> Result<(), ParseError> {
        let mut depth: u32 = 1;
        while depth > 0 {
            match self.next_step()? {
                Step::Start(..) => depth += 1,
                Step::End(_) => depth -= 1,
                Step::Eof => return Err(MultistatusParser::<R>::unexpected_eof("in skip")),
                _ => {}
            }
        }
        Ok(())
    }

    fn http_status(text: &str) -> Option<u16> {
        let mut parts = text.split_ascii_whitespace();
        let _version = parts.next()?;
        parts.next()?.parse::<u16>().ok()
    }
}

pub struct ControlBytes;

impl ControlBytes {
    pub fn is_illegal(b: u8) -> bool {
        matches!(b, 0x00..=0x08 | 0x0b | 0x0c | 0x0e..=0x1f)
    }

    pub fn strip(body: &[u8]) -> Cow<'_, [u8]> {
        if body.iter().any(|&b| ControlBytes::is_illegal(b)) {
            Cow::Owned(
                body.iter()
                    .copied()
                    .filter(|&b| !ControlBytes::is_illegal(b))
                    .collect(),
            )
        } else {
            Cow::Borrowed(body)
        }
    }

    fn compact(filled: &mut [u8]) -> usize {
        let mut write = 0;
        let mut read = 0;
        while let Some(rest) = filled.get(read..) {
            let run = rest
                .iter()
                .position(|&b| ControlBytes::is_illegal(b))
                .unwrap_or(rest.len());
            if write != read {
                filled.copy_within(read..read + run, write);
            }
            write += run;
            read += run + 1;
        }
        write
    }
}

pub struct ControlStrippingReader<R> {
    inner: R,
}

impl<R: Read> ControlStrippingReader<R> {
    pub fn new(inner: R) -> Self {
        ControlStrippingReader { inner }
    }
}

impl<R: Read> Read for ControlStrippingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let n = self.inner.read(buf)?;
            let Some(filled @ [_, ..]) = buf.get_mut(..n) else {
                return Ok(0);
            };
            let kept = ControlBytes::compact(filled);
            if kept > 0 {
                return Ok(kept);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn gone_probe(status: Option<u16>, propstat_errors: Vec<u16>) -> DavResponse {
        DavResponse {
            href: Href::from_normalised("/dav/cal/u/d/e.ics".to_owned()),
            status,
            props: ResourceProps::default(),
            propstat_errors,
        }
    }

    #[test]
    fn response_level_404_or_410_is_gone() {
        assert!(gone_probe(Some(404), vec![]).is_gone());
        assert!(gone_probe(Some(410), vec![]).is_gone());
    }

    #[test]
    fn per_propstat_404_or_410_is_gone() {
        assert!(gone_probe(None, vec![404]).is_gone());
        assert!(gone_probe(None, vec![200, 410]).is_gone());
    }

    #[test]
    fn other_propstat_errors_are_not_gone() {
        assert!(!gone_probe(None, vec![403]).is_gone());
        assert!(!gone_probe(None, vec![]).is_gone());
    }

    fn parse(body: &str) -> Vec<DavResponse> {
        MultistatusParser::parse(Cursor::new(body), "https://x/").expect("parse")
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
        let cleaned = ControlBytes::strip(raw);
        assert!(!cleaned.contains(&0x01));
    }

    #[test]
    fn stripping_reader_drops_control_bytes_across_reads() {
        let raw = b"\x01ab\x02\x03c\x1fd\x01";
        for chunk in [1, 2, 3, 64] {
            let mut reader =
                ControlStrippingReader::new(io::BufReader::with_capacity(chunk, &raw[..]));
            let mut out = Vec::new();
            reader.read_to_end(&mut out).expect("read");
            assert_eq!(out, b"abcd", "chunk {chunk}");
        }
    }

    #[test]
    fn strip_control_chars_leaves_tab_newline_cr() {
        let raw = b"<?xml ?>\t\n\r";
        let cleaned = ControlBytes::strip(raw);
        match cleaned {
            Cow::Borrowed(_) => {}
            Cow::Owned(_) => panic!("should not have allocated"),
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
    fn last_propstat_wins_for_duplicate_keys() {
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
