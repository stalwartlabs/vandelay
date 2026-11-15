/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use quick_xml::events::Event;

use crate::exchange::ews::error::EwsError;
use crate::exchange::ews::parse::{Attrs, ItemDepth, ItemElement, Ns, TextBuf, Trim, XmlBool};
use crate::exchange::ews::types::ItemId;

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
    pub categories: Vec<String>,
    pub flag_status: Option<String>,
}

#[derive(Clone, Copy)]
enum MessageField {
    ItemId,
    ParentFolderId,
    MimeContent,
    Subject,
    DateTimeReceived,
    IsRead,
    IsDraft,
    Categories,
    String,
    Flag,
    FlagStatus,
    ExtendedFieldUri,
    Value,
    ExtendedProperty,
}

#[derive(Clone, Copy)]
enum MessageText {
    MimeContent,
    Subject,
    Received,
    IsRead,
    IsDraft,
    Category,
    FlagStatus,
    FlagStatusExt,
}

impl MessageField {
    fn parse(local: &str) -> Option<MessageField> {
        hashify::map_ignore_case!(
            local.as_bytes(),
            MessageField,
            "ItemId" => MessageField::ItemId,
            "ParentFolderId" => MessageField::ParentFolderId,
            "MimeContent" => MessageField::MimeContent,
            "Subject" => MessageField::Subject,
            "DateTimeReceived" => MessageField::DateTimeReceived,
            "IsRead" => MessageField::IsRead,
            "IsDraft" => MessageField::IsDraft,
            "Categories" => MessageField::Categories,
            "String" => MessageField::String,
            "Flag" => MessageField::Flag,
            "FlagStatus" => MessageField::FlagStatus,
            "ExtendedFieldURI" => MessageField::ExtendedFieldUri,
            "Value" => MessageField::Value,
            "ExtendedProperty" => MessageField::ExtendedProperty,
        )
        .copied()
    }
}

impl MessageItem {
    pub fn parse(inner_xml: &str) -> Result<MessageItem, EwsError> {
        let mut xml = Trim::Keep.reader(inner_xml.as_bytes());
        let mut buf = Vec::new();
        let mut item = MessageItem::default();
        let mut current: Option<MessageText> = None;
        let mut mime_charset: Option<String> = None;
        let mut category_collecting = false;
        let mut in_flag = false;
        let mut in_flagstatus_ext = false;
        let mut depth = ItemDepth::default();
        let mut cur = TextBuf::default();
        loop {
            buf.clear();
            let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
            match ev {
                Event::Start(ref e) | Event::Empty(ref e) => {
                    let local = e.local_name().into_inner();
                    let is_empty = matches!(ev, Event::Empty(_));
                    let level = depth.open(is_empty);
                    if item.element.is_empty() && ItemElement::is(local) {
                        local.clone_into(&mut item.element);
                        depth.enter_item(level);
                    } else if Ns::of(&ns) == Ns::Types {
                        cur.0.clear();
                        match MessageField::parse(local) {
                            Some(MessageField::ItemId) => {
                                if depth.is_item_child(level) {
                                    Attrs(e).ids(&mut item.id.id, &mut item.id.change_key);
                                }
                            }
                            Some(MessageField::ParentFolderId) => {
                                if let Some(pid) = Attrs(e).parent_id() {
                                    item.parent_folder_id = Some(pid);
                                }
                            }
                            Some(MessageField::MimeContent) => {
                                mime_charset = Attrs(e).value("CharacterSet");
                                current = Some(MessageText::MimeContent);
                            }
                            Some(MessageField::Subject) => current = Some(MessageText::Subject),
                            Some(MessageField::DateTimeReceived) => {
                                current = Some(MessageText::Received);
                            }
                            Some(MessageField::IsRead) => current = Some(MessageText::IsRead),
                            Some(MessageField::IsDraft) => current = Some(MessageText::IsDraft),
                            Some(MessageField::Categories) => category_collecting = true,
                            Some(MessageField::String) if category_collecting => {
                                current = Some(MessageText::Category);
                            }
                            Some(MessageField::Flag) => in_flag = true,
                            Some(MessageField::FlagStatus) if in_flag => {
                                current = Some(MessageText::FlagStatus);
                            }
                            Some(MessageField::ExtendedFieldUri) => {
                                if let Some(tag) = Attrs(e).value("PropertyTag") {
                                    let tag = tag.trim();
                                    in_flagstatus_ext =
                                        tag.eq_ignore_ascii_case("0x1090") || tag == "4240";
                                }
                            }
                            Some(MessageField::Value) if in_flagstatus_ext => {
                                current = Some(MessageText::FlagStatusExt);
                            }
                            _ => current = None,
                        }
                    }
                    if is_empty {
                        current = None;
                    }
                }
                Event::End(e) => {
                    depth.close();
                    if let Some(field) = current.take() {
                        let text = cur.take();
                        match field {
                            MessageText::MimeContent => {
                                item.mime_content = Some(text);
                                item.mime_charset = mime_charset.take();
                            }
                            MessageText::Subject => item.subject = Some(text),
                            MessageText::Received => item.date_time_received = Some(text),
                            MessageText::IsRead => item.is_read = Some(XmlBool::parse(&text)),
                            MessageText::IsDraft => item.is_draft = Some(XmlBool::parse(&text)),
                            MessageText::Category => item.categories.push(text),
                            MessageText::FlagStatus => item.flag_status = Some(text),
                            MessageText::FlagStatusExt if text.trim() == "2" => {
                                item.flag_status = Some("Flagged".to_owned());
                            }
                            MessageText::FlagStatusExt => {}
                        }
                    }
                    cur.0.clear();
                    match MessageField::parse(e.local_name().into_inner()) {
                        Some(MessageField::Categories) => category_collecting = false,
                        Some(MessageField::Flag) => in_flag = false,
                        Some(MessageField::ExtendedProperty) => in_flagstatus_ext = false,
                        _ => {}
                    }
                }
                Event::Text(ref t) => cur.0.push_str(t),
                Event::CData(ref c) => cur.0.push_str(c),
                Event::GeneralRef(ref g) => cur.push_ref(g),
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(item)
    }
}
