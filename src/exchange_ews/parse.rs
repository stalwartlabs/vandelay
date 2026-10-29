/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::io::BufRead;

use quick_xml::NsReader;
use quick_xml::XmlVersion;
use quick_xml::events::{BytesRef, BytesStart, Event};
use quick_xml::name::ResolveResult;

use crate::exchange_ews::error::EwsError;
use crate::exchange_ews::soap::{NS_MESSAGES, NS_TYPES};
use crate::exchange_ews::types::{CalendarItemType, FolderId, ItemId, ResponseCode, ServerVersion};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ns {
    Messages,
    Types,
    Other,
}

fn classify(ns: &ResolveResult<'_>) -> Ns {
    match ns {
        ResolveResult::Bound(prefix) => {
            let p = prefix.as_ref();
            if p == NS_MESSAGES {
                Ns::Messages
            } else if p == NS_TYPES {
                Ns::Types
            } else {
                Ns::Other
            }
        }
        _ => Ns::Other,
    }
}

fn is(ns: Ns, local: &str, expected_ns: Ns, target: &str) -> bool {
    ns == expected_ns && local.eq_ignore_ascii_case(target)
}

fn attr_value(e: &BytesStart<'_>, name: &str) -> Option<String> {
    for a in e.attributes().flatten() {
        let key = a.key.local_name();
        if key.as_ref().eq_ignore_ascii_case(name) {
            return Some(
                a.normalized_value(XmlVersion::Implicit1_0)
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| a.value.into_owned()),
            );
        }
    }
    None
}

fn capture_id_attrs(e: &BytesStart<'_>, id_out: &mut String, ck_out: &mut String) {
    for a in e.attributes().flatten() {
        let key = a.key.local_name();
        let kb = key.as_ref();
        let v = a
            .normalized_value(XmlVersion::Implicit1_0)
            .map(|c| c.into_owned())
            .unwrap_or_else(|_| a.value.into_owned());
        if kb.eq_ignore_ascii_case("Id") {
            *id_out = v;
        } else if kb.eq_ignore_ascii_case("ChangeKey") {
            *ck_out = v;
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ServerVersionInfo {
    pub major_version: Option<u32>,
    pub minor_version: Option<u32>,
    pub major_build: Option<u32>,
    pub minor_build: Option<u32>,
    pub version: Option<String>,
}

impl ServerVersionInfo {
    pub fn to_server_version(&self) -> Option<ServerVersion> {
        match (self.major_version, self.minor_version) {
            (Some(maj), Some(min)) => Some(ServerVersion::from_build(maj, min)),
            _ => None,
        }
    }
}

fn read_version_attrs(e: &BytesStart<'_>, out: &mut ServerVersionInfo) {
    for a in e.attributes().flatten() {
        let key = a.key.local_name();
        let kb = key.as_ref();
        let v = a.value.into_owned();
        if kb.eq_ignore_ascii_case("MajorVersion") {
            out.major_version = v.parse().ok();
        } else if kb.eq_ignore_ascii_case("MinorVersion") {
            out.minor_version = v.parse().ok();
        } else if kb.eq_ignore_ascii_case("MajorBuildNumber") {
            out.major_build = v.parse().ok();
        } else if kb.eq_ignore_ascii_case("MinorBuildNumber") {
            out.minor_build = v.parse().ok();
        } else if kb.eq_ignore_ascii_case("Version") {
            out.version = Some(v);
        }
    }
}

#[derive(Debug, Clone)]
pub struct SoapFault {
    pub fault_code: String,
    pub fault_string: String,
    pub response_code: ResponseCode,
    pub back_off_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum EnvelopeKind {
    Fault {
        version: ServerVersionInfo,
        fault: SoapFault,
    },
    Body {
        version: ServerVersionInfo,
    },
}

pub fn read_envelope_summary(bytes: &[u8]) -> Result<EnvelopeKind, EwsError> {
    let mut xml = NsReader::from_reader(bytes);
    xml.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut version = ServerVersionInfo::default();
    let mut seen_body = false;
    loop {
        buf.clear();
        let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
        let ns_kind = classify(&ns);
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let local = e.local_name().as_ref().to_owned();
                if is(ns_kind, &local, Ns::Types, "ServerVersionInfo") {
                    read_version_attrs(e, &mut version);
                } else if local.eq_ignore_ascii_case("Fault") {
                    let fault = parse_fault(&mut xml)?;
                    return Ok(EnvelopeKind::Fault { version, fault });
                } else if local.eq_ignore_ascii_case("Body") {
                    seen_body = true;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if seen_body {
        Ok(EnvelopeKind::Body { version })
    } else {
        Err(EwsError::Malformed("envelope had no soap:Body".to_owned()))
    }
}

fn parse_fault<R: BufRead>(xml: &mut NsReader<R>) -> Result<SoapFault, EwsError> {
    let mut buf = Vec::new();
    let mut fault_code = String::new();
    let mut fault_string = String::new();
    let mut code = ResponseCode::Other(String::new());
    let mut back_off: Option<u64> = None;
    let mut text_target: Option<&'static str> = None;
    let mut last_value_name: Option<String> = None;
    let mut depth: u32 = 1;
    loop {
        buf.clear();
        let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
        let ns_kind = classify(&ns);
        match ev {
            Event::Start(e) => {
                depth += 1;
                let local = e.local_name().as_ref().to_owned();
                if local.eq_ignore_ascii_case("faultcode") {
                    text_target = Some("faultcode");
                } else if local.eq_ignore_ascii_case("faultstring") {
                    text_target = Some("faultstring");
                } else if local.eq_ignore_ascii_case("ResponseCode") {
                    text_target = Some("responseCode");
                } else if local.eq_ignore_ascii_case("Value") && ns_kind == Ns::Types {
                    last_value_name = attr_value(&e, "Name");
                    text_target = Some("messageXmlValue");
                } else {
                    text_target = None;
                }
            }
            Event::Empty(e) => {
                let local = e.local_name().as_ref().to_owned();
                if is(ns_kind, &local, Ns::Types, "Value") {
                    last_value_name = attr_value(&e, "Name");
                }
            }
            Event::End(_) => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
                text_target = None;
                if depth == 0 {
                    break;
                }
            }
            Event::Text(t) => {
                let text = t.into_inner().into_owned();
                match text_target {
                    Some("faultcode") => fault_code = text,
                    Some("faultstring") => fault_string.push_str(&text),
                    Some("responseCode") => code = ResponseCode::parse(&text),
                    Some("messageXmlValue")
                        if last_value_name.as_deref() == Some("BackOffMilliseconds") =>
                    {
                        back_off = text.trim().parse().ok();
                    }
                    _ => {}
                }
            }
            Event::CData(c) if text_target == Some("faultstring") => {
                fault_string.push_str(&c);
            }
            Event::GeneralRef(ref g) if text_target == Some("faultstring") => {
                if let Some(c) = entity_to_char(g) {
                    fault_string.push(c);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if matches!(code, ResponseCode::ServerBusy { .. }) {
        code = ResponseCode::ServerBusy {
            back_off_ms: back_off,
        };
    }
    Ok(SoapFault {
        fault_code,
        fault_string,
        response_code: code,
        back_off_ms: back_off,
    })
}

#[derive(Debug, Clone, Default)]
pub struct FindFolderResponse {
    pub folders: Vec<FolderEntry>,
    pub more: bool,
    pub total_in_view: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct FolderEntry {
    pub element: FolderElement,
    pub folder_id: FolderId,
    pub parent_id: Option<String>,
    pub display_name: String,
    pub folder_class: String,
    pub total_count: Option<u32>,
    pub child_count: Option<u32>,
}

impl Default for FolderEntry {
    fn default() -> Self {
        FolderEntry {
            element: FolderElement::Folder,
            folder_id: FolderId {
                id: String::new(),
                change_key: String::new(),
            },
            parent_id: None,
            display_name: String::new(),
            folder_class: String::new(),
            total_count: None,
            child_count: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderElement {
    Folder,
    CalendarFolder,
    ContactsFolder,
    TasksFolder,
    SearchFolder,
}

impl FolderElement {
    fn from_local(local: &str) -> Option<FolderElement> {
        if local.eq_ignore_ascii_case("Folder") {
            Some(FolderElement::Folder)
        } else if local.eq_ignore_ascii_case("CalendarFolder") {
            Some(FolderElement::CalendarFolder)
        } else if local.eq_ignore_ascii_case("ContactsFolder") {
            Some(FolderElement::ContactsFolder)
        } else if local.eq_ignore_ascii_case("TasksFolder") {
            Some(FolderElement::TasksFolder)
        } else if local.eq_ignore_ascii_case("SearchFolder") {
            Some(FolderElement::SearchFolder)
        } else {
            None
        }
    }
}

pub fn parse_find_folder_response(body: &[u8]) -> Result<FindFolderResponse, EwsError> {
    let mut xml = NsReader::from_reader(body);
    xml.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out = FindFolderResponse::default();
    loop {
        buf.clear();
        let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
        let ns_kind = classify(&ns);
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let local = e.local_name().as_ref().to_owned();
                if is(ns_kind, &local, Ns::Messages, "RootFolder") {
                    if let Some(v) = attr_value(e, "TotalItemsInView") {
                        out.total_in_view = v.trim().parse().ok();
                    }
                    if let Some(v) = attr_value(e, "IncludesLastItemInRange") {
                        out.more = !matches!(v.trim(), "true" | "1");
                    }
                } else if ns_kind == Ns::Types
                    && let Some(elem) = FolderElement::from_local(&local)
                {
                    let entry = parse_folder_element(&mut xml, elem)?;
                    out.folders.push(entry);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

pub fn parse_folder_inner(inner_xml: &str) -> Result<Option<FolderEntry>, EwsError> {
    let mut xml = NsReader::from_reader(inner_xml.as_bytes());
    xml.config_mut().trim_text(true);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
        let ns_kind = classify(&ns);
        match ev {
            Event::Start(_) | Event::Empty(_) => {
                let local = match &ev {
                    Event::Start(e) | Event::Empty(e) => e.local_name().as_ref().to_owned(),
                    _ => continue,
                };
                if ns_kind == Ns::Types
                    && let Some(elem) = FolderElement::from_local(&local)
                {
                    return Ok(Some(parse_folder_element(&mut xml, elem)?));
                }
            }
            Event::Eof => return Ok(None),
            _ => {}
        }
    }
}

pub fn parse_get_folder_response(body: &[u8]) -> Result<Vec<FolderEntry>, EwsError> {
    let mut xml = NsReader::from_reader(body);
    xml.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out: Vec<FolderEntry> = Vec::new();
    loop {
        buf.clear();
        let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
        let ns_kind = classify(&ns);
        match ev {
            Event::Start(_) | Event::Empty(_) => {
                let local = match &ev {
                    Event::Start(e) | Event::Empty(e) => e.local_name().as_ref().to_owned(),
                    _ => continue,
                };
                if ns_kind == Ns::Types
                    && let Some(elem) = FolderElement::from_local(&local)
                {
                    let entry = parse_folder_element(&mut xml, elem)?;
                    out.push(entry);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

fn parse_folder_element<R: BufRead>(
    xml: &mut NsReader<R>,
    element: FolderElement,
) -> Result<FolderEntry, EwsError> {
    xml.config_mut().trim_text(false);
    let mut entry = FolderEntry {
        element,
        ..FolderEntry::default()
    };
    let mut buf = Vec::new();
    let mut depth: u32 = 1;
    let mut current: Option<&'static str> = None;
    let mut cur = String::new();
    loop {
        buf.clear();
        let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
        let ns_kind = classify(&ns);
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let is_empty = matches!(ev, Event::Empty(_));
                if !is_empty {
                    depth += 1;
                }
                let local = e.local_name().as_ref().to_owned();
                if ns_kind == Ns::Types {
                    if local.eq_ignore_ascii_case("FolderId") {
                        capture_id_attrs(
                            e,
                            &mut entry.folder_id.id,
                            &mut entry.folder_id.change_key,
                        );
                    } else if local.eq_ignore_ascii_case("ParentFolderId") {
                        let mut pid = String::new();
                        let mut ck = String::new();
                        capture_id_attrs(e, &mut pid, &mut ck);
                        if !pid.is_empty() {
                            entry.parent_id = Some(pid);
                        }
                    } else if local.eq_ignore_ascii_case("DisplayName") {
                        current = Some("displayName");
                        cur.clear();
                    } else if local.eq_ignore_ascii_case("FolderClass") {
                        current = Some("folderClass");
                        cur.clear();
                    } else if local.eq_ignore_ascii_case("TotalCount") {
                        current = Some("totalCount");
                        cur.clear();
                    } else if local.eq_ignore_ascii_case("ChildFolderCount") {
                        current = Some("childCount");
                        cur.clear();
                    } else {
                        current = None;
                    }
                }
                if is_empty {
                    current = None;
                }
            }
            Event::End(_) => {
                if let Some(field) = current.take() {
                    let text = std::mem::take(&mut cur);
                    match field {
                        "displayName" => entry.display_name = text.trim().to_owned(),
                        "folderClass" => entry.folder_class = text.trim().to_owned(),
                        "totalCount" => entry.total_count = text.trim().parse().ok(),
                        "childCount" => entry.child_count = text.trim().parse().ok(),
                        _ => {}
                    }
                }
                if depth == 0 {
                    break;
                }
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Event::Text(t) => {
                cur.push_str(&t);
            }
            Event::CData(c) => {
                cur.push_str(&c);
            }
            Event::GeneralRef(ref g) => {
                if let Some(c) = entity_to_char(g) {
                    cur.push(c);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(entry)
}

#[derive(Debug, Clone, Default)]
pub struct FindItemResponse {
    pub items: Vec<ItemEntry>,
    pub total_in_view: Option<u32>,
    pub more: bool,
}

#[derive(Debug, Clone)]
pub struct ItemEntry {
    pub element: String,
    pub id: ItemId,
}

pub fn parse_find_item_response(body: &[u8]) -> Result<FindItemResponse, EwsError> {
    let mut xml = NsReader::from_reader(body);
    xml.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out = FindItemResponse::default();
    let mut in_root = false;
    loop {
        buf.clear();
        let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
        let ns_kind = classify(&ns);
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let local = e.local_name().as_ref().to_owned();
                if is(ns_kind, &local, Ns::Messages, "RootFolder") {
                    in_root = true;
                    if let Some(v) = attr_value(e, "TotalItemsInView") {
                        out.total_in_view = v.trim().parse().ok();
                    }
                    if let Some(v) = attr_value(e, "IncludesLastItemInRange") {
                        out.more = !matches!(v.trim(), "true" | "1");
                    }
                } else if in_root && ns_kind == Ns::Types {
                    if is_item_element(&local) {
                        out.items.push(ItemEntry {
                            element: local.clone(),
                            id: ItemId::default(),
                        });
                    } else if local.eq_ignore_ascii_case("ItemId")
                        && let Some(last) = out.items.last_mut()
                    {
                        capture_id_attrs(e, &mut last.id.id, &mut last.id.change_key);
                    }
                }
            }
            Event::End(e) => {
                let local = e.local_name().as_ref().to_owned();
                if local.eq_ignore_ascii_case("RootFolder") {
                    in_root = false;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

pub(crate) fn entity_to_char(g: &BytesRef) -> Option<char> {
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

fn is_item_element(local: &str) -> bool {
    matches!(
        local.to_ascii_lowercase().as_str(),
        "message"
            | "calendaritem"
            | "contact"
            | "distributionlist"
            | "meetingrequest"
            | "meetingresponse"
            | "meetingmessage"
            | "meetingcancellation"
            | "item"
    )
}

#[derive(Debug, Clone)]
pub struct ResponseMessage {
    pub success: bool,
    pub response_code: ResponseCode,
    pub message_text: String,
    pub inner_xml: String,
}

pub fn parse_response_messages(
    body: &[u8],
    response_message_local: &str,
) -> Result<Vec<ResponseMessage>, EwsError> {
    let mut xml = NsReader::from_reader(body);
    xml.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut out: Vec<ResponseMessage> = Vec::new();
    loop {
        buf.clear();
        let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
        let ns_kind = classify(&ns);
        match ev {
            Event::Start(e) => {
                let local = e.local_name().as_ref().to_owned();
                if is(ns_kind, &local, Ns::Messages, response_message_local) {
                    let response_class =
                        attr_value(&e, "ResponseClass").unwrap_or_else(|| "Success".to_owned());
                    let msg = parse_one_response_message(&mut xml, response_class)?;
                    out.push(msg);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

fn parse_one_response_message<R: BufRead>(
    xml: &mut NsReader<R>,
    response_class: String,
) -> Result<ResponseMessage, EwsError> {
    let mut buf = Vec::new();
    let mut response_code = ResponseCode::NoError;
    let mut message_text = String::new();
    let mut inner = String::new();
    let mut depth: u32 = 1;
    let mut current: Option<&'static str> = None;
    let mut capture_depth: u32 = 0;
    loop {
        buf.clear();
        let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
        let ns_kind = classify(&ns);
        let in_capture = capture_depth > 0;
        match ev {
            Event::Start(e) => {
                depth += 1;
                let local = e.local_name().as_ref().to_owned();
                if !in_capture && is(ns_kind, &local, Ns::Messages, "ResponseCode") {
                    current = Some("responseCode");
                } else if !in_capture && is(ns_kind, &local, Ns::Messages, "MessageText") {
                    current = Some("messageText");
                } else if !in_capture
                    && (is(ns_kind, &local, Ns::Messages, "Items")
                        || is(ns_kind, &local, Ns::Messages, "Attachments")
                        || is(ns_kind, &local, Ns::Messages, "Folders"))
                {
                    capture_depth = 1;
                    current = None;
                } else if in_capture {
                    write_start_xml(&mut inner, &e);
                    capture_depth += 1;
                } else {
                    current = None;
                }
            }
            Event::Empty(e) => {
                let local = e.local_name().as_ref().to_owned();
                if in_capture {
                    write_empty_xml(&mut inner, &e);
                } else if is(ns_kind, &local, Ns::Messages, "ResponseCode")
                    || is(ns_kind, &local, Ns::Messages, "MessageText")
                {
                }
            }
            Event::End(e) => {
                if in_capture {
                    if capture_depth == 1 {
                        capture_depth = 0;
                    } else {
                        write_end_xml_event(&mut inner, &e);
                        capture_depth -= 1;
                    }
                }
                if depth == 0 {
                    break;
                }
                depth -= 1;
                current = None;
                if depth == 0 {
                    break;
                }
            }
            Event::Text(t) => {
                let text = t.into_inner().into_owned();
                if in_capture {
                    write_text_xml(&mut inner, &text);
                } else {
                    match current {
                        Some("responseCode") => response_code = ResponseCode::parse(text.trim()),
                        Some("messageText") => message_text = text,
                        _ => {}
                    }
                }
            }
            Event::CData(c) => {
                let text = c.into_inner().into_owned();
                if in_capture {
                    write_text_xml(&mut inner, &text);
                }
            }
            Event::GeneralRef(ref g) => {
                if in_capture {
                    inner.push('&');
                    inner.push_str(g.as_ref());
                    inner.push(';');
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(ResponseMessage {
        success: matches!(response_class.as_str(), "Success" | "Warning"),
        response_code,
        message_text,
        inner_xml: wrap_inner_with_namespaces(&inner),
    })
}

fn wrap_inner_with_namespaces(inner: &str) -> String {
    if inner.is_empty() {
        return String::new();
    }
    format!(
        "<vandelay-inner xmlns=\"{NS_TYPES}\" xmlns:t=\"{NS_TYPES}\" xmlns:m=\"{NS_MESSAGES}\">{inner}</vandelay-inner>",
    )
}

fn write_start_xml(out: &mut String, e: &BytesStart<'_>) {
    out.push('<');
    out.push_str(e.name().as_ref());
    for a in e.attributes().flatten() {
        out.push(' ');
        out.push_str(a.key.as_ref());
        out.push_str("=\"");
        let val = a.value;
        for ch in val.chars() {
            match ch {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                _ => out.push(ch),
            }
        }
        out.push('"');
    }
    out.push('>');
}

fn write_empty_xml(out: &mut String, e: &BytesStart<'_>) {
    out.push('<');
    out.push_str(e.name().as_ref());
    for a in e.attributes().flatten() {
        out.push(' ');
        out.push_str(a.key.as_ref());
        out.push_str("=\"");
        let val = a.value;
        for ch in val.chars() {
            match ch {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                _ => out.push(ch),
            }
        }
        out.push('"');
    }
    out.push_str("/>");
}

fn write_end_xml_event(out: &mut String, e: &quick_xml::events::BytesEnd<'_>) {
    out.push_str("</");
    out.push_str(e.name().as_ref());
    out.push('>');
}

fn write_text_xml(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SyncFolderItemsResponse {
    pub sync_state: String,
    pub more: bool,
    pub changes: Vec<SyncChange>,
}

#[derive(Debug, Clone)]
pub enum SyncChange {
    Create { id: ItemId, element: String },
    Update { id: ItemId, element: String },
    Delete { id: ItemId },
    ReadFlagChange { id: ItemId, is_read: bool },
}

pub fn parse_sync_folder_items_response(body: &[u8]) -> Result<SyncFolderItemsResponse, EwsError> {
    let mut xml = NsReader::from_reader(body);
    xml.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut sync_state = String::new();
    let mut more = false;
    let mut changes: Vec<SyncChange> = Vec::new();
    let mut current_text: Option<&'static str> = None;
    let mut in_changes = false;
    let mut current_change: Option<SyncChange> = None;
    let mut reading_is_read = false;
    let mut pending_is_read: Option<bool> = None;
    loop {
        buf.clear();
        let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
        let ns_kind = classify(&ns);
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let local = e.local_name().as_ref().to_owned();
                let is_empty = matches!(ev, Event::Empty(_));
                if is(ns_kind, &local, Ns::Messages, "SyncState") {
                    current_text = Some("syncState");
                } else if is(ns_kind, &local, Ns::Messages, "IncludesLastItemInRange") {
                    current_text = Some("includesLast");
                } else if is(ns_kind, &local, Ns::Messages, "Changes") {
                    in_changes = true;
                } else if in_changes && ns_kind == Ns::Types {
                    if local.eq_ignore_ascii_case("Create") {
                        current_change = Some(SyncChange::Create {
                            id: ItemId::default(),
                            element: String::new(),
                        });
                    } else if local.eq_ignore_ascii_case("Update") {
                        current_change = Some(SyncChange::Update {
                            id: ItemId::default(),
                            element: String::new(),
                        });
                    } else if local.eq_ignore_ascii_case("Delete") {
                        current_change = Some(SyncChange::Delete {
                            id: ItemId::default(),
                        });
                    } else if local.eq_ignore_ascii_case("ReadFlagChange") {
                        current_change = Some(SyncChange::ReadFlagChange {
                            id: ItemId::default(),
                            is_read: false,
                        });
                        pending_is_read = None;
                    } else if local.eq_ignore_ascii_case("ItemId") {
                        if let Some(change) = current_change.as_mut() {
                            let (id, ck) = item_id_mut(change);
                            capture_id_attrs(e, id, ck);
                        }
                    } else if local.eq_ignore_ascii_case("IsRead") {
                        reading_is_read = true;
                    } else if is_item_element(&local) {
                        match current_change.as_mut() {
                            Some(SyncChange::Create { element, .. })
                            | Some(SyncChange::Update { element, .. }) => {
                                *element = local.clone();
                            }
                            _ => {}
                        }
                    }
                }
                if is_empty {
                    current_text = None;
                }
            }
            Event::End(e) => {
                let local = e.local_name().as_ref().to_owned();
                let lower = local.to_ascii_lowercase();
                match lower.as_str() {
                    "create" | "update" | "delete" | "readflagchange" => {
                        if let Some(mut change) = current_change.take() {
                            if let SyncChange::ReadFlagChange { is_read, .. } = &mut change
                                && let Some(v) = pending_is_read.take()
                            {
                                *is_read = v;
                            }
                            changes.push(change);
                        }
                    }
                    "isread" => reading_is_read = false,
                    "changes" => in_changes = false,
                    _ => {}
                }
                current_text = None;
            }
            Event::Text(t) => {
                let text = t.into_inner().into_owned();
                if reading_is_read {
                    pending_is_read = Some(matches!(text.trim(), "true" | "1"));
                }
                match current_text {
                    Some("syncState") => sync_state = text,
                    Some("includesLast") => more = !matches!(text.trim(), "true" | "1"),
                    _ => {}
                }
            }
            Event::CData(c) => {
                let text = c.into_inner().into_owned();
                if current_text == Some("syncState") {
                    sync_state = text;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(SyncFolderItemsResponse {
        sync_state,
        more,
        changes,
    })
}

fn item_id_mut(change: &mut SyncChange) -> (&mut String, &mut String) {
    match change {
        SyncChange::Create { id, .. }
        | SyncChange::Update { id, .. }
        | SyncChange::Delete { id }
        | SyncChange::ReadFlagChange { id, .. } => (&mut id.id, &mut id.change_key),
    }
}

#[derive(Debug, Clone, Default)]
pub struct MessageItem {
    pub element: String,
    pub id: ItemId,
    pub parent_folder_id: Option<String>,
    pub mime_content: Option<String>,
    pub mime_charset: Option<String>,
    pub subject: Option<String>,
    pub date_time_received: Option<String>,
    pub is_read: Option<bool>,
    pub is_draft: Option<bool>,
    pub is_read_receipt_requested: Option<bool>,
    pub categories: Vec<String>,
    pub flag_status: Option<String>,
}

pub fn parse_message_item(inner_xml: &str) -> Result<MessageItem, EwsError> {
    let mut xml = NsReader::from_reader(inner_xml.as_bytes());
    xml.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut item = MessageItem::default();
    let mut current: Option<&'static str> = None;
    let mut mime_charset: Option<String> = None;
    let mut category_collecting = false;
    let mut in_flag = false;
    let mut in_flagstatus_ext = false;
    let mut cur = String::new();
    loop {
        buf.clear();
        let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
        let ns_kind = classify(&ns);
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let local = e.local_name().as_ref().to_owned();
                let is_empty = matches!(ev, Event::Empty(_));
                if item.element.is_empty() && is_item_element(&local) {
                    item.element = local.clone();
                } else if ns_kind == Ns::Types {
                    cur.clear();
                    if local.eq_ignore_ascii_case("ItemId") {
                        capture_id_attrs(e, &mut item.id.id, &mut item.id.change_key);
                    } else if local.eq_ignore_ascii_case("ParentFolderId") {
                        let mut pid = String::new();
                        let mut ck = String::new();
                        capture_id_attrs(e, &mut pid, &mut ck);
                        if !pid.is_empty() {
                            item.parent_folder_id = Some(pid);
                        }
                    } else if local.eq_ignore_ascii_case("MimeContent") {
                        mime_charset = attr_value(e, "CharacterSet");
                        current = Some("mimeContent");
                    } else if local.eq_ignore_ascii_case("Subject") {
                        current = Some("subject");
                    } else if local.eq_ignore_ascii_case("DateTimeReceived") {
                        current = Some("received");
                    } else if local.eq_ignore_ascii_case("IsRead") {
                        current = Some("isRead");
                    } else if local.eq_ignore_ascii_case("IsDraft") {
                        current = Some("isDraft");
                    } else if local.eq_ignore_ascii_case("IsReadReceiptRequested") {
                        current = Some("readReceipt");
                    } else if local.eq_ignore_ascii_case("Categories") {
                        category_collecting = true;
                    } else if category_collecting && local.eq_ignore_ascii_case("String") {
                        current = Some("category");
                    } else if local.eq_ignore_ascii_case("Flag") {
                        in_flag = true;
                    } else if in_flag && local.eq_ignore_ascii_case("FlagStatus") {
                        current = Some("flagStatus");
                    } else if local.eq_ignore_ascii_case("ExtendedFieldURI") {
                        if let Some(tag) = attr_value(e, "PropertyTag") {
                            let t = tag.trim().to_ascii_lowercase();
                            in_flagstatus_ext = t == "0x1090" || t == "4240";
                        }
                    } else if in_flagstatus_ext && local.eq_ignore_ascii_case("Value") {
                        current = Some("flagStatusExt");
                    } else {
                        current = None;
                    }
                }
                if is_empty {
                    current = None;
                }
            }
            Event::End(e) => {
                let local = e.local_name().as_ref().to_owned();
                let lower = local.to_ascii_lowercase();
                if let Some(field) = current.take() {
                    let text = std::mem::take(&mut cur);
                    match field {
                        "mimeContent" => {
                            item.mime_content = Some(text);
                            item.mime_charset = mime_charset.clone();
                        }
                        "subject" => item.subject = Some(text),
                        "received" => item.date_time_received = Some(text),
                        "isRead" => item.is_read = Some(matches!(text.trim(), "true" | "1")),
                        "isDraft" => item.is_draft = Some(matches!(text.trim(), "true" | "1")),
                        "readReceipt" => {
                            item.is_read_receipt_requested =
                                Some(matches!(text.trim(), "true" | "1"));
                        }
                        "category" => item.categories.push(text),
                        "flagStatus" => item.flag_status = Some(text),
                        "flagStatusExt" if text.trim() == "2" => {
                            item.flag_status = Some("Flagged".to_owned());
                        }
                        _ => {}
                    }
                }
                cur.clear();
                if lower == "categories" {
                    category_collecting = false;
                } else if lower == "flag" {
                    in_flag = false;
                } else if lower == "extendedproperty" {
                    in_flagstatus_ext = false;
                }
            }
            Event::Text(ref t) => {
                cur.push_str(t);
            }
            Event::CData(ref c) => {
                cur.push_str(c);
            }
            Event::GeneralRef(ref g) => {
                if let Some(c) = entity_to_char(g) {
                    cur.push(c);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(item)
}

#[derive(Debug, Clone, Default)]
pub struct CalendarItemRaw {
    pub element: String,
    pub id: ItemId,
    pub parent_folder_id: Option<String>,
    pub uid: Option<String>,
    pub subject: Option<String>,
    pub start: Option<String>,
    pub end: Option<String>,
    pub original_start: Option<String>,
    pub is_all_day_event: Option<bool>,
    pub legacy_free_busy_status: Option<String>,
    pub location: Option<String>,
    pub calendar_item_type: Option<CalendarItemType>,
    pub recurrence_id: Option<String>,
    pub start_tz: Option<String>,
    pub end_tz: Option<String>,
    pub recurrence: Option<RawRecurrence>,
    pub modified_occurrences: Vec<RawOccurrence>,
    pub deleted_occurrences: Vec<RawOccurrence>,
    pub organizer_smtp: Option<String>,
    pub organizer_routing_type: Option<String>,
    pub organizer_name: Option<String>,
    pub required_attendees: Vec<RawAttendee>,
    pub optional_attendees: Vec<RawAttendee>,
    pub resources: Vec<RawAttendee>,
    pub reminder_is_set: Option<bool>,
    pub reminder_minutes_before_start: Option<i64>,
    pub is_online_meeting: Option<bool>,
    pub join_online_meeting_url: Option<String>,
    pub net_show_url: Option<String>,
    pub meeting_workspace_url: Option<String>,
    pub categories: Vec<String>,
    pub created: Option<String>,
    pub last_modified: Option<String>,
    pub body_text: Option<String>,
    pub body_html: Option<String>,
    pub attachments: Vec<RawAttachmentRef>,
}

#[derive(Debug, Clone, Default)]
pub struct RawOccurrence {
    pub item_id: ItemId,
    pub start: Option<String>,
    pub end: Option<String>,
    pub original_start: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RawAttendee {
    pub email: Option<String>,
    pub routing_type: Option<String>,
    pub name: Option<String>,
    pub response_type: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RawAttachmentRef {
    pub attachment_id: String,
    pub name: Option<String>,
    pub content_type: Option<String>,
    pub is_contact_photo: bool,
    pub is_item_attachment: bool,
}

#[derive(Debug, Clone, Default)]
pub struct RawRecurrence {
    pub pattern: Option<RecurrencePattern>,
    pub range: Option<RecurrenceRange>,
}

#[derive(Debug, Clone)]
pub enum RecurrencePattern {
    Daily {
        interval: u32,
    },
    Weekly {
        interval: u32,
        days_of_week: Vec<String>,
    },
    AbsoluteMonthly {
        interval: u32,
        day_of_month: u32,
    },
    RelativeMonthly {
        interval: u32,
        day_of_week_index: String,
        days_of_week: Vec<String>,
    },
    AbsoluteYearly {
        month: String,
        day_of_month: u32,
    },
    RelativeYearly {
        month: String,
        day_of_week_index: String,
        days_of_week: Vec<String>,
    },
}

#[derive(Debug, Clone)]
pub enum RecurrenceRange {
    NoEnd {
        start_date: String,
    },
    EndDate {
        start_date: String,
        end_date: String,
    },
    Numbered {
        start_date: String,
        number_of_occurrences: u32,
    },
}

pub fn parse_calendar_item(inner_xml: &str) -> Result<CalendarItemRaw, EwsError> {
    let mut xml = NsReader::from_reader(inner_xml.as_bytes());
    xml.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut item = CalendarItemRaw::default();
    let mut text_target: Option<&'static str> = None;
    let mut category_collecting = false;
    let mut attendee_kind: Option<&'static str> = None;
    let mut current_attendee: Option<RawAttendee> = None;
    let mut in_mailbox = false;
    let mut in_organizer = false;
    let mut occurrence_stack: Vec<RawOccurrence> = Vec::new();
    let mut deleted_stack: Vec<RawOccurrence> = Vec::new();
    let mut in_modified = false;
    let mut in_deleted = false;
    let mut recurrence_path: Vec<String> = Vec::new();
    let mut recurrence_text: Option<&'static str> = None;
    let mut recurrence = RawRecurrence::default();
    let mut pending = PendingRecurrence::default();
    let mut cur = String::new();

    loop {
        buf.clear();
        let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
        let ns_kind = classify(&ns);
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let local = e.local_name().as_ref().to_owned();
                let is_empty = matches!(ev, Event::Empty(_));
                if item.element.is_empty() && is_item_element(&local) {
                    item.element = local.clone();
                    continue;
                }
                if ns_kind != Ns::Types {
                    continue;
                }
                if !recurrence_path.is_empty() {
                    recurrence_path.push(local.clone());
                    if local.eq_ignore_ascii_case("Interval") {
                        recurrence_text = Some("interval");
                    } else if local.eq_ignore_ascii_case("DaysOfWeek") {
                        recurrence_text = Some("daysOfWeek");
                    } else if local.eq_ignore_ascii_case("DayOfMonth") {
                        recurrence_text = Some("dayOfMonth");
                    } else if local.eq_ignore_ascii_case("DayOfWeekIndex") {
                        recurrence_text = Some("dayOfWeekIndex");
                    } else if local.eq_ignore_ascii_case("Month") {
                        recurrence_text = Some("month");
                    } else if local.eq_ignore_ascii_case("StartDate") {
                        recurrence_text = Some("startDate");
                    } else if local.eq_ignore_ascii_case("EndDate") {
                        recurrence_text = Some("endDate");
                    } else if local.eq_ignore_ascii_case("NumberOfOccurrences") {
                        recurrence_text = Some("numberOfOccurrences");
                    } else if local.eq_ignore_ascii_case("DailyRecurrence") {
                        pending.pattern_choice = Some("Daily");
                    } else if local.eq_ignore_ascii_case("WeeklyRecurrence") {
                        pending.pattern_choice = Some("Weekly");
                    } else if local.eq_ignore_ascii_case("AbsoluteMonthlyRecurrence") {
                        pending.pattern_choice = Some("AbsoluteMonthly");
                    } else if local.eq_ignore_ascii_case("RelativeMonthlyRecurrence") {
                        pending.pattern_choice = Some("RelativeMonthly");
                    } else if local.eq_ignore_ascii_case("AbsoluteYearlyRecurrence") {
                        pending.pattern_choice = Some("AbsoluteYearly");
                    } else if local.eq_ignore_ascii_case("RelativeYearlyRecurrence") {
                        pending.pattern_choice = Some("RelativeYearly");
                    } else if local.eq_ignore_ascii_case("NoEndRecurrence") {
                        pending.range_choice = Some("NoEnd");
                    } else if local.eq_ignore_ascii_case("EndDateRecurrence") {
                        pending.range_choice = Some("EndDate");
                    } else if local.eq_ignore_ascii_case("NumberedRecurrence") {
                        pending.range_choice = Some("Numbered");
                    } else {
                        recurrence_text = None;
                    }
                    if is_empty {
                        recurrence_path.pop();
                        recurrence_text = None;
                    }
                    continue;
                }
                cur.clear();
                if local.eq_ignore_ascii_case("ItemId") {
                    if in_modified {
                        if let Some(occ) = occurrence_stack.last_mut() {
                            capture_id_attrs(e, &mut occ.item_id.id, &mut occ.item_id.change_key);
                        }
                    } else if in_deleted {
                        if let Some(occ) = deleted_stack.last_mut() {
                            capture_id_attrs(e, &mut occ.item_id.id, &mut occ.item_id.change_key);
                        }
                    } else {
                        capture_id_attrs(e, &mut item.id.id, &mut item.id.change_key);
                    }
                } else if local.eq_ignore_ascii_case("ParentFolderId") {
                    let mut pid = String::new();
                    let mut ck = String::new();
                    capture_id_attrs(e, &mut pid, &mut ck);
                    if !pid.is_empty() {
                        item.parent_folder_id = Some(pid);
                    }
                } else if local.eq_ignore_ascii_case("UID") {
                    text_target = Some("uid");
                } else if local.eq_ignore_ascii_case("Subject") {
                    text_target = Some("subject");
                } else if local.eq_ignore_ascii_case("Start") {
                    if in_modified || in_deleted {
                        text_target = Some("occStart");
                    } else {
                        text_target = Some("start");
                    }
                } else if local.eq_ignore_ascii_case("End") {
                    if in_modified || in_deleted {
                        text_target = Some("occEnd");
                    } else {
                        text_target = Some("end");
                    }
                } else if local.eq_ignore_ascii_case("OriginalStart") {
                    if in_modified || in_deleted {
                        text_target = Some("occOrig");
                    } else {
                        text_target = Some("originalStart");
                    }
                } else if local.eq_ignore_ascii_case("IsAllDayEvent") {
                    text_target = Some("isAllDay");
                } else if local.eq_ignore_ascii_case("LegacyFreeBusyStatus") {
                    text_target = Some("freeBusy");
                } else if local.eq_ignore_ascii_case("Location") {
                    text_target = Some("location");
                } else if local.eq_ignore_ascii_case("CalendarItemType") {
                    text_target = Some("calendarItemType");
                } else if local.eq_ignore_ascii_case("RecurrenceId") {
                    text_target = Some("recurrenceId");
                } else if local.eq_ignore_ascii_case("StartTimeZone") {
                    if let Some(v) = attr_value(e, "Id") {
                        item.start_tz = Some(v);
                    }
                } else if local.eq_ignore_ascii_case("EndTimeZone") {
                    if let Some(v) = attr_value(e, "Id") {
                        item.end_tz = Some(v);
                    }
                } else if local.eq_ignore_ascii_case("Recurrence") {
                    recurrence_path.push(local.clone());
                } else if local.eq_ignore_ascii_case("ModifiedOccurrences") {
                    in_modified = true;
                } else if local.eq_ignore_ascii_case("DeletedOccurrences") {
                    in_deleted = true;
                } else if local.eq_ignore_ascii_case("Occurrence") {
                    let mut occ = RawOccurrence::default();
                    capture_id_attrs(e, &mut occ.item_id.id, &mut occ.item_id.change_key);
                    occurrence_stack.push(occ);
                } else if local.eq_ignore_ascii_case("DeletedOccurrence") {
                    deleted_stack.push(RawOccurrence::default());
                } else if local.eq_ignore_ascii_case("Organizer") {
                    in_organizer = true;
                } else if local.eq_ignore_ascii_case("RequiredAttendees") {
                    attendee_kind = Some("required");
                } else if local.eq_ignore_ascii_case("OptionalAttendees") {
                    attendee_kind = Some("optional");
                } else if local.eq_ignore_ascii_case("Resources") {
                    attendee_kind = Some("resource");
                } else if local.eq_ignore_ascii_case("ReminderIsSet") {
                    text_target = Some("reminderIsSet");
                } else if local.eq_ignore_ascii_case("ReminderMinutesBeforeStart") {
                    text_target = Some("reminderMinutes");
                } else if local.eq_ignore_ascii_case("IsOnlineMeeting") {
                    text_target = Some("isOnlineMeeting");
                } else if local.eq_ignore_ascii_case("JoinOnlineMeetingUrl") {
                    text_target = Some("joinUrl");
                } else if local.eq_ignore_ascii_case("NetShowUrl") {
                    text_target = Some("netShowUrl");
                } else if local.eq_ignore_ascii_case("MeetingWorkspaceUrl") {
                    text_target = Some("workspaceUrl");
                } else if local.eq_ignore_ascii_case("Attendee") && attendee_kind.is_some() {
                    current_attendee = Some(RawAttendee::default());
                } else if local.eq_ignore_ascii_case("Mailbox") {
                    in_mailbox = true;
                } else if local.eq_ignore_ascii_case("Name") && in_mailbox {
                    text_target = Some(if in_organizer {
                        "organizerName"
                    } else {
                        "attendeeName"
                    });
                } else if local.eq_ignore_ascii_case("EmailAddress") && in_mailbox {
                    text_target = Some(if in_organizer {
                        "organizerEmail"
                    } else {
                        "attendeeEmail"
                    });
                } else if local.eq_ignore_ascii_case("RoutingType") && in_mailbox {
                    text_target = Some(if in_organizer {
                        "organizerRouting"
                    } else {
                        "attendeeRouting"
                    });
                } else if local.eq_ignore_ascii_case("ResponseType") && current_attendee.is_some() {
                    text_target = Some("attendeeResponse");
                } else if local.eq_ignore_ascii_case("Categories") {
                    category_collecting = true;
                } else if category_collecting && local.eq_ignore_ascii_case("String") {
                    text_target = Some("category");
                } else if local.eq_ignore_ascii_case("DateTimeCreated") {
                    text_target = Some("created");
                } else if local.eq_ignore_ascii_case("LastModifiedTime") {
                    text_target = Some("lastModified");
                } else if local.eq_ignore_ascii_case("Body") {
                    let body_type = attr_value(e, "BodyType").unwrap_or_default();
                    text_target = Some(if body_type.eq_ignore_ascii_case("HTML") {
                        "bodyHtml"
                    } else {
                        "bodyText"
                    });
                } else if local.eq_ignore_ascii_case("FileAttachment")
                    || local.eq_ignore_ascii_case("ItemAttachment")
                {
                    let att = parse_attachment_ref(&mut xml, &local)?;
                    item.attachments.push(att);
                } else {
                    text_target = None;
                }
                if is_empty {
                    text_target = None;
                }
            }
            Event::End(e) => {
                let local = e.local_name().as_ref().to_owned();
                let lower = local.to_ascii_lowercase();
                if !recurrence_path.is_empty() {
                    recurrence_path.pop();
                    if recurrence_path.is_empty() {
                        pending.finalize(&mut recurrence);
                        item.recurrence = Some(recurrence.clone());
                        pending = PendingRecurrence::default();
                    }
                    recurrence_text = None;
                    continue;
                }
                if let Some(tt) = text_target {
                    let text = std::mem::take(&mut cur);
                    match tt {
                        "uid" => item.uid = Some(text),
                        "subject" => item.subject = Some(text),
                        "start" => item.start = Some(text),
                        "end" => item.end = Some(text),
                        "originalStart" => item.original_start = Some(text),
                        "isAllDay" => {
                            item.is_all_day_event = Some(matches!(text.trim(), "true" | "1"));
                        }
                        "freeBusy" => item.legacy_free_busy_status = Some(text),
                        "location" => item.location = Some(text),
                        "calendarItemType" => {
                            item.calendar_item_type = CalendarItemType::parse(text.trim());
                        }
                        "recurrenceId" => item.recurrence_id = Some(text),
                        "organizerName" => item.organizer_name = Some(text),
                        "organizerEmail" => item.organizer_smtp = Some(text),
                        "organizerRouting" => item.organizer_routing_type = Some(text),
                        "attendeeName" => {
                            if let Some(att) = current_attendee.as_mut() {
                                att.name = Some(text);
                            }
                        }
                        "attendeeEmail" => {
                            if let Some(att) = current_attendee.as_mut() {
                                att.email = Some(text);
                            }
                        }
                        "attendeeRouting" => {
                            if let Some(att) = current_attendee.as_mut() {
                                att.routing_type = Some(text);
                            }
                        }
                        "attendeeResponse" => {
                            if let Some(att) = current_attendee.as_mut() {
                                att.response_type = Some(text);
                            }
                        }
                        "reminderIsSet" => {
                            item.reminder_is_set = Some(matches!(text.trim(), "true" | "1"));
                        }
                        "reminderMinutes" => {
                            item.reminder_minutes_before_start = text.trim().parse().ok();
                        }
                        "isOnlineMeeting" => {
                            item.is_online_meeting = Some(matches!(text.trim(), "true" | "1"));
                        }
                        "joinUrl" => item.join_online_meeting_url = Some(text),
                        "netShowUrl" => item.net_show_url = Some(text),
                        "workspaceUrl" => item.meeting_workspace_url = Some(text),
                        "category" => item.categories.push(text),
                        "created" => item.created = Some(text),
                        "lastModified" => item.last_modified = Some(text),
                        "bodyText" => item.body_text = Some(text),
                        "bodyHtml" => item.body_html = Some(text),
                        "occStart" => {
                            if let Some(occ) = occurrence_stack.last_mut() {
                                occ.start = Some(text.clone());
                            }
                            if let Some(occ) = deleted_stack.last_mut() {
                                occ.start = Some(text);
                            }
                        }
                        "occEnd" => {
                            if let Some(occ) = occurrence_stack.last_mut() {
                                occ.end = Some(text);
                            }
                        }
                        "occOrig" => {
                            if let Some(occ) = occurrence_stack.last_mut() {
                                occ.original_start = Some(text);
                            }
                        }
                        _ => {}
                    }
                }
                cur.clear();
                text_target = None;
                match lower.as_str() {
                    "modifiedoccurrences" => in_modified = false,
                    "deletedoccurrences" => in_deleted = false,
                    "occurrence" => {
                        if let Some(occ) = occurrence_stack.pop() {
                            item.modified_occurrences.push(occ);
                        }
                    }
                    "deletedoccurrence" => {
                        if let Some(occ) = deleted_stack.pop() {
                            item.deleted_occurrences.push(occ);
                        }
                    }
                    "attendee" => {
                        if let Some(att) = current_attendee.take() {
                            match attendee_kind {
                                Some("required") => item.required_attendees.push(att),
                                Some("optional") => item.optional_attendees.push(att),
                                Some("resource") => item.resources.push(att),
                                _ => {}
                            }
                        }
                    }
                    "requiredattendees" | "optionalattendees" | "resources" => attendee_kind = None,
                    "organizer" => in_organizer = false,
                    "mailbox" => in_mailbox = false,
                    "categories" => category_collecting = false,
                    _ => {}
                }
            }
            Event::Text(ref t) => {
                let text = t.as_ref().to_owned();
                if !recurrence_path.is_empty() {
                    match recurrence_text {
                        Some("interval") => pending.interval = text.trim().parse().unwrap_or(1),
                        Some("daysOfWeek") => {
                            for word in text.split_whitespace() {
                                pending.days_of_week.push(word.to_owned());
                            }
                        }
                        Some("dayOfMonth") => {
                            pending.day_of_month = text.trim().parse().unwrap_or(0);
                        }
                        Some("dayOfWeekIndex") => {
                            pending.day_of_week_index = text.trim().to_owned()
                        }
                        Some("month") => pending.month = text.trim().to_owned(),
                        Some("startDate") => pending.start_date = text.trim().to_owned(),
                        Some("endDate") => pending.end_date = text.trim().to_owned(),
                        Some("numberOfOccurrences") => {
                            pending.number_of_occurrences = text.trim().parse().unwrap_or(0);
                        }
                        _ => {}
                    }
                } else {
                    cur.push_str(&text);
                }
            }
            Event::CData(ref c) => {
                if recurrence_path.is_empty() {
                    cur.push_str(c);
                }
            }
            Event::GeneralRef(ref g) => {
                if recurrence_path.is_empty()
                    && let Some(c) = entity_to_char(g)
                {
                    cur.push(c);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(item)
}

#[derive(Default)]
struct PendingRecurrence {
    pattern_choice: Option<&'static str>,
    range_choice: Option<&'static str>,
    interval: u32,
    days_of_week: Vec<String>,
    day_of_month: u32,
    day_of_week_index: String,
    month: String,
    start_date: String,
    end_date: String,
    number_of_occurrences: u32,
}

impl PendingRecurrence {
    fn finalize(self, out: &mut RawRecurrence) {
        let interval = if self.interval == 0 { 1 } else { self.interval };
        out.pattern = match self.pattern_choice {
            Some("Daily") => Some(RecurrencePattern::Daily { interval }),
            Some("Weekly") => Some(RecurrencePattern::Weekly {
                interval,
                days_of_week: self.days_of_week.clone(),
            }),
            Some("AbsoluteMonthly") => Some(RecurrencePattern::AbsoluteMonthly {
                interval,
                day_of_month: self.day_of_month,
            }),
            Some("RelativeMonthly") => Some(RecurrencePattern::RelativeMonthly {
                interval,
                day_of_week_index: self.day_of_week_index.clone(),
                days_of_week: self.days_of_week.clone(),
            }),
            Some("AbsoluteYearly") => Some(RecurrencePattern::AbsoluteYearly {
                month: self.month.clone(),
                day_of_month: self.day_of_month,
            }),
            Some("RelativeYearly") => Some(RecurrencePattern::RelativeYearly {
                month: self.month.clone(),
                day_of_week_index: self.day_of_week_index.clone(),
                days_of_week: self.days_of_week.clone(),
            }),
            _ => None,
        };
        out.range = match self.range_choice {
            Some("NoEnd") => Some(RecurrenceRange::NoEnd {
                start_date: self.start_date,
            }),
            Some("EndDate") => Some(RecurrenceRange::EndDate {
                start_date: self.start_date,
                end_date: self.end_date,
            }),
            Some("Numbered") => Some(RecurrenceRange::Numbered {
                start_date: self.start_date,
                number_of_occurrences: self.number_of_occurrences,
            }),
            _ => None,
        };
    }
}

#[derive(Debug, Clone, Default)]
pub struct ContactItemRaw {
    pub id: ItemId,
    pub parent_folder_id: Option<String>,
    pub display_name: Option<String>,
    pub given_name: Option<String>,
    pub middle_name: Option<String>,
    pub surname: Option<String>,
    pub initials: Option<String>,
    pub nickname: Option<String>,
    pub company_name: Option<String>,
    pub department: Option<String>,
    pub job_title: Option<String>,
    pub generation: Option<String>,
    pub office_location: Option<String>,
    pub url: Option<String>,
    pub birthday: Option<String>,
    pub wedding_anniversary: Option<String>,
    pub notes: Option<String>,
    pub categories: Vec<String>,
    pub emails: Vec<(String, String)>,
    pub phones: Vec<(String, String)>,
    pub addresses: Vec<RawContactAddress>,
    pub ims: Vec<(String, String)>,
    pub manager: Option<String>,
    pub spouse: Option<String>,
    pub assistant: Option<String>,
    pub children: Vec<String>,
    pub companies: Vec<String>,
    pub profession: Option<String>,
    pub postal_address_index: Option<String>,
    pub created: Option<String>,
    pub last_modified: Option<String>,
    pub attachments: Vec<RawAttachmentRef>,
    pub is_group: bool,
    pub members: Vec<RawGroupMember>,
}

#[derive(Debug, Clone, Default)]
pub struct RawGroupMember {
    pub name: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RawContactAddress {
    pub key: String,
    pub street: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub country: Option<String>,
    pub postal_code: Option<String>,
}

pub fn parse_contact_item(inner_xml: &str) -> Result<ContactItemRaw, EwsError> {
    let mut xml = NsReader::from_reader(inner_xml.as_bytes());
    xml.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut item = ContactItemRaw::default();
    let mut text_target: Option<&'static str> = None;
    let mut category_collecting = false;
    let mut children_collecting = false;
    let mut companies_collecting = false;
    let mut entry_container: Option<&'static str> = None;
    let mut entry_key: Option<String> = None;
    let mut current_address: Option<RawContactAddress> = None;
    let mut address_text: Option<&'static str> = None;
    let mut seen_root_contact = false;
    let mut in_members = false;
    let mut member_mailbox = false;
    let mut current_member: Option<RawGroupMember> = None;
    let mut cur = String::new();
    loop {
        buf.clear();
        let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
        let ns_kind = classify(&ns);
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let local = e.local_name().as_ref().to_owned();
                let is_empty = matches!(ev, Event::Empty(_));
                if !seen_root_contact
                    && (local.eq_ignore_ascii_case("Contact")
                        || local.eq_ignore_ascii_case("DistributionList"))
                {
                    seen_root_contact = true;
                    if local.eq_ignore_ascii_case("DistributionList") {
                        item.is_group = true;
                    }
                    continue;
                }
                if ns_kind != Ns::Types {
                    continue;
                }
                cur.clear();
                if local.eq_ignore_ascii_case("ItemId") {
                    capture_id_attrs(e, &mut item.id.id, &mut item.id.change_key);
                } else if local.eq_ignore_ascii_case("ParentFolderId") {
                    let mut pid = String::new();
                    let mut ck = String::new();
                    capture_id_attrs(e, &mut pid, &mut ck);
                    if !pid.is_empty() {
                        item.parent_folder_id = Some(pid);
                    }
                } else if local.eq_ignore_ascii_case("DisplayName") {
                    text_target = Some("displayName");
                } else if local.eq_ignore_ascii_case("GivenName") {
                    text_target = Some("givenName");
                } else if local.eq_ignore_ascii_case("MiddleName") {
                    text_target = Some("middleName");
                } else if local.eq_ignore_ascii_case("Surname") {
                    text_target = Some("surname");
                } else if local.eq_ignore_ascii_case("Initials") {
                    text_target = Some("initials");
                } else if local.eq_ignore_ascii_case("Nickname") {
                    text_target = Some("nickname");
                } else if local.eq_ignore_ascii_case("CompanyName") {
                    text_target = Some("companyName");
                } else if local.eq_ignore_ascii_case("Department") {
                    text_target = Some("department");
                } else if local.eq_ignore_ascii_case("JobTitle") {
                    text_target = Some("jobTitle");
                } else if local.eq_ignore_ascii_case("Generation") {
                    text_target = Some("generation");
                } else if local.eq_ignore_ascii_case("OfficeLocation") {
                    text_target = Some("officeLocation");
                } else if local.eq_ignore_ascii_case("BusinessHomePage") {
                    text_target = Some("url");
                } else if local.eq_ignore_ascii_case("Birthday") {
                    text_target = Some("birthday");
                } else if local.eq_ignore_ascii_case("WeddingAnniversary") {
                    text_target = Some("weddingAnniversary");
                } else if local.eq_ignore_ascii_case("Manager") {
                    text_target = Some("manager");
                } else if local.eq_ignore_ascii_case("SpouseName") {
                    text_target = Some("spouse");
                } else if local.eq_ignore_ascii_case("AssistantName") {
                    text_target = Some("assistant");
                } else if local.eq_ignore_ascii_case("Profession") {
                    text_target = Some("profession");
                } else if local.eq_ignore_ascii_case("PostalAddressIndex") {
                    text_target = Some("postalAddressIndex");
                } else if local.eq_ignore_ascii_case("Members") {
                    in_members = true;
                } else if in_members && local.eq_ignore_ascii_case("Member") {
                    current_member = Some(RawGroupMember::default());
                } else if in_members && local.eq_ignore_ascii_case("Mailbox") {
                    member_mailbox = true;
                } else if member_mailbox && local.eq_ignore_ascii_case("Name") {
                    text_target = Some("memberName");
                } else if member_mailbox && local.eq_ignore_ascii_case("EmailAddress") {
                    text_target = Some("memberEmail");
                } else if local.eq_ignore_ascii_case("Body") {
                    text_target = Some("notes");
                } else if local.eq_ignore_ascii_case("DateTimeCreated") {
                    text_target = Some("created");
                } else if local.eq_ignore_ascii_case("LastModifiedTime") {
                    text_target = Some("lastModified");
                } else if local.eq_ignore_ascii_case("Categories") {
                    category_collecting = true;
                } else if category_collecting && local.eq_ignore_ascii_case("String") {
                    text_target = Some("category");
                } else if local.eq_ignore_ascii_case("Children") {
                    children_collecting = true;
                } else if children_collecting && local.eq_ignore_ascii_case("String") {
                    text_target = Some("child");
                } else if local.eq_ignore_ascii_case("Companies") {
                    companies_collecting = true;
                } else if companies_collecting && local.eq_ignore_ascii_case("String") {
                    text_target = Some("company");
                } else if local.eq_ignore_ascii_case("EmailAddresses") {
                    entry_container = Some("email");
                } else if local.eq_ignore_ascii_case("PhoneNumbers") {
                    entry_container = Some("phone");
                } else if local.eq_ignore_ascii_case("ImAddresses") {
                    entry_container = Some("im");
                } else if local.eq_ignore_ascii_case("PhysicalAddresses") {
                    entry_container = Some("address");
                } else if local.eq_ignore_ascii_case("Entry") {
                    entry_key = attr_value(e, "Key");
                    if entry_container == Some("address") {
                        current_address = Some(RawContactAddress {
                            key: entry_key.clone().unwrap_or_default(),
                            ..RawContactAddress::default()
                        });
                    } else {
                        text_target = Some("entryValue");
                    }
                } else if current_address.is_some() {
                    if local.eq_ignore_ascii_case("Street") {
                        address_text = Some("street");
                    } else if local.eq_ignore_ascii_case("City") {
                        address_text = Some("city");
                    } else if local.eq_ignore_ascii_case("State") {
                        address_text = Some("state");
                    } else if local.eq_ignore_ascii_case("CountryOrRegion") {
                        address_text = Some("country");
                    } else if local.eq_ignore_ascii_case("PostalCode") {
                        address_text = Some("postal");
                    } else {
                        address_text = None;
                    }
                } else if local.eq_ignore_ascii_case("FileAttachment")
                    || local.eq_ignore_ascii_case("ItemAttachment")
                {
                    let att = parse_attachment_ref(&mut xml, &local)?;
                    item.attachments.push(att);
                } else {
                    text_target = None;
                }
                if is_empty {
                    text_target = None;
                    address_text = None;
                }
            }
            Event::End(e) => {
                let local = e.local_name().as_ref().to_owned();
                let lower = local.to_ascii_lowercase();
                if let Some(at) = address_text {
                    if let Some(addr) = current_address.as_mut() {
                        let text = std::mem::take(&mut cur);
                        match at {
                            "street" => addr.street = Some(text),
                            "city" => addr.city = Some(text),
                            "state" => addr.state = Some(text),
                            "country" => addr.country = Some(text),
                            "postal" => addr.postal_code = Some(text),
                            _ => {}
                        }
                    }
                } else if let Some(tt) = text_target {
                    let text = std::mem::take(&mut cur);
                    match tt {
                        "displayName" => item.display_name = Some(text),
                        "givenName" => item.given_name = Some(text),
                        "middleName" => item.middle_name = Some(text),
                        "surname" => item.surname = Some(text),
                        "initials" => item.initials = Some(text),
                        "nickname" => item.nickname = Some(text),
                        "companyName" => item.company_name = Some(text),
                        "department" => item.department = Some(text),
                        "jobTitle" => item.job_title = Some(text),
                        "generation" => item.generation = Some(text),
                        "officeLocation" => item.office_location = Some(text),
                        "url" => item.url = Some(text),
                        "birthday" => item.birthday = Some(text),
                        "weddingAnniversary" => item.wedding_anniversary = Some(text),
                        "manager" => item.manager = Some(text),
                        "spouse" => item.spouse = Some(text),
                        "assistant" => item.assistant = Some(text),
                        "profession" => item.profession = Some(text),
                        "postalAddressIndex" => item.postal_address_index = Some(text),
                        "notes" => item.notes = Some(text),
                        "created" => item.created = Some(text),
                        "lastModified" => item.last_modified = Some(text),
                        "memberName" => {
                            if let Some(m) = current_member.as_mut() {
                                m.name = Some(text);
                            }
                        }
                        "memberEmail" => {
                            if let Some(m) = current_member.as_mut() {
                                m.email = Some(text);
                            }
                        }
                        "category" => item.categories.push(text),
                        "child" => item.children.push(text),
                        "company" => item.companies.push(text),
                        "entryValue" => {
                            let key = entry_key.clone().unwrap_or_default();
                            match entry_container {
                                Some("email") => item.emails.push((key, text)),
                                Some("phone") => item.phones.push((key, text)),
                                Some("im") => item.ims.push((key, text)),
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
                cur.clear();
                text_target = None;
                address_text = None;
                match lower.as_str() {
                    "categories" => category_collecting = false,
                    "children" => children_collecting = false,
                    "companies" => companies_collecting = false,
                    "emailaddresses" | "phonenumbers" | "imaddresses" | "physicaladdresses" => {
                        entry_container = None
                    }
                    "entry" => {
                        if let Some(addr) = current_address.take() {
                            item.addresses.push(addr);
                        }
                        entry_key = None;
                    }
                    "members" => in_members = false,
                    "mailbox" => member_mailbox = false,
                    "member" => {
                        if let Some(m) = current_member.take()
                            && (m.email.is_some() || m.name.is_some())
                        {
                            item.members.push(m);
                        }
                    }
                    _ => {}
                }
            }
            Event::Text(ref t) => {
                cur.push_str(t);
            }
            Event::CData(ref c) => {
                cur.push_str(c);
            }
            Event::GeneralRef(ref g) => {
                if let Some(c) = entity_to_char(g) {
                    cur.push(c);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(item)
}

fn parse_attachment_ref<R: BufRead>(
    xml: &mut NsReader<R>,
    element_local: &str,
) -> Result<RawAttachmentRef, EwsError> {
    let mut att = RawAttachmentRef {
        is_item_attachment: element_local.eq_ignore_ascii_case("itemattachment"),
        ..RawAttachmentRef::default()
    };
    let mut buf = Vec::new();
    let mut depth: u32 = 1;
    let mut current: Option<&'static str> = None;
    let mut cur = String::new();
    loop {
        buf.clear();
        let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
        let ns_kind = classify(&ns);
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let is_empty = matches!(ev, Event::Empty(_));
                if !is_empty {
                    depth += 1;
                }
                let local = e.local_name().as_ref().to_owned();
                if ns_kind == Ns::Types {
                    cur.clear();
                    if local.eq_ignore_ascii_case("AttachmentId") {
                        if let Some(v) = attr_value(e, "Id") {
                            att.attachment_id = v;
                        }
                    } else if local.eq_ignore_ascii_case("Name") {
                        current = Some("name");
                    } else if local.eq_ignore_ascii_case("ContentType") {
                        current = Some("contentType");
                    } else if local.eq_ignore_ascii_case("IsContactPhoto") {
                        current = Some("isContactPhoto");
                    } else {
                        current = None;
                    }
                }
                if is_empty {
                    current = None;
                }
            }
            Event::End(_) => {
                if let Some(field) = current.take() {
                    let text = std::mem::take(&mut cur);
                    match field {
                        "name" => att.name = Some(text),
                        "contentType" => att.content_type = Some(text),
                        "isContactPhoto" => {
                            att.is_contact_photo = matches!(text.trim(), "true" | "1");
                        }
                        _ => {}
                    }
                }
                cur.clear();
                if depth == 0 {
                    break;
                }
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Event::Text(ref t) => {
                cur.push_str(t);
            }
            Event::CData(ref c) => {
                cur.push_str(c);
            }
            Event::GeneralRef(ref g) => {
                if let Some(c) = entity_to_char(g) {
                    cur.push(c);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(att)
}

#[derive(Debug, Clone, Default)]
pub struct GetAttachmentInline {
    pub attachment_id: String,
    pub name: Option<String>,
    pub content_type: Option<String>,
    pub is_contact_photo: bool,
    pub content_base64: String,
}

pub fn parse_get_attachment_inline(body: &[u8]) -> Result<Vec<GetAttachmentInline>, EwsError> {
    let mut xml = NsReader::from_reader(body);
    xml.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out: Vec<GetAttachmentInline> = Vec::new();
    let mut current: Option<GetAttachmentInline> = None;
    let mut text_target: Option<&'static str> = None;
    loop {
        buf.clear();
        let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
        let ns_kind = classify(&ns);
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let local = e.local_name().as_ref().to_owned();
                let is_empty = matches!(ev, Event::Empty(_));
                if ns_kind == Ns::Types
                    && (local.eq_ignore_ascii_case("FileAttachment")
                        || local.eq_ignore_ascii_case("ItemAttachment"))
                {
                    current = Some(GetAttachmentInline::default());
                } else if let Some(cur) = current.as_mut()
                    && ns_kind == Ns::Types
                {
                    if local.eq_ignore_ascii_case("AttachmentId") {
                        if let Some(v) = attr_value(e, "Id") {
                            cur.attachment_id = v;
                        }
                    } else if local.eq_ignore_ascii_case("Name") {
                        text_target = Some("name");
                    } else if local.eq_ignore_ascii_case("ContentType") {
                        text_target = Some("contentType");
                    } else if local.eq_ignore_ascii_case("IsContactPhoto") {
                        text_target = Some("isContactPhoto");
                    } else if local.eq_ignore_ascii_case("Content") {
                        text_target = Some("content");
                    } else {
                        text_target = None;
                    }
                }
                if is_empty {
                    text_target = None;
                }
            }
            Event::End(e) => {
                let local = e.local_name().as_ref().to_owned();
                if (local.eq_ignore_ascii_case("FileAttachment")
                    || local.eq_ignore_ascii_case("ItemAttachment"))
                    && let Some(att) = current.take()
                {
                    out.push(att);
                }
                text_target = None;
            }
            Event::Text(_) | Event::CData(_) => {
                let text = match ev {
                    Event::Text(ref t) => t.as_ref().to_owned(),
                    Event::CData(ref c) => c.as_ref().to_owned(),
                    _ => unreachable!(),
                };

                if let Some(cur) = current.as_mut() {
                    match text_target {
                        Some("name") => {
                            cur.name.get_or_insert_with(String::new).push_str(&text);
                        }
                        Some("contentType") => {
                            cur.content_type
                                .get_or_insert_with(String::new)
                                .push_str(&text);
                        }
                        Some("isContactPhoto") => {
                            cur.is_contact_photo = matches!(text.trim(), "true" | "1");
                        }
                        Some("content") => cur.content_base64.push_str(&text),
                        _ => {}
                    }
                }
            }
            Event::GeneralRef(ref g) => {
                if let (Some(cur), Some(c)) = (current.as_mut(), entity_to_char(g)) {
                    match text_target {
                        Some("name") => cur.name.get_or_insert_with(String::new).push(c),
                        Some("contentType") => {
                            cur.content_type.get_or_insert_with(String::new).push(c)
                        }
                        Some("content") => cur.content_base64.push(c),
                        _ => {}
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: &str = " xmlns:soap=\"http://schemas.xmlsoap.org/soap/envelope/\" \
                      xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\" \
                      xmlns:m=\"http://schemas.microsoft.com/exchange/services/2006/messages\"";

    #[test]
    fn server_busy_fault_extracts_backoff_milliseconds() {
        let body = format!(
            "<soap:Envelope{NS}><soap:Header><t:ServerVersionInfo MajorVersion=\"15\" MinorVersion=\"1\"/></soap:Header><soap:Body>\
             <soap:Fault><faultcode>soap:Server</faultcode><faultstring>busy</faultstring><detail>\
             <ResponseCode xmlns=\"http://schemas.microsoft.com/exchange/services/2006/types\">ErrorServerBusy</ResponseCode>\
             <t:MessageXml><t:Value Name=\"BackOffMilliseconds\">297749</t:Value></t:MessageXml>\
             </detail></soap:Fault></soap:Body></soap:Envelope>"
        );
        let env = read_envelope_summary(body.as_bytes()).unwrap();
        match env {
            EnvelopeKind::Fault { fault, version } => {
                assert!(matches!(
                    fault.response_code,
                    ResponseCode::ServerBusy {
                        back_off_ms: Some(297749)
                    }
                ));
                assert_eq!(fault.back_off_ms, Some(297749));
                assert_eq!(
                    version.to_server_version(),
                    Some(ServerVersion::Exchange2016)
                );
            }
            _ => panic!("expected fault"),
        }
    }

    #[test]
    fn body_envelope_returns_version_only() {
        let body = format!(
            "<soap:Envelope{NS}><soap:Header><t:ServerVersionInfo MajorVersion=\"15\" MinorVersion=\"1\"/></soap:Header>\
             <soap:Body><m:FindFolderResponse/></soap:Body></soap:Envelope>"
        );
        let env = read_envelope_summary(body.as_bytes()).unwrap();
        assert!(matches!(env, EnvelopeKind::Body { .. }));
    }

    #[test]
    fn find_folder_parses_two_folders() {
        let body = format!(
            "<m:FindFolderResponse{NS}><m:ResponseMessages><m:FindFolderResponseMessage ResponseClass=\"Success\">\
             <m:ResponseCode>NoError</m:ResponseCode>\
             <m:RootFolder TotalItemsInView=\"2\" IncludesLastItemInRange=\"true\">\
             <t:Folders>\
             <t:Folder><t:FolderId Id=\"F1\" ChangeKey=\"C1\"/><t:ParentFolderId Id=\"ROOT\"/>\
               <t:FolderClass>IPF.Note</t:FolderClass><t:DisplayName>Inbox</t:DisplayName>\
               <t:TotalCount>7</t:TotalCount><t:ChildFolderCount>0</t:ChildFolderCount></t:Folder>\
             <t:CalendarFolder><t:FolderId Id=\"F2\"/><t:ParentFolderId Id=\"ROOT\"/>\
               <t:FolderClass>IPF.Appointment</t:FolderClass><t:DisplayName>Calendar</t:DisplayName>\
               </t:CalendarFolder>\
             </t:Folders></m:RootFolder></m:FindFolderResponseMessage></m:ResponseMessages></m:FindFolderResponse>"
        );
        let r = parse_find_folder_response(body.as_bytes()).unwrap();
        assert_eq!(r.folders.len(), 2);
        assert_eq!(r.folders[0].folder_id.id, "F1");
        assert_eq!(r.folders[0].folder_id.change_key, "C1");
        assert_eq!(r.folders[0].display_name, "Inbox");
        assert_eq!(r.folders[0].folder_class, "IPF.Note");
        assert_eq!(r.folders[0].parent_id.as_deref(), Some("ROOT"));
        assert_eq!(r.folders[1].element, FolderElement::CalendarFolder);
        assert!(!r.more);
    }

    #[test]
    fn find_item_parses_two_messages_with_pagination_flag() {
        let body = format!(
            "<m:FindItemResponse{NS}><m:ResponseMessages><m:FindItemResponseMessage ResponseClass=\"Success\">\
             <m:ResponseCode>NoError</m:ResponseCode>\
             <m:RootFolder TotalItemsInView=\"3\" IncludesLastItemInRange=\"false\">\
             <t:Items>\
               <t:Message><t:ItemId Id=\"I1\" ChangeKey=\"CK1\"/></t:Message>\
               <t:Message><t:ItemId Id=\"I2\" ChangeKey=\"CK2\"/></t:Message>\
             </t:Items></m:RootFolder></m:FindItemResponseMessage></m:ResponseMessages></m:FindItemResponse>"
        );
        let r = parse_find_item_response(body.as_bytes()).unwrap();
        assert_eq!(r.items.len(), 2);
        assert_eq!(r.items[0].id.id, "I1");
        assert_eq!(r.items[0].id.change_key, "CK1");
        assert_eq!(r.items[1].id.id, "I2");
        assert!(r.more);
        assert_eq!(r.total_in_view, Some(3));
    }

    #[test]
    fn parse_response_messages_carries_inner_xml_and_codes() {
        let body = format!(
            "<m:GetItemResponse{NS}><m:ResponseMessages>\
             <m:GetItemResponseMessage ResponseClass=\"Success\">\
               <m:ResponseCode>NoError</m:ResponseCode>\
               <m:Items><t:Message><t:ItemId Id=\"X1\" ChangeKey=\"K1\"/>\
                 <t:Subject>Hi</t:Subject><t:IsRead>true</t:IsRead>\
                 <t:MimeContent CharacterSet=\"UTF-8\">SGVsbG8=</t:MimeContent>\
                 </t:Message></m:Items></m:GetItemResponseMessage>\
             <m:GetItemResponseMessage ResponseClass=\"Error\">\
               <m:ResponseCode>ErrorItemNotFound</m:ResponseCode>\
               <m:MessageText>not found</m:MessageText>\
               </m:GetItemResponseMessage>\
             </m:ResponseMessages></m:GetItemResponse>"
        );
        let r = parse_response_messages(body.as_bytes(), "GetItemResponseMessage").unwrap();
        assert_eq!(r.len(), 2);
        assert!(r[0].success);
        assert!(r[0].inner_xml.contains("<t:ItemId"));
        assert!(r[0].inner_xml.contains("SGVsbG8="));
        let parsed = parse_message_item(&r[0].inner_xml).unwrap();
        assert_eq!(parsed.id.id, "X1");
        assert_eq!(parsed.subject.as_deref(), Some("Hi"));
        assert_eq!(parsed.is_read, Some(true));
        assert_eq!(parsed.mime_content.as_deref(), Some("SGVsbG8="));
        assert!(!r[1].success);
        assert!(matches!(r[1].response_code, ResponseCode::ItemNotFound));
    }

    #[test]
    fn pid_tag_flag_status_extended_property_marks_message_flagged() {
        fn parse_one(message_xml: &str) -> MessageItem {
            let body = format!(
                "<m:GetItemResponse{NS}><m:ResponseMessages>\
                 <m:GetItemResponseMessage ResponseClass=\"Success\">\
                   <m:ResponseCode>NoError</m:ResponseCode>\
                   <m:Items>{message_xml}</m:Items>\
                 </m:GetItemResponseMessage></m:ResponseMessages></m:GetItemResponse>"
            );
            let r = parse_response_messages(body.as_bytes(), "GetItemResponseMessage").unwrap();
            parse_message_item(&r[0].inner_xml).unwrap()
        }

        let flagged = parse_one(
            "<t:Message><t:ItemId Id=\"F1\" ChangeKey=\"K\"/>\
             <t:Subject>Flagged</t:Subject>\
             <t:ExtendedProperty>\
               <t:ExtendedFieldURI PropertyTag=\"0x1090\" PropertyType=\"Integer\"/>\
               <t:Value>2</t:Value>\
             </t:ExtendedProperty>\
             <t:IsRead>true</t:IsRead></t:Message>",
        );
        assert_eq!(flagged.flag_status.as_deref(), Some("Flagged"));
        assert_eq!(flagged.is_read, Some(true));

        let cleared = parse_one(
            "<t:Message><t:ItemId Id=\"F2\" ChangeKey=\"K\"/>\
             <t:ExtendedProperty>\
               <t:ExtendedFieldURI PropertyTag=\"0x1090\" PropertyType=\"Integer\"/>\
               <t:Value>0</t:Value>\
             </t:ExtendedProperty></t:Message>",
        );
        assert_eq!(cleared.flag_status, None);
    }

    #[test]
    fn native_item_flag_still_parses_on_exchange_2013_plus() {
        let body = format!(
            "<m:GetItemResponse{NS}><m:ResponseMessages>\
             <m:GetItemResponseMessage ResponseClass=\"Success\">\
               <m:ResponseCode>NoError</m:ResponseCode>\
               <m:Items><t:Message><t:ItemId Id=\"M\" ChangeKey=\"K\"/>\
                 <t:Subject>Native</t:Subject>\
                 <t:Flag><t:FlagStatus>Flagged</t:FlagStatus></t:Flag>\
                 <t:IsRead>false</t:IsRead></t:Message></m:Items>\
             </m:GetItemResponseMessage></m:ResponseMessages></m:GetItemResponse>"
        );
        let r = parse_response_messages(body.as_bytes(), "GetItemResponseMessage").unwrap();
        let p = parse_message_item(&r[0].inner_xml).unwrap();
        assert_eq!(
            p.flag_status.as_deref(),
            Some("Flagged"),
            "the 2013+ native item:Flag path must be unaffected by the pre-2013 extended-property fallback"
        );
        assert_eq!(p.is_read, Some(false));
    }

    #[test]
    fn sync_folder_items_emits_create_delete_readflag() {
        let body = format!(
            "<m:SyncFolderItemsResponse{NS}><m:ResponseMessages><m:SyncFolderItemsResponseMessage ResponseClass=\"Success\">\
             <m:ResponseCode>NoError</m:ResponseCode>\
             <m:SyncState>AAAA</m:SyncState>\
             <m:IncludesLastItemInRange>true</m:IncludesLastItemInRange>\
             <m:Changes>\
               <t:Create><t:Message><t:ItemId Id=\"I1\" ChangeKey=\"K1\"/></t:Message></t:Create>\
               <t:Delete><t:ItemId Id=\"I2\"/></t:Delete>\
               <t:ReadFlagChange><t:ItemId Id=\"I3\"/><t:IsRead>true</t:IsRead></t:ReadFlagChange>\
             </m:Changes></m:SyncFolderItemsResponseMessage></m:ResponseMessages></m:SyncFolderItemsResponse>"
        );
        let r = parse_sync_folder_items_response(body.as_bytes()).unwrap();
        assert_eq!(r.sync_state, "AAAA");
        assert!(!r.more);
        assert_eq!(r.changes.len(), 3);
        match &r.changes[0] {
            SyncChange::Create { id, element } => {
                assert_eq!(id.id, "I1");
                assert_eq!(element, "Message");
            }
            _ => panic!("expected Create"),
        }
        match &r.changes[2] {
            SyncChange::ReadFlagChange { id, is_read } => {
                assert_eq!(id.id, "I3");
                assert!(*is_read);
            }
            _ => panic!("expected ReadFlagChange"),
        }
    }

    #[test]
    fn calendar_master_recurrence_and_occurrences_round_trip() {
        let body = format!(
            "<t:CalendarItem{NS}>\
             <t:ItemId Id=\"M1\" ChangeKey=\"K1\"/>\
             <t:Subject>Daily</t:Subject>\
             <t:UID>uid-1</t:UID>\
             <t:Start>2025-06-15T14:00:00Z</t:Start>\
             <t:End>2025-06-15T15:00:00Z</t:End>\
             <t:CalendarItemType>RecurringMaster</t:CalendarItemType>\
             <t:Recurrence>\
               <t:DailyRecurrence><t:Interval>1</t:Interval></t:DailyRecurrence>\
               <t:NumberedRecurrence><t:StartDate>2025-06-15</t:StartDate><t:NumberOfOccurrences>3</t:NumberOfOccurrences></t:NumberedRecurrence>\
             </t:Recurrence>\
             <t:ModifiedOccurrences>\
               <t:Occurrence><t:ItemId Id=\"OCC1\"/><t:Start>2025-06-16T15:00:00Z</t:Start><t:End>2025-06-16T16:30:00Z</t:End><t:OriginalStart>2025-06-16T14:00:00Z</t:OriginalStart></t:Occurrence>\
             </t:ModifiedOccurrences>\
             <t:DeletedOccurrences>\
               <t:DeletedOccurrence><t:Start>2025-06-17T14:00:00Z</t:Start></t:DeletedOccurrence>\
             </t:DeletedOccurrences>\
             </t:CalendarItem>"
        );
        let parsed = parse_calendar_item(&body).unwrap();
        assert_eq!(parsed.id.id, "M1");
        assert_eq!(parsed.uid.as_deref(), Some("uid-1"));
        assert_eq!(
            parsed.calendar_item_type,
            Some(CalendarItemType::RecurringMaster)
        );
        assert_eq!(parsed.modified_occurrences.len(), 1);
        assert_eq!(
            parsed.modified_occurrences[0].original_start.as_deref(),
            Some("2025-06-16T14:00:00Z")
        );
        assert_eq!(parsed.deleted_occurrences.len(), 1);
        assert_eq!(
            parsed.deleted_occurrences[0].start.as_deref(),
            Some("2025-06-17T14:00:00Z")
        );
        let rec = parsed.recurrence.unwrap();
        assert!(matches!(
            rec.pattern,
            Some(RecurrencePattern::Daily { interval: 1 })
        ));
        assert!(matches!(
            rec.range,
            Some(RecurrenceRange::Numbered {
                number_of_occurrences: 3,
                ..
            })
        ));
    }

    #[test]
    fn end_date_recurrence_wire_shape_with_trailing_z_yields_clean_until() {
        let body = format!(
            "<t:CalendarItem{NS}><t:ItemId Id=\"M\" ChangeKey=\"K\"/>\
             <t:Subject>RecDailyEnd</t:Subject><t:UID>u</t:UID>\
             <t:Start>2026-07-01T15:00:00Z</t:Start><t:End>2026-07-01T15:30:00Z</t:End>\
             <t:CalendarItemType>RecurringMaster</t:CalendarItemType>\
             <t:Recurrence>\
               <t:DailyRecurrence><t:Interval>2</t:Interval></t:DailyRecurrence>\
               <t:EndDateRecurrence><t:StartDate>2026-07-01Z</t:StartDate><t:EndDate>2026-08-01Z</t:EndDate></t:EndDateRecurrence>\
             </t:Recurrence></t:CalendarItem>"
        );
        let parsed = parse_calendar_item(&body).unwrap();
        let rule = crate::exchange_ews::recurrence::to_jscalendar_rule(
            parsed.recurrence.as_ref().unwrap(),
        )
        .unwrap();
        assert_eq!(rule["frequency"], "daily");
        assert_eq!(rule["interval"], 2);
        assert_eq!(
            rule["until"], "2026-08-01T23:59:59",
            "trailing Z on EndDate must be stripped, not embedded mid-string"
        );
    }

    #[test]
    fn absolute_and_relative_recurrence_wire_shapes_round_trip() {
        let cases = [
            (
                "<t:AbsoluteMonthlyRecurrence><t:Interval>1</t:Interval><t:DayOfMonth>15</t:DayOfMonth></t:AbsoluteMonthlyRecurrence><t:NoEndRecurrence><t:StartDate>2026-07-01Z</t:StartDate></t:NoEndRecurrence>",
                "monthly",
            ),
            (
                "<t:RelativeMonthlyRecurrence><t:Interval>1</t:Interval><t:DaysOfWeek>Friday</t:DaysOfWeek><t:DayOfWeekIndex>Last</t:DayOfWeekIndex></t:RelativeMonthlyRecurrence><t:NoEndRecurrence><t:StartDate>2026-07-01Z</t:StartDate></t:NoEndRecurrence>",
                "monthly",
            ),
            (
                "<t:AbsoluteYearlyRecurrence><t:DayOfMonth>1</t:DayOfMonth><t:Month>January</t:Month></t:AbsoluteYearlyRecurrence><t:NoEndRecurrence><t:StartDate>2026-07-01Z</t:StartDate></t:NoEndRecurrence>",
                "yearly",
            ),
            (
                "<t:RelativeYearlyRecurrence><t:DaysOfWeek>Monday</t:DaysOfWeek><t:DayOfWeekIndex>First</t:DayOfWeekIndex><t:Month>September</t:Month></t:RelativeYearlyRecurrence><t:NoEndRecurrence><t:StartDate>2026-07-01Z</t:StartDate></t:NoEndRecurrence>",
                "yearly",
            ),
        ];
        for (rec_xml, freq) in cases {
            let body = format!(
                "<t:CalendarItem{NS}><t:ItemId Id=\"M\" ChangeKey=\"K\"/><t:UID>u</t:UID>\
                 <t:Start>2026-07-01T15:00:00Z</t:Start><t:End>2026-07-01T15:30:00Z</t:End>\
                 <t:Recurrence>{rec_xml}</t:Recurrence></t:CalendarItem>"
            );
            let parsed = parse_calendar_item(&body).unwrap();
            let rule = crate::exchange_ews::recurrence::to_jscalendar_rule(
                parsed.recurrence.as_ref().unwrap(),
            )
            .unwrap();
            assert_eq!(rule["frequency"], freq, "for {rec_xml}");
        }
        let body = format!(
            "<t:CalendarItem{NS}><t:ItemId Id=\"M\" ChangeKey=\"K\"/><t:UID>u</t:UID>\
             <t:Start>2026-07-01T15:00:00Z</t:Start><t:End>2026-07-01T15:30:00Z</t:End>\
             <t:Recurrence><t:RelativeMonthlyRecurrence><t:Interval>1</t:Interval>\
             <t:DaysOfWeek>Friday</t:DaysOfWeek><t:DayOfWeekIndex>Last</t:DayOfWeekIndex></t:RelativeMonthlyRecurrence>\
             <t:NoEndRecurrence><t:StartDate>2026-07-01Z</t:StartDate></t:NoEndRecurrence></t:Recurrence></t:CalendarItem>"
        );
        let parsed = parse_calendar_item(&body).unwrap();
        let rule = crate::exchange_ews::recurrence::to_jscalendar_rule(
            parsed.recurrence.as_ref().unwrap(),
        )
        .unwrap();
        assert_eq!(rule["byDay"][0]["day"], "fr");
        assert_eq!(rule["byDay"][0]["nthOfPeriod"], -1);
    }

    #[test]
    fn attendee_and_organizer_routing_type_is_parsed() {
        let body = format!(
            "<t:CalendarItem{NS}><t:ItemId Id=\"M\" ChangeKey=\"K\"/><t:UID>u</t:UID>\
             <t:Organizer><t:Mailbox><t:Name>Org</t:Name>\
               <t:EmailAddress>/o=ExLabs/cn=org</t:EmailAddress><t:RoutingType>EX</t:RoutingType></t:Mailbox></t:Organizer>\
             <t:RequiredAttendees><t:Attendee><t:Mailbox><t:Name>Att</t:Name>\
               <t:EmailAddress>/o=ExLabs/cn=att</t:EmailAddress><t:RoutingType>EX</t:RoutingType></t:Mailbox>\
               <t:ResponseType>Accept</t:ResponseType></t:Attendee></t:RequiredAttendees>\
             </t:CalendarItem>"
        );
        let parsed = parse_calendar_item(&body).unwrap();
        assert_eq!(parsed.organizer_routing_type.as_deref(), Some("EX"));
        assert_eq!(parsed.required_attendees.len(), 1);
        assert_eq!(
            parsed.required_attendees[0].routing_type.as_deref(),
            Some("EX")
        );
        assert_eq!(
            parsed.required_attendees[0].email.as_deref(),
            Some("/o=ExLabs/cn=att")
        );
    }

    #[test]
    fn calendar_parses_reminder_resources_and_online_meeting() {
        let body = format!(
            "<t:CalendarItem{NS}>\
             <t:ItemId Id=\"C1\" ChangeKey=\"K1\"/>\
             <t:Subject>Standup</t:Subject>\
             <t:UID>uid-r</t:UID>\
             <t:ReminderIsSet>true</t:ReminderIsSet>\
             <t:ReminderMinutesBeforeStart>10</t:ReminderMinutesBeforeStart>\
             <t:Start>2025-06-15T14:00:00Z</t:Start>\
             <t:End>2025-06-15T15:00:00Z</t:End>\
             <t:IsOnlineMeeting>true</t:IsOnlineMeeting>\
             <t:NetShowUrl>https://meet/abc</t:NetShowUrl>\
             <t:RequiredAttendees>\
               <t:Attendee><t:Mailbox><t:Name>Bob</t:Name><t:EmailAddress>bob@x</t:EmailAddress></t:Mailbox><t:ResponseType>Accept</t:ResponseType></t:Attendee>\
             </t:RequiredAttendees>\
             <t:Resources>\
               <t:Attendee><t:Mailbox><t:Name>Room 7</t:Name><t:EmailAddress>room7@x</t:EmailAddress></t:Mailbox></t:Attendee>\
             </t:Resources>\
             </t:CalendarItem>"
        );
        let parsed = parse_calendar_item(&body).unwrap();
        assert_eq!(parsed.reminder_is_set, Some(true));
        assert_eq!(parsed.reminder_minutes_before_start, Some(10));
        assert_eq!(parsed.is_online_meeting, Some(true));
        assert_eq!(parsed.net_show_url.as_deref(), Some("https://meet/abc"));
        assert_eq!(parsed.required_attendees.len(), 1);
        assert_eq!(parsed.required_attendees[0].email.as_deref(), Some("bob@x"));
        assert_eq!(parsed.resources.len(), 1);
        assert_eq!(parsed.resources[0].email.as_deref(), Some("room7@x"));
    }

    #[test]
    fn distribution_list_parses_members() {
        let body = format!(
            "<t:DistributionList{NS}>\
             <t:ItemId Id=\"DL1\" ChangeKey=\"K1\"/>\
             <t:DisplayName>The Team</t:DisplayName>\
             <t:Members>\
               <t:Member><t:Mailbox><t:Name>Bob</t:Name><t:EmailAddress>bob@x</t:EmailAddress></t:Mailbox><t:Status>Normal</t:Status></t:Member>\
               <t:Member><t:Mailbox><t:Name>Carol</t:Name><t:EmailAddress>carol@x</t:EmailAddress></t:Mailbox></t:Member>\
             </t:Members>\
             </t:DistributionList>"
        );
        let parsed = parse_contact_item(&body).unwrap();
        assert!(parsed.is_group);
        assert_eq!(parsed.id.id, "DL1");
        assert_eq!(parsed.display_name.as_deref(), Some("The Team"));
        assert_eq!(parsed.members.len(), 2);
        assert_eq!(parsed.members[0].email.as_deref(), Some("bob@x"));
        assert_eq!(parsed.members[1].name.as_deref(), Some("Carol"));
    }

    #[test]
    fn contact_decodes_xml_entities() {
        let body = format!(
            "<t:Contact{NS}>\
             <t:ItemId Id=\"C1\" ChangeKey=\"K1\"/>\
             <t:Categories><t:String>R&amp;D</t:String></t:Categories>\
             <t:DisplayName>Sales &amp; Marketing &lt;Team&gt;</t:DisplayName>\
             <t:CompanyName>AT&amp;T</t:CompanyName>\
             <t:Department>R&amp;D</t:Department>\
             <t:EmailAddresses><t:Entry Key=\"EmailAddress1\">a&amp;b@x.com</t:Entry></t:EmailAddresses>\
             </t:Contact>"
        );
        let parsed = parse_contact_item(&body).unwrap();
        assert_eq!(
            parsed.display_name.as_deref(),
            Some("Sales & Marketing <Team>")
        );
        assert_eq!(parsed.company_name.as_deref(), Some("AT&T"));
        assert_eq!(parsed.department.as_deref(), Some("R&D"));
        assert_eq!(parsed.categories, vec!["R&D".to_owned()]);
        assert_eq!(parsed.emails[0].1, "a&b@x.com");
    }

    #[test]
    fn calendar_decodes_xml_entities_in_subject_and_location() {
        let body = format!(
            "<t:CalendarItem{NS}>\
             <t:ItemId Id=\"E1\" ChangeKey=\"K\"/>\
             <t:Subject>Q&amp;A &lt;all hands&gt;</t:Subject>\
             <t:Location>Room A &amp; B</t:Location>\
             <t:Start>2025-06-15T14:00:00Z</t:Start>\
             <t:End>2025-06-15T15:00:00Z</t:End>\
             </t:CalendarItem>"
        );
        let parsed = parse_calendar_item(&body).unwrap();
        assert_eq!(parsed.subject.as_deref(), Some("Q&A <all hands>"));
        assert_eq!(parsed.location.as_deref(), Some("Room A & B"));
    }

    #[test]
    fn folder_decodes_xml_entities_in_display_name() {
        let body = format!(
            "<t:Folder{NS}><t:FolderId Id=\"F1\" ChangeKey=\"K\"/>\
             <t:DisplayName>Sales &amp; Marketing</t:DisplayName>\
             <t:FolderClass>IPF.Note</t:FolderClass></t:Folder>"
        );
        let entry = parse_folder_inner(&body).unwrap().unwrap();
        assert_eq!(entry.display_name, "Sales & Marketing");
    }

    #[test]
    fn response_capture_preserves_entities_for_reparse() {
        let body = format!(
            "<m:GetItemResponse{NS}><m:ResponseMessages><m:GetItemResponseMessage ResponseClass=\"Success\">\
             <m:ResponseCode>NoError</m:ResponseCode><m:Items>\
             <t:Contact><t:ItemId Id=\"C1\" ChangeKey=\"K\"/><t:CompanyName>AT&amp;T</t:CompanyName></t:Contact>\
             </m:Items></m:GetItemResponseMessage></m:ResponseMessages></m:GetItemResponse>"
        );
        let msgs = parse_response_messages(body.as_bytes(), "GetItemResponseMessage").unwrap();
        assert!(
            msgs[0].inner_xml.contains("AT&amp;T") || msgs[0].inner_xml.contains("AT&T"),
            "capture must preserve the entity for the per-item parser: {}",
            msgs[0].inner_xml
        );
        let parsed = parse_contact_item(&msgs[0].inner_xml).unwrap();
        assert_eq!(parsed.company_name.as_deref(), Some("AT&T"));
    }

    #[test]
    fn contact_parses_emails_phones_addresses() {
        let body = format!(
            "<t:Contact{NS}>\
             <t:ItemId Id=\"C1\" ChangeKey=\"K1\"/>\
             <t:DisplayName>Alice Doe</t:DisplayName>\
             <t:GivenName>Alice</t:GivenName>\
             <t:Surname>Doe</t:Surname>\
             <t:EmailAddresses>\
               <t:Entry Key=\"EmailAddress1\">alice@example.com</t:Entry>\
             </t:EmailAddresses>\
             <t:PhoneNumbers>\
               <t:Entry Key=\"BusinessPhone1\">+15551234</t:Entry>\
             </t:PhoneNumbers>\
             <t:PhysicalAddresses>\
               <t:Entry Key=\"Home\"><t:Street>1 Main</t:Street><t:City>Town</t:City><t:CountryOrRegion>US</t:CountryOrRegion></t:Entry>\
             </t:PhysicalAddresses>\
             </t:Contact>"
        );
        let parsed = parse_contact_item(&body).unwrap();
        assert_eq!(parsed.given_name.as_deref(), Some("Alice"));
        assert_eq!(parsed.surname.as_deref(), Some("Doe"));
        assert_eq!(parsed.emails.len(), 1);
        assert_eq!(parsed.emails[0].0, "EmailAddress1");
        assert_eq!(parsed.emails[0].1, "alice@example.com");
        assert_eq!(parsed.phones.len(), 1);
        assert_eq!(parsed.phones[0].0, "BusinessPhone1");
        assert_eq!(parsed.addresses.len(), 1);
        assert_eq!(parsed.addresses[0].key, "Home");
        assert_eq!(parsed.addresses[0].street.as_deref(), Some("1 Main"));
        assert_eq!(parsed.addresses[0].country.as_deref(), Some("US"));
    }

    #[test]
    fn get_item_with_unprefixed_types_elements_parses() {
        let body = format!(
            "<m:GetItemResponse{NS}><m:ResponseMessages>\
             <m:GetItemResponseMessage ResponseClass=\"Success\">\
             <m:ResponseCode>NoError</m:ResponseCode>\
             <m:Items xmlns=\"http://schemas.microsoft.com/exchange/services/2006/types\">\
             <Message><ItemId Id=\"NP1\" ChangeKey=\"K\"/>\
             <Subject>Hello</Subject>\
             <MimeContent CharacterSet=\"UTF-8\">SGk=</MimeContent>\
             </Message></m:Items></m:GetItemResponseMessage>\
             </m:ResponseMessages></m:GetItemResponse>"
        );
        let msgs = parse_response_messages(body.as_bytes(), "GetItemResponseMessage").unwrap();
        assert_eq!(msgs.len(), 1);
        let item = parse_message_item(&msgs[0].inner_xml).unwrap();
        assert_eq!(item.id.id, "NP1");
        assert_eq!(item.subject.as_deref(), Some("Hello"));
        assert_eq!(item.mime_content.as_deref(), Some("SGk="));
    }

    #[test]
    fn default_namespace_response_parses_via_resolved_names() {
        let body = "<Envelope xmlns=\"http://schemas.xmlsoap.org/soap/envelope/\" \
                    xmlns:m=\"http://schemas.microsoft.com/exchange/services/2006/messages\" \
                    xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\">\
                    <Body>\
                    <m:FindFolderResponse><m:ResponseMessages>\
                    <m:FindFolderResponseMessage ResponseClass=\"Success\">\
                    <m:ResponseCode>NoError</m:ResponseCode>\
                    <m:RootFolder TotalItemsInView=\"1\" IncludesLastItemInRange=\"true\">\
                    <t:Folders><t:Folder><t:FolderId Id=\"D1\"/><t:ParentFolderId Id=\"R\"/>\
                    <t:FolderClass>IPF.Note</t:FolderClass><t:DisplayName>Inbox</t:DisplayName>\
                    </t:Folder></t:Folders></m:RootFolder>\
                    </m:FindFolderResponseMessage></m:ResponseMessages></m:FindFolderResponse>\
                    </Body></Envelope>";
        let parsed = parse_find_folder_response(body.as_bytes()).unwrap();
        assert_eq!(parsed.folders.len(), 1);
        assert_eq!(parsed.folders[0].folder_id.id, "D1");
        assert_eq!(parsed.folders[0].display_name, "Inbox");
    }

    #[test]
    fn fault_response_code_in_messages_namespace_is_recognised() {
        let body = format!(
            "<soap:Envelope{NS}><soap:Body><soap:Fault>\
             <faultcode>soap:Server</faultcode><faultstring>busy</faultstring>\
             <detail><m:ResponseCode>ErrorServerBusy</m:ResponseCode>\
             <t:MessageXml><t:Value Name=\"BackOffMilliseconds\">42</t:Value></t:MessageXml>\
             </detail></soap:Fault></soap:Body></soap:Envelope>"
        );
        let env = read_envelope_summary(body.as_bytes()).unwrap();
        match env {
            EnvelopeKind::Fault { fault, .. } => {
                assert!(matches!(
                    fault.response_code,
                    ResponseCode::ServerBusy { .. }
                ));
                assert_eq!(fault.back_off_ms, Some(42));
            }
            _ => panic!("expected fault"),
        }
    }

    #[test]
    fn change_key_attribute_with_entities_is_xml_decoded() {
        let body = format!(
            "<m:FindFolderResponse{NS}><m:ResponseMessages><m:FindFolderResponseMessage ResponseClass=\"Success\">\
             <m:ResponseCode>NoError</m:ResponseCode>\
             <m:RootFolder TotalItemsInView=\"1\" IncludesLastItemInRange=\"true\">\
             <t:Folders><t:Folder><t:FolderId Id=\"FID\" ChangeKey=\"A&amp;B\"/>\
             <t:DisplayName>X</t:DisplayName></t:Folder></t:Folders>\
             </m:RootFolder></m:FindFolderResponseMessage></m:ResponseMessages></m:FindFolderResponse>"
        );
        let parsed = parse_find_folder_response(body.as_bytes()).unwrap();
        assert_eq!(parsed.folders[0].folder_id.change_key, "A&B");
    }

    #[test]
    fn warning_response_class_is_treated_as_success() {
        let body = format!(
            "<m:GetItemResponse{NS}><m:ResponseMessages>\
             <m:GetItemResponseMessage ResponseClass=\"Warning\">\
             <m:ResponseCode>NoError</m:ResponseCode>\
             <m:Items><t:Message><t:ItemId Id=\"W1\" ChangeKey=\"K\"/></t:Message></m:Items>\
             </m:GetItemResponseMessage></m:ResponseMessages></m:GetItemResponse>"
        );
        let r = parse_response_messages(body.as_bytes(), "GetItemResponseMessage").unwrap();
        assert_eq!(r.len(), 1);
        assert!(r[0].success, "Warning should be success-equivalent");
    }

    #[test]
    fn get_folder_response_message_captures_folder_inner() {
        let body = format!(
            "<m:GetFolderResponse{NS}><m:ResponseMessages>\
             <m:GetFolderResponseMessage ResponseClass=\"Success\">\
             <m:ResponseCode>NoError</m:ResponseCode>\
             <m:Folders><t:Folder><t:FolderId Id=\"F1\"/>\
             <t:DisplayName>Inbox</t:DisplayName></t:Folder></m:Folders>\
             </m:GetFolderResponseMessage>\
             <m:GetFolderResponseMessage ResponseClass=\"Error\">\
             <m:ResponseCode>ErrorAccessDenied</m:ResponseCode></m:GetFolderResponseMessage>\
             </m:ResponseMessages></m:GetFolderResponse>"
        );
        let msgs = parse_response_messages(body.as_bytes(), "GetFolderResponseMessage").unwrap();
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].success);
        let f = parse_folder_inner(&msgs[0].inner_xml).unwrap().unwrap();
        assert_eq!(f.folder_id.id, "F1");
        assert!(!msgs[1].success);
    }

    #[test]
    fn get_attachment_inline_decodes_content_field() {
        let body = format!(
            "<m:GetAttachmentResponse{NS}><m:ResponseMessages>\
             <m:GetAttachmentResponseMessage ResponseClass=\"Success\">\
             <m:ResponseCode>NoError</m:ResponseCode>\
             <m:Attachments>\
               <t:FileAttachment><t:AttachmentId Id=\"A1\"/><t:Name>p.png</t:Name>\
               <t:ContentType>image/png</t:ContentType>\
               <t:IsContactPhoto>true</t:IsContactPhoto>\
               <t:Content>iVBORw0KGgo=</t:Content></t:FileAttachment>\
             </m:Attachments></m:GetAttachmentResponseMessage></m:ResponseMessages></m:GetAttachmentResponse>"
        );
        let items = parse_get_attachment_inline(body.as_bytes()).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].attachment_id, "A1");
        assert_eq!(items[0].name.as_deref(), Some("p.png"));
        assert_eq!(items[0].content_type.as_deref(), Some("image/png"));
        assert!(items[0].is_contact_photo);
        assert_eq!(items[0].content_base64, "iVBORw0KGgo=");
    }
}
