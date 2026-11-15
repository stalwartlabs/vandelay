/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::io::BufRead;

use quick_xml::NsReader;
use quick_xml::events::Event;

use crate::exchange::ews::error::EwsError;
use crate::exchange::ews::parse::{Attrs, Ns, TextBuf, Trim, XmlBool};
use crate::net::xml::Entity;

#[derive(Debug, Clone, Default)]
pub struct RawAttachmentRef {
    pub attachment_id: String,
    pub name: Option<String>,
    pub content_type: Option<String>,
    pub is_contact_photo: bool,
    pub is_item_attachment: bool,
}

#[derive(Debug, Clone, Default)]
pub struct GetAttachmentInline {
    pub attachment_id: String,
    pub name: Option<String>,
    pub content_type: Option<String>,
    pub is_contact_photo: bool,
    pub content_base64: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum AttachmentElement {
    File,
    Item,
}

#[derive(Clone, Copy)]
enum AttachmentField {
    Attachment,
    AttachmentId,
    Name,
    ContentType,
    IsContactPhoto,
    Content,
}

#[derive(Clone, Copy)]
enum AttachmentText {
    Name,
    ContentType,
    IsContactPhoto,
    Content,
}

impl AttachmentField {
    fn parse(local: &str) -> Option<AttachmentField> {
        hashify::map_ignore_case!(
            local.as_bytes(),
            AttachmentField,
            "FileAttachment" => AttachmentField::Attachment,
            "ItemAttachment" => AttachmentField::Attachment,
            "AttachmentId" => AttachmentField::AttachmentId,
            "Name" => AttachmentField::Name,
            "ContentType" => AttachmentField::ContentType,
            "IsContactPhoto" => AttachmentField::IsContactPhoto,
            "Content" => AttachmentField::Content,
        )
        .copied()
    }
}

impl RawAttachmentRef {
    pub(super) fn read<R: BufRead>(
        xml: &mut NsReader<R>,
        element: AttachmentElement,
    ) -> Result<RawAttachmentRef, EwsError> {
        let mut att = RawAttachmentRef {
            is_item_attachment: element == AttachmentElement::Item,
            ..RawAttachmentRef::default()
        };
        let mut buf = Vec::new();
        let mut depth: u32 = 1;
        let mut current: Option<AttachmentText> = None;
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
                        cur.0.clear();
                        match AttachmentField::parse(e.local_name().into_inner()) {
                            Some(AttachmentField::AttachmentId) => {
                                if let Some(v) = Attrs(e).value("Id") {
                                    att.attachment_id = v;
                                }
                            }
                            Some(AttachmentField::Name) => current = Some(AttachmentText::Name),
                            Some(AttachmentField::ContentType) => {
                                current = Some(AttachmentText::ContentType);
                            }
                            Some(AttachmentField::IsContactPhoto) => {
                                current = Some(AttachmentText::IsContactPhoto);
                            }
                            _ => current = None,
                        }
                    }
                    if is_empty {
                        current = None;
                    }
                }
                Event::End(_) => {
                    if let Some(field) = current.take() {
                        let text = cur.take();
                        match field {
                            AttachmentText::Name => att.name = Some(text),
                            AttachmentText::ContentType => att.content_type = Some(text),
                            AttachmentText::IsContactPhoto => {
                                att.is_contact_photo = XmlBool::parse(&text);
                            }
                            AttachmentText::Content => {}
                        }
                    }
                    cur.0.clear();
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Event::Text(ref t) => cur.0.push_str(t),
                Event::CData(ref c) => cur.0.push_str(c),
                Event::GeneralRef(ref g) => cur.push_ref(g),
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(att)
    }
}

impl GetAttachmentInline {
    pub fn parse_all(body: &[u8]) -> Result<Vec<GetAttachmentInline>, EwsError> {
        let mut xml = Trim::Text.reader(body);
        let mut buf = Vec::new();
        let mut out: Vec<GetAttachmentInline> = Vec::new();
        let mut current: Option<GetAttachmentInline> = None;
        let mut text_target: Option<AttachmentText> = None;
        loop {
            buf.clear();
            let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
            match ev {
                Event::Start(ref e) | Event::Empty(ref e) => {
                    let is_empty = matches!(ev, Event::Empty(_));
                    if Ns::of(&ns) == Ns::Types {
                        let field = AttachmentField::parse(e.local_name().into_inner());
                        if let Some(AttachmentField::Attachment) = field {
                            current = Some(GetAttachmentInline::default());
                        } else if let Some(cur) = current.as_mut() {
                            text_target = match field {
                                Some(AttachmentField::AttachmentId) => {
                                    if let Some(v) = Attrs(e).value("Id") {
                                        cur.attachment_id = v;
                                    }
                                    text_target
                                }
                                Some(AttachmentField::Name) => Some(AttachmentText::Name),
                                Some(AttachmentField::ContentType) => {
                                    Some(AttachmentText::ContentType)
                                }
                                Some(AttachmentField::IsContactPhoto) => {
                                    Some(AttachmentText::IsContactPhoto)
                                }
                                Some(AttachmentField::Content) => Some(AttachmentText::Content),
                                _ => None,
                            };
                        }
                    }
                    if is_empty {
                        text_target = None;
                    }
                }
                Event::End(e) => {
                    if let Some(AttachmentField::Attachment) =
                        AttachmentField::parse(e.local_name().into_inner())
                        && let Some(att) = current.take()
                    {
                        out.push(att);
                    }
                    text_target = None;
                }
                Event::Text(ref t) => GetAttachmentInline::push_text(&mut current, text_target, t),
                Event::CData(ref c) => GetAttachmentInline::push_text(&mut current, text_target, c),
                Event::GeneralRef(ref g) => {
                    if let (Some(cur), Some(c)) = (current.as_mut(), Entity::from_ref(g)) {
                        match text_target {
                            Some(AttachmentText::Name) => {
                                cur.name.get_or_insert_with(String::new).push(c);
                            }
                            Some(AttachmentText::ContentType) => {
                                cur.content_type.get_or_insert_with(String::new).push(c);
                            }
                            Some(AttachmentText::Content) => cur.content_base64.push(c),
                            Some(AttachmentText::IsContactPhoto) | None => {}
                        }
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(out)
    }

    fn push_text(
        current: &mut Option<GetAttachmentInline>,
        target: Option<AttachmentText>,
        text: &str,
    ) {
        let Some(cur) = current.as_mut() else {
            return;
        };
        match target {
            Some(AttachmentText::Name) => {
                cur.name.get_or_insert_with(String::new).push_str(text);
            }
            Some(AttachmentText::ContentType) => {
                cur.content_type
                    .get_or_insert_with(String::new)
                    .push_str(text);
            }
            Some(AttachmentText::IsContactPhoto) => cur.is_contact_photo = XmlBool::parse(text),
            Some(AttachmentText::Content) => cur.content_base64.push_str(text),
            None => {}
        }
    }
}
