/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod attachment;
mod calendar;
mod contact;
mod envelope;
mod folder;
mod item;
mod message;
mod recurrence;
mod sync;

#[cfg(test)]
mod tests;

use std::mem;

use quick_xml::NsReader;
use quick_xml::XmlVersion;
use quick_xml::events::{BytesRef, BytesStart};
use quick_xml::name::ResolveResult;

use crate::exchange::ews::soap::{NS_MESSAGES, NS_TYPES};
use crate::net::xml::Entity;

pub use attachment::{GetAttachmentInline, RawAttachmentRef};
pub use calendar::{CalendarItemRaw, RawAttendee, RawOccurrence};
pub use contact::{ContactItemRaw, RawContactAddress, RawGroupMember};
pub use envelope::{EnvelopeKind, ServerVersionInfo, SoapFault};
pub use folder::{FindFolderResponse, FolderElement, FolderEntry};
pub use item::{FindItemResponse, ItemEntry, ResponseMessage};
pub use message::MessageItem;
pub use recurrence::{RawRecurrence, RecurrencePattern, RecurrenceRange};
pub use sync::{SyncChange, SyncFolderItemsResponse};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ns {
    Messages,
    Types,
    Other,
}

impl Ns {
    fn of(ns: &ResolveResult<'_>) -> Ns {
        match ns {
            ResolveResult::Bound(uri) if uri.as_ref() == NS_MESSAGES => Ns::Messages,
            ResolveResult::Bound(uri) if uri.as_ref() == NS_TYPES => Ns::Types,
            _ => Ns::Other,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Trim {
    Text,
    Keep,
}

impl Trim {
    fn reader(self, bytes: &[u8]) -> NsReader<&[u8]> {
        let mut xml = NsReader::from_reader(bytes);
        xml.config_mut().trim_text(self == Trim::Text);
        xml
    }
}

#[derive(Clone, Copy)]
struct Attrs<'a, 'b>(&'a BytesStart<'b>);

impl Attrs<'_, '_> {
    fn value(self, name: &str) -> Option<String> {
        self.0
            .attributes()
            .flatten()
            .find(|a| a.key.local_name().into_inner().eq_ignore_ascii_case(name))
            .map(|a| {
                a.normalized_value(XmlVersion::Implicit1_0)
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| a.value.into_owned())
            })
    }

    fn ids(self, id_out: &mut String, ck_out: &mut String) {
        for a in self.0.attributes().flatten() {
            let target = match IdAttr::parse(a.key.local_name().into_inner()) {
                Some(IdAttr::Id) => &mut *id_out,
                Some(IdAttr::ChangeKey) => &mut *ck_out,
                None => continue,
            };
            *target = a
                .normalized_value(XmlVersion::Implicit1_0)
                .map(|c| c.into_owned())
                .unwrap_or_else(|_| a.value.into_owned());
        }
    }

    fn parent_id(self) -> Option<String> {
        let mut id = String::new();
        let mut change_key = String::new();
        self.ids(&mut id, &mut change_key);
        (!id.is_empty()).then_some(id)
    }
}

#[derive(Clone, Copy)]
enum IdAttr {
    Id,
    ChangeKey,
}

impl IdAttr {
    fn parse(name: &str) -> Option<IdAttr> {
        hashify::map_ignore_case!(
            name.as_bytes(),
            IdAttr,
            "Id" => IdAttr::Id,
            "ChangeKey" => IdAttr::ChangeKey,
        )
        .copied()
    }
}

struct XmlBool;

impl XmlBool {
    fn parse(text: &str) -> bool {
        matches!(text.trim(), "true" | "1")
    }
}

struct ItemElement;

impl ItemElement {
    fn is(local: &str) -> bool {
        hashify::set_ignore_case!(
            local.as_bytes(),
            "Item",
            "Message",
            "SharingMessage",
            "CalendarItem",
            "Contact",
            "DistributionList",
            "MeetingMessage",
            "MeetingRequest",
            "MeetingResponse",
            "MeetingCancellation",
            "Task",
            "PostItem",
        )
    }
}

#[derive(Default)]
struct TextBuf(String);

impl TextBuf {
    fn push_ref(&mut self, reference: &BytesRef<'_>) {
        if let Some(c) = Entity::from_ref(reference) {
            self.0.push(c);
        }
    }

    fn take(&mut self) -> String {
        mem::take(&mut self.0)
    }
}

#[derive(Default)]
struct ItemDepth {
    depth: usize,
    item_level: Option<usize>,
}

impl ItemDepth {
    fn open(&mut self, is_empty: bool) -> usize {
        let level = self.depth + 1;
        if !is_empty {
            self.depth = level;
        }
        level
    }

    fn close(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn enter_item(&mut self, level: usize) {
        self.item_level = Some(level);
    }

    fn is_item_child(&self, level: usize) -> bool {
        self.item_level.is_some_and(|item| level == item + 1)
    }
}
