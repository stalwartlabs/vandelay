/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::io::BufRead;

use quick_xml::NsReader;
use quick_xml::events::{BytesEnd, BytesStart, Event};

use crate::exchange::ews::error::EwsError;
use crate::exchange::ews::parse::{Attrs, ItemDepth, Ns, Trim, XmlBool};
use crate::exchange::ews::soap::{NS_MESSAGES, NS_TYPES};
use crate::exchange::ews::types::{ItemId, ResponseCode};

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
    pub item_class: Option<String>,
}

#[derive(Clone, Copy)]
enum FindItemElement {
    RootFolder,
    Items,
    ItemId,
    ItemClass,
}

impl FindItemElement {
    fn parse(local: &str) -> Option<FindItemElement> {
        hashify::map_ignore_case!(
            local.as_bytes(),
            FindItemElement,
            "RootFolder" => FindItemElement::RootFolder,
            "Items" => FindItemElement::Items,
            "ItemId" => FindItemElement::ItemId,
            "ItemClass" => FindItemElement::ItemClass,
        )
        .copied()
    }
}

impl FindItemResponse {
    pub fn parse(body: &[u8]) -> Result<FindItemResponse, EwsError> {
        let mut xml = Trim::Text.reader(body);
        let mut buf = Vec::new();
        let mut out = FindItemResponse::default();
        let mut in_root = false;
        let mut depth = ItemDepth::default();
        let mut items_level: Option<usize> = None;
        let mut reading_class = false;
        loop {
            buf.clear();
            let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
            let ns = Ns::of(&ns);
            match ev {
                Event::Start(ref e) | Event::Empty(ref e) => {
                    let is_empty = matches!(ev, Event::Empty(_));
                    let level = depth.open(is_empty);
                    let local = e.local_name().into_inner();
                    let element = FindItemElement::parse(local);
                    if ns == Ns::Messages && matches!(element, Some(FindItemElement::RootFolder)) {
                        in_root = true;
                        let attrs = Attrs(e);
                        if let Some(v) = attrs.value("TotalItemsInView") {
                            out.total_in_view = v.trim().parse().ok();
                        }
                        if let Some(v) = attrs.value("IncludesLastItemInRange") {
                            out.more = !XmlBool::parse(&v);
                        }
                    } else if in_root && ns == Ns::Types {
                        if items_level.is_none() && matches!(element, Some(FindItemElement::Items))
                        {
                            items_level = Some(level);
                        } else if items_level.is_some_and(|items| level == items + 1) {
                            depth.enter_item(level);
                            out.items.push(ItemEntry {
                                element: local.to_owned(),
                                id: ItemId::default(),
                                item_class: None,
                            });
                        } else if depth.is_item_child(level) {
                            match element {
                                Some(FindItemElement::ItemId) => {
                                    if let Some(last) = out.items.last_mut() {
                                        Attrs(e).ids(&mut last.id.id, &mut last.id.change_key);
                                    }
                                }
                                Some(FindItemElement::ItemClass) => reading_class = !is_empty,
                                _ => {}
                            }
                        }
                    }
                }
                Event::Text(ref t) if reading_class => {
                    if let Some(last) = out.items.last_mut() {
                        last.item_class = Some(t.trim().to_owned());
                    }
                }
                Event::End(e) => {
                    depth.close();
                    reading_class = false;
                    if matches!(
                        FindItemElement::parse(e.local_name().into_inner()),
                        Some(FindItemElement::RootFolder)
                    ) {
                        in_root = false;
                        items_level = None;
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(out)
    }
}

#[derive(Debug, Clone)]
pub struct ResponseMessage {
    pub success: bool,
    pub response_code: ResponseCode,
    pub message_text: String,
    pub inner_xml: String,
}

#[derive(Clone, Copy)]
enum MessageElement {
    ResponseCode,
    MessageText,
    Capture,
}

impl MessageElement {
    fn parse(local: &str) -> Option<MessageElement> {
        hashify::map_ignore_case!(
            local.as_bytes(),
            MessageElement,
            "ResponseCode" => MessageElement::ResponseCode,
            "MessageText" => MessageElement::MessageText,
            "Items" => MessageElement::Capture,
            "Attachments" => MessageElement::Capture,
            "Folders" => MessageElement::Capture,
        )
        .copied()
    }
}

#[derive(Clone, Copy)]
enum MessageText {
    ResponseCode,
    MessageText,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Escape {
    Attribute,
    Text,
}

#[derive(Default)]
struct InnerXml(String);

impl InnerXml {
    const OPEN: &str = "<vandelay-inner xmlns=\"";
    const CLOSE: &str = "</vandelay-inner>";

    fn push_escaped(&mut self, text: &str, escape: Escape) {
        for ch in text.chars() {
            match ch {
                '&' => self.0.push_str("&amp;"),
                '<' => self.0.push_str("&lt;"),
                '>' => self.0.push_str("&gt;"),
                '"' if escape == Escape::Attribute => self.0.push_str("&quot;"),
                _ => self.0.push(ch),
            }
        }
    }

    fn push_tag(&mut self, e: &BytesStart<'_>) {
        self.0.push('<');
        self.0.push_str(e.name().into_inner());
        for a in e.attributes().flatten() {
            self.0.push(' ');
            self.0.push_str(a.key.into_inner());
            self.0.push_str("=\"");
            self.push_escaped(&a.value, Escape::Attribute);
            self.0.push('"');
        }
    }

    fn start(&mut self, e: &BytesStart<'_>) {
        self.push_tag(e);
        self.0.push('>');
    }

    fn empty(&mut self, e: &BytesStart<'_>) {
        self.push_tag(e);
        self.0.push_str("/>");
    }

    fn end(&mut self, e: &BytesEnd<'_>) {
        self.0.push_str("</");
        self.0.push_str(e.name().into_inner());
        self.0.push('>');
    }

    fn text(&mut self, text: &str) {
        self.push_escaped(text, Escape::Text);
    }

    fn reference(&mut self, name: &str) {
        self.0.push('&');
        self.0.push_str(name);
        self.0.push(';');
    }

    fn finish(self) -> String {
        if self.0.is_empty() {
            return self.0;
        }
        let parts = [
            InnerXml::OPEN,
            NS_TYPES,
            "\" xmlns:t=\"",
            NS_TYPES,
            "\" xmlns:m=\"",
            NS_MESSAGES,
            "\">",
            &self.0,
            InnerXml::CLOSE,
        ];
        let mut out = String::with_capacity(parts.iter().map(|p| p.len()).sum());
        parts.iter().for_each(|p| out.push_str(p));
        out
    }
}

impl ResponseMessage {
    pub fn parse_all(
        body: &[u8],
        response_message_local: &str,
    ) -> Result<Vec<ResponseMessage>, EwsError> {
        let mut xml = Trim::Keep.reader(body);
        let mut buf = Vec::new();
        let mut out: Vec<ResponseMessage> = Vec::new();
        loop {
            buf.clear();
            let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
            match ev {
                Event::Start(e) => {
                    if Ns::of(&ns) == Ns::Messages
                        && e.local_name()
                            .into_inner()
                            .eq_ignore_ascii_case(response_message_local)
                    {
                        let success = Attrs(&e)
                            .value("ResponseClass")
                            .is_none_or(|class| matches!(class.as_str(), "Success" | "Warning"));
                        let mut msg = ResponseMessage::read(&mut xml)?;
                        msg.success = success;
                        out.push(msg);
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(out)
    }

    fn read<R: BufRead>(xml: &mut NsReader<R>) -> Result<ResponseMessage, EwsError> {
        let mut buf = Vec::new();
        let mut response_code = ResponseCode::NoError;
        let mut message_text = String::new();
        let mut inner = InnerXml::default();
        let mut depth: u32 = 1;
        let mut current: Option<MessageText> = None;
        let mut capture_depth: u32 = 0;
        loop {
            buf.clear();
            let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
            let in_capture = capture_depth > 0;
            match ev {
                Event::Start(e) => {
                    depth += 1;
                    if in_capture {
                        inner.start(&e);
                        capture_depth += 1;
                    } else {
                        let element = if Ns::of(&ns) == Ns::Messages {
                            MessageElement::parse(e.local_name().into_inner())
                        } else {
                            None
                        };
                        current = match element {
                            Some(MessageElement::ResponseCode) => Some(MessageText::ResponseCode),
                            Some(MessageElement::MessageText) => Some(MessageText::MessageText),
                            Some(MessageElement::Capture) => {
                                capture_depth = 1;
                                None
                            }
                            None => None,
                        };
                    }
                }
                Event::Empty(e) => {
                    if in_capture {
                        inner.empty(&e);
                    }
                }
                Event::End(e) => {
                    if in_capture {
                        if capture_depth == 1 {
                            capture_depth = 0;
                        } else {
                            inner.end(&e);
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
                    if in_capture {
                        inner.text(&t);
                    } else {
                        match current {
                            Some(MessageText::ResponseCode) => {
                                response_code = ResponseCode::parse(t.trim());
                            }
                            Some(MessageText::MessageText) => {
                                message_text = t.into_inner().into_owned();
                            }
                            None => {}
                        }
                    }
                }
                Event::CData(c) => {
                    if in_capture {
                        inner.text(&c);
                    }
                }
                Event::GeneralRef(ref g) => {
                    if in_capture {
                        inner.reference(g);
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(ResponseMessage {
            success: true,
            response_code,
            message_text,
            inner_xml: inner.finish(),
        })
    }
}
