/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::BTreeMap;
use std::io::BufRead;

use super::{NamespaceEntry, Response, Status, StatusLine, Untagged, Value};
use crate::imap::error::ImapError;

pub(super) const LITERAL_RESERVE: usize = 1 << 20;
pub(super) const MAX_SEQ_SET: usize = 1 << 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keyword {
    Status(Status),
    Capability,
    List,
    Lsub,
    StatusItems,
    Search,
    Esearch,
    Flags,
    Namespace,
    Enabled,
}

impl Keyword {
    fn parse(word: &str) -> Option<Keyword> {
        hashify::map_ignore_case!(word.as_bytes(), Keyword,
            "OK" => Keyword::Status(Status::Ok),
            "NO" => Keyword::Status(Status::No),
            "BAD" => Keyword::Status(Status::Bad),
            "BYE" => Keyword::Status(Status::Bye),
            "PREAUTH" => Keyword::Status(Status::PreAuth),
            "CAPABILITY" => Keyword::Capability,
            "LIST" => Keyword::List,
            "LSUB" => Keyword::Lsub,
            "STATUS" => Keyword::StatusItems,
            "SEARCH" => Keyword::Search,
            "ESEARCH" => Keyword::Esearch,
            "FLAGS" => Keyword::Flags,
            "NAMESPACE" => Keyword::Namespace,
            "ENABLED" => Keyword::Enabled,
        )
        .copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageKeyword {
    Exists,
    Recent,
    Expunge,
    Fetch,
}

impl MessageKeyword {
    fn parse(word: &str) -> Option<MessageKeyword> {
        hashify::map_ignore_case!(word.as_bytes(), MessageKeyword,
            "EXISTS" => MessageKeyword::Exists,
            "RECENT" => MessageKeyword::Recent,
            "EXPUNGE" => MessageKeyword::Expunge,
            "FETCH" => MessageKeyword::Fetch,
        )
        .copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EsearchItem {
    Uid,
    All,
    Count,
}

impl EsearchItem {
    fn parse(word: &str) -> Option<EsearchItem> {
        hashify::map_ignore_case!(word.as_bytes(), EsearchItem,
            "UID" => EsearchItem::Uid,
            "ALL" => EsearchItem::All,
            "COUNT" => EsearchItem::Count,
        )
        .copied()
    }
}

pub(super) struct Parser<'r, R: BufRead> {
    pub(super) reader: &'r mut R,
    pub(super) buf: Vec<u8>,
    pub(super) pos: usize,
}

impl<'r, R: BufRead> Parser<'r, R> {
    pub(super) fn new(reader: &'r mut R) -> Self {
        Self {
            reader,
            buf: Vec::new(),
            pos: 0,
        }
    }

    pub(super) fn parse_response(&mut self) -> Result<Response, ImapError> {
        self.read_line()?;
        if self.buf.is_empty() {
            return Err(ImapError::Disconnected);
        }
        if self.peek() == Some(b'+') {
            self.bump();
            self.skip_ws();
            let text = self.read_until_crlf();
            return Ok(Response::Continuation(text));
        }
        if self.peek() == Some(b'*') {
            self.bump();
            self.skip_ws();
            return self.parse_untagged().map(Response::Untagged);
        }
        let tag = self.read_atom_string()?;
        self.skip_ws();
        let line = self.parse_status_line()?;
        Ok(Response::Tagged { tag, line })
    }

    fn parse_untagged(&mut self) -> Result<Untagged, ImapError> {
        let first = self.read_atom_string()?;
        let Some(keyword) = Keyword::parse(&first) else {
            return self.parse_numbered(first);
        };
        match keyword {
            Keyword::Status(status) => {
                self.skip_ws();
                let mut line = self.parse_status_line_body()?;
                line.status = status;
                if status == Status::Bye {
                    return Ok(Untagged::Bye(line.text));
                }
                Ok(Untagged::StatusLine(line))
            }
            Keyword::Capability => Ok(Untagged::Capability(self.read_space_separated()?)),
            Keyword::List | Keyword::Lsub => {
                self.skip_ws();
                self.expect(b'(')?;
                let mut attributes = Vec::new();
                while self.peek() != Some(b')') {
                    self.skip_ws();
                    if self.peek() == Some(b')') {
                        break;
                    }
                    attributes.push(self.read_atom_string()?);
                }
                self.expect(b')')?;
                self.skip_ws();
                let delimiter = match self.parse_value()? {
                    Value::Str(s) | Value::Atom(s) => s.chars().next(),
                    Value::Bytes(b) => b.first().map(|&b| char::from(b)),
                    _ => None,
                };
                self.skip_ws();
                let name = self
                    .parse_value()?
                    .into_name()
                    .ok_or_else(|| ImapError::Parse("LIST mailbox name missing".into()))?;
                Ok(if keyword == Keyword::List {
                    Untagged::List {
                        attributes,
                        delimiter,
                        name,
                    }
                } else {
                    Untagged::Lsub {
                        attributes,
                        delimiter,
                        name,
                    }
                })
            }
            Keyword::StatusItems => {
                self.skip_ws();
                let mailbox = self
                    .parse_value()?
                    .into_name()
                    .ok_or_else(|| ImapError::Parse("STATUS mailbox missing".into()))?;
                self.skip_ws();
                self.expect(b'(')?;
                let mut items = BTreeMap::new();
                while self.peek() != Some(b')') {
                    self.skip_ws();
                    if self.peek() == Some(b')') {
                        break;
                    }
                    let mut key = self.read_atom_string()?;
                    key.make_ascii_uppercase();
                    self.skip_ws();
                    if let Some(n) = self.parse_value()?.as_number() {
                        items.insert(key, n);
                    }
                }
                self.expect(b')')?;
                Ok(Untagged::Status { mailbox, items })
            }
            Keyword::Search => {
                let mut ids = Vec::new();
                while !self.at_end() {
                    self.skip_ws();
                    if self.at_end() {
                        break;
                    }
                    if let Some(n) = self
                        .parse_value()?
                        .as_number()
                        .and_then(|n| u32::try_from(n).ok())
                    {
                        ids.push(n);
                    }
                }
                Ok(Untagged::Search(ids))
            }
            Keyword::Esearch => self.parse_esearch(),
            Keyword::Flags => {
                self.skip_ws();
                let flags = match self.parse_value()? {
                    Value::List(items) => items
                        .into_iter()
                        .filter_map(|x| match x {
                            Value::Atom(s) | Value::Str(s) => Some(s),
                            _ => None,
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                Ok(Untagged::Flags(flags))
            }
            Keyword::Namespace => self.parse_namespace(),
            Keyword::Enabled => Ok(Untagged::Enabled(self.read_space_separated()?)),
        }
    }

    fn parse_numbered(&mut self, first: String) -> Result<Untagged, ImapError> {
        let Ok(num) = first.parse::<u32>() else {
            let rest = self.read_until_crlf();
            return Ok(Untagged::Raw(format!("{first} {rest}")));
        };
        self.skip_ws();
        let mut kw = self.read_atom_string()?;
        match MessageKeyword::parse(&kw) {
            Some(MessageKeyword::Exists) => Ok(Untagged::Exists(num)),
            Some(MessageKeyword::Recent) => Ok(Untagged::Recent(num)),
            Some(MessageKeyword::Expunge) => Ok(Untagged::Expunge(num)),
            Some(MessageKeyword::Fetch) => self.parse_fetch_body(num),
            None => {
                kw.make_ascii_uppercase();
                let rest = self.read_until_crlf();
                Ok(Untagged::Raw(format!("{num} {kw} {rest}")))
            }
        }
    }

    fn parse_esearch(&mut self) -> Result<Untagged, ImapError> {
        let mut tag = None;
        let mut all = Vec::new();
        let mut count = None;
        if self.peek() == Some(b' ') {
            self.skip_ws();
        }
        if self.peek() == Some(b'(') {
            self.bump();
            self.skip_ws();
            let key = self.read_atom_string()?;
            if key.eq_ignore_ascii_case("TAG") {
                self.skip_ws();
                if let Value::Str(s) = self.parse_value()? {
                    tag = Some(s);
                }
            }
            self.skip_ws();
            self.expect(b')')?;
        }
        while !self.at_end() {
            self.skip_ws();
            if self.at_end() {
                break;
            }
            let key = self.read_atom_string()?;
            match EsearchItem::parse(&key) {
                Some(EsearchItem::Uid) => {}
                Some(EsearchItem::All) => {
                    self.skip_ws();
                    all = self.read_seq_set()?;
                }
                Some(EsearchItem::Count) => {
                    self.skip_ws();
                    if let Value::Number(n) = self.parse_value()? {
                        count = u32::try_from(n).ok();
                    }
                }
                None => {
                    self.skip_ws();
                    let _ = self.parse_value();
                }
            }
        }
        Ok(Untagged::Esearch { tag, all, count })
    }

    fn parse_namespace(&mut self) -> Result<Untagged, ImapError> {
        self.skip_ws();
        let personal = self.parse_namespace_list()?;
        self.skip_ws();
        let others = self.parse_namespace_list()?;
        self.skip_ws();
        let shared = self.parse_namespace_list()?;
        Ok(Untagged::Namespace {
            personal,
            others,
            shared,
        })
    }

    fn parse_namespace_list(&mut self) -> Result<Vec<NamespaceEntry>, ImapError> {
        match self.parse_value()? {
            Value::Nil => Ok(Vec::new()),
            Value::List(entries) => {
                let mut out = Vec::new();
                for e in entries {
                    if let Value::List(inner) = e
                        && let Some(prefix) = inner.first().and_then(|v| v.as_str())
                    {
                        let delim = inner.get(1).and_then(|v| match v {
                            Value::Nil => None,
                            Value::Str(s) | Value::Atom(s) => s.chars().next(),
                            _ => None,
                        });
                        out.push(NamespaceEntry {
                            prefix: prefix.to_owned(),
                            delimiter: delim,
                        });
                    }
                }
                Ok(out)
            }
            _ => Ok(Vec::new()),
        }
    }

    fn parse_fetch_body(&mut self, seq: u32) -> Result<Untagged, ImapError> {
        self.skip_ws();
        self.expect(b'(')?;
        let mut items = Vec::new();
        while self.peek() != Some(b')') {
            self.skip_ws();
            if self.peek() == Some(b')') {
                break;
            }
            let name = self.read_fetch_item_name()?;
            self.skip_ws();
            let val = self.parse_value()?;
            items.push((name, val));
        }
        self.expect(b')')?;
        Ok(Untagged::Fetch { seq, items })
    }

    fn read_fetch_item_name(&mut self) -> Result<String, ImapError> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            match b {
                b' ' | b'\r' | b'\n' | b')' | b'(' => break,
                _ => self.bump(),
            }
        }
        str::from_utf8(self.span(start))
            .map(|s| s.to_ascii_uppercase())
            .map_err(|e| ImapError::Parse(format!("non-utf8 fetch item name: {e}")))
    }

    fn parse_status_line(&mut self) -> Result<StatusLine, ImapError> {
        let kw = self.read_atom_string()?;
        self.skip_ws();
        let mut line = self.parse_status_line_body()?;
        line.status = match Keyword::parse(&kw) {
            Some(Keyword::Status(status)) => status,
            _ => {
                return Err(ImapError::Parse(format!(
                    "unknown status: {}",
                    kw.to_ascii_uppercase()
                )));
            }
        };
        Ok(line)
    }

    fn parse_status_line_body(&mut self) -> Result<StatusLine, ImapError> {
        let mut code = None;
        let mut code_args = None;
        if self.peek() == Some(b'[') {
            self.bump();
            let start = self.pos;
            let mut depth = 1usize;
            while let Some(b) = self.peek() {
                if b == b'[' {
                    depth += 1;
                    self.bump();
                } else if b == b']' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    self.bump();
                } else {
                    self.bump();
                }
            }
            let inside_str = str::from_utf8(self.span(start))
                .map_err(|e| ImapError::Parse(format!("non-utf8 status code: {e}")))?;
            let (head, tail) = match inside_str.split_once(' ') {
                Some((h, t)) => (h.to_owned(), Some(t.to_owned())),
                None => (inside_str.to_owned(), None),
            };
            code = Some(head.to_ascii_uppercase());
            code_args = tail;
            self.expect(b']')?;
            self.skip_ws();
        }
        let text = self.read_until_crlf();
        Ok(StatusLine {
            status: Status::Ok,
            code,
            code_args,
            text,
        })
    }
}
