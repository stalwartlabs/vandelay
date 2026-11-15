/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::io::BufRead;

use quick_xml::NsReader;
use quick_xml::events::{BytesStart, Event};

use crate::exchange::ews::error::EwsError;
use crate::exchange::ews::parse::{Attrs, Ns, TextBuf, Trim, XmlBool};
use crate::exchange::ews::types::FolderId;

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
    pub is_hidden: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderElement {
    Folder,
    CalendarFolder,
    ContactsFolder,
    TasksFolder,
    SearchFolder,
}

#[derive(Clone, Copy)]
enum FolderField {
    FolderId,
    ParentFolderId,
    DisplayName,
    FolderClass,
    TotalCount,
    ChildFolderCount,
    ExtendedFieldUri,
    Value,
}

#[derive(Clone, Copy)]
enum FolderText {
    DisplayName,
    FolderClass,
    TotalCount,
    ChildCount,
    Hidden,
}

impl FolderElement {
    fn from_local(local: &str) -> Option<FolderElement> {
        hashify::map_ignore_case!(
            local.as_bytes(),
            FolderElement,
            "Folder" => FolderElement::Folder,
            "CalendarFolder" => FolderElement::CalendarFolder,
            "ContactsFolder" => FolderElement::ContactsFolder,
            "TasksFolder" => FolderElement::TasksFolder,
            "SearchFolder" => FolderElement::SearchFolder,
        )
        .copied()
    }

    fn of_types(ns: Ns, e: &BytesStart<'_>) -> Option<FolderElement> {
        if ns == Ns::Types {
            FolderElement::from_local(e.local_name().into_inner())
        } else {
            None
        }
    }
}

impl FindFolderResponse {
    pub fn parse(body: &[u8]) -> Result<FindFolderResponse, EwsError> {
        let mut xml = Trim::Text.reader(body);
        let mut buf = Vec::new();
        let mut out = FindFolderResponse::default();
        loop {
            buf.clear();
            let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
            let ns = Ns::of(&ns);
            match ev {
                Event::Start(ref e) | Event::Empty(ref e) => {
                    if ns == Ns::Messages
                        && e.local_name()
                            .into_inner()
                            .eq_ignore_ascii_case("RootFolder")
                    {
                        let attrs = Attrs(e);
                        if let Some(v) = attrs.value("TotalItemsInView") {
                            out.total_in_view = v.trim().parse().ok();
                        }
                        if let Some(v) = attrs.value("IncludesLastItemInRange") {
                            out.more = !XmlBool::parse(&v);
                        }
                    } else if let Some(elem) = FolderElement::of_types(ns, e) {
                        out.folders.push(FolderEntry::read(&mut xml, elem)?);
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(out)
    }
}

impl Default for FolderEntry {
    fn default() -> Self {
        FolderEntry::new(FolderElement::Folder)
    }
}

impl FolderEntry {
    fn new(element: FolderElement) -> FolderEntry {
        FolderEntry {
            element,
            folder_id: FolderId {
                id: String::new(),
                change_key: String::new(),
            },
            parent_id: None,
            display_name: String::new(),
            folder_class: String::new(),
            total_count: None,
            child_count: None,
            is_hidden: false,
        }
    }

    pub fn parse_inner(inner_xml: &str) -> Result<Option<FolderEntry>, EwsError> {
        let mut xml = Trim::Text.reader(inner_xml.as_bytes());
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
            match ev {
                Event::Start(ref e) | Event::Empty(ref e) => {
                    if let Some(elem) = FolderElement::of_types(Ns::of(&ns), e) {
                        return Ok(Some(FolderEntry::read(&mut xml, elem)?));
                    }
                }
                Event::Eof => return Ok(None),
                _ => {}
            }
        }
    }

    pub fn parse_get_response(body: &[u8]) -> Result<Vec<FolderEntry>, EwsError> {
        let mut xml = Trim::Text.reader(body);
        let mut buf = Vec::new();
        let mut out: Vec<FolderEntry> = Vec::new();
        loop {
            buf.clear();
            let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
            match ev {
                Event::Start(ref e) | Event::Empty(ref e) => {
                    if let Some(elem) = FolderElement::of_types(Ns::of(&ns), e) {
                        out.push(FolderEntry::read(&mut xml, elem)?);
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(out)
    }

    fn read<R: BufRead>(
        xml: &mut NsReader<R>,
        element: FolderElement,
    ) -> Result<FolderEntry, EwsError> {
        xml.config_mut().trim_text(false);
        let mut entry = FolderEntry::new(element);
        let mut buf = Vec::new();
        let mut depth: u32 = 1;
        let mut current: Option<FolderText> = None;
        let mut in_hidden_ext = false;
        let mut cur = TextBuf::default();
        loop {
            buf.clear();
            let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
            match ev {
                Event::Start(ref e) | Event::Empty(ref e) => {
                    let is_empty = matches!(ev, Event::Empty(_));
                    if !is_empty {
                        depth += 1;
                    }
                    if Ns::of(&ns) == Ns::Types {
                        entry.open_field(e, &mut in_hidden_ext, &mut current, &mut cur);
                    }
                    if is_empty {
                        current = None;
                    }
                }
                Event::End(_) => {
                    if let Some(field) = current.take() {
                        let text = cur.take();
                        let text = text.trim();
                        match field {
                            FolderText::DisplayName => entry.display_name = text.to_owned(),
                            FolderText::FolderClass => entry.folder_class = text.to_owned(),
                            FolderText::TotalCount => entry.total_count = text.parse().ok(),
                            FolderText::ChildCount => entry.child_count = text.parse().ok(),
                            FolderText::Hidden => entry.is_hidden = XmlBool::parse(text),
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
                Event::Text(t) => cur.0.push_str(&t),
                Event::CData(c) => cur.0.push_str(&c),
                Event::GeneralRef(ref g) => cur.push_ref(g),
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(entry)
    }

    fn open_field(
        &mut self,
        e: &BytesStart<'_>,
        in_hidden_ext: &mut bool,
        current: &mut Option<FolderText>,
        cur: &mut TextBuf,
    ) {
        let field = hashify::map_ignore_case!(
            e.local_name().into_inner().as_bytes(),
            FolderField,
            "FolderId" => FolderField::FolderId,
            "ParentFolderId" => FolderField::ParentFolderId,
            "DisplayName" => FolderField::DisplayName,
            "FolderClass" => FolderField::FolderClass,
            "TotalCount" => FolderField::TotalCount,
            "ChildFolderCount" => FolderField::ChildFolderCount,
            "ExtendedFieldURI" => FolderField::ExtendedFieldUri,
            "Value" => FolderField::Value,
        )
        .copied();
        let text = match field {
            Some(FolderField::FolderId) => {
                return Attrs(e).ids(&mut self.folder_id.id, &mut self.folder_id.change_key);
            }
            Some(FolderField::ParentFolderId) => {
                if let Some(pid) = Attrs(e).parent_id() {
                    self.parent_id = Some(pid);
                }
                return;
            }
            Some(FolderField::ExtendedFieldUri) => {
                *in_hidden_ext = Attrs(e).value("PropertyTag").is_some_and(|tag| {
                    let tag = tag.trim();
                    tag.eq_ignore_ascii_case("0x10f4") || tag == "4340"
                });
                return;
            }
            Some(FolderField::DisplayName) => FolderText::DisplayName,
            Some(FolderField::FolderClass) => FolderText::FolderClass,
            Some(FolderField::TotalCount) => FolderText::TotalCount,
            Some(FolderField::ChildFolderCount) => FolderText::ChildCount,
            Some(FolderField::Value) if *in_hidden_ext => FolderText::Hidden,
            _ => {
                *current = None;
                return;
            }
        };
        *current = Some(text);
        cur.0.clear();
    }
}
