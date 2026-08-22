/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::BTreeMap;
use std::io::BufRead;

use super::error::{ImapError, NoError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Ok,
    No,
    Bad,
    Bye,
    PreAuth,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLine {
    pub status: Status,
    pub code: Option<String>,
    pub code_args: Option<String>,
    pub text: String,
}

impl StatusLine {
    pub fn into_no_error(self) -> NoError {
        NoError::new(self.text, self.code)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Atom(String),
    Str(String),
    Bytes(Vec<u8>),
    Number(u64),
    Nil,
    List(Vec<Value>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Atom(s) | Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_number(&self) -> Option<u64> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(items) => Some(items.as_slice()),
            _ => None,
        }
    }

    pub fn into_bytes(self) -> Option<Vec<u8>> {
        match self {
            Value::Bytes(b) => Some(b),
            Value::Str(s) => Some(s.into_bytes()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Untagged {
    Capability(Vec<String>),
    List {
        attributes: Vec<String>,
        delimiter: Option<char>,
        name: String,
    },
    Lsub {
        attributes: Vec<String>,
        delimiter: Option<char>,
        name: String,
    },
    Status {
        mailbox: String,
        items: BTreeMap<String, u64>,
    },
    Search(Vec<u32>),
    Esearch {
        tag: Option<String>,
        all: Vec<u32>,
        count: Option<u32>,
    },
    Exists(u32),
    Recent(u32),
    Expunge(u32),
    Fetch {
        seq: u32,
        items: Vec<(String, Value)>,
    },
    Flags(Vec<String>),
    Namespace {
        personal: Vec<NamespaceEntry>,
        others: Vec<NamespaceEntry>,
        shared: Vec<NamespaceEntry>,
    },
    Enabled(Vec<String>),
    StatusLine(StatusLine),
    Bye(String),
    Raw(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceEntry {
    pub prefix: String,
    pub delimiter: Option<char>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Tagged { tag: String, line: StatusLine },
    Untagged(Untagged),
    Continuation(String),
}

pub fn parse_response<R: BufRead>(reader: &mut R) -> Result<Response, ImapError> {
    let mut parser = Parser::new(reader);
    parser.parse_response()
}

struct Parser<'r, R: BufRead> {
    reader: &'r mut R,
    buf: Vec<u8>,
    pos: usize,
}

impl<'r, R: BufRead> Parser<'r, R> {
    fn new(reader: &'r mut R) -> Self {
        Self {
            reader,
            buf: Vec::new(),
            pos: 0,
        }
    }

    fn parse_response(&mut self) -> Result<Response, ImapError> {
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
        let upper = first.to_ascii_uppercase();
        match upper.as_str() {
            "OK" | "NO" | "BAD" | "BYE" | "PREAUTH" => {
                self.skip_ws();
                let mut line = self.parse_status_line_body()?;
                line.status = match upper.as_str() {
                    "OK" => Status::Ok,
                    "NO" => Status::No,
                    "BAD" => Status::Bad,
                    "BYE" => Status::Bye,
                    "PREAUTH" => Status::PreAuth,
                    _ => unreachable!(),
                };
                if matches!(line.status, Status::Bye) {
                    return Ok(Untagged::Bye(line.text));
                }
                Ok(Untagged::StatusLine(line))
            }
            "CAPABILITY" => {
                let mut caps = Vec::new();
                while !self.at_end() {
                    self.skip_ws();
                    if self.at_end() {
                        break;
                    }
                    caps.push(self.read_atom_string()?);
                }
                Ok(Untagged::Capability(caps))
            }
            "LIST" | "LSUB" => {
                self.skip_ws();
                self.expect(b'(')?;
                let mut attrs = Vec::new();
                while self.peek() != Some(b')') {
                    self.skip_ws();
                    if self.peek() == Some(b')') {
                        break;
                    }
                    attrs.push(self.read_atom_string()?);
                }
                self.expect(b')')?;
                self.skip_ws();
                let delim_value = self.parse_value()?;
                let delimiter = match delim_value {
                    Value::Nil => None,
                    Value::Str(s) | Value::Atom(s) => s.chars().next(),
                    Value::Bytes(b) if !b.is_empty() => Some(b[0] as char),
                    _ => None,
                };
                self.skip_ws();
                let name_val = self.parse_value()?;
                let name = match name_val {
                    Value::Str(s) | Value::Atom(s) => s,
                    Value::Number(n) => n.to_string(),
                    Value::Bytes(b) => String::from_utf8_lossy(&b).into_owned(),
                    _ => return Err(ImapError::Parse("LIST mailbox name missing".into())),
                };
                if upper == "LIST" {
                    Ok(Untagged::List {
                        attributes: attrs,
                        delimiter,
                        name,
                    })
                } else {
                    Ok(Untagged::Lsub {
                        attributes: attrs,
                        delimiter,
                        name,
                    })
                }
            }
            "STATUS" => {
                self.skip_ws();
                let mailbox = match self.parse_value()? {
                    Value::Str(s) | Value::Atom(s) => s,
                    Value::Number(n) => n.to_string(),
                    Value::Bytes(b) => String::from_utf8_lossy(&b).into_owned(),
                    _ => return Err(ImapError::Parse("STATUS mailbox missing".into())),
                };
                self.skip_ws();
                self.expect(b'(')?;
                let mut items = BTreeMap::new();
                while self.peek() != Some(b')') {
                    self.skip_ws();
                    if self.peek() == Some(b')') {
                        break;
                    }
                    let key = self.read_atom_string()?.to_ascii_uppercase();
                    self.skip_ws();
                    let val = self.parse_value()?;
                    if let Some(n) = val.as_number() {
                        items.insert(key, n);
                    }
                }
                self.expect(b')')?;
                Ok(Untagged::Status { mailbox, items })
            }
            "SEARCH" => {
                let mut ids = Vec::new();
                while !self.at_end() {
                    self.skip_ws();
                    if self.at_end() {
                        break;
                    }
                    let v = self.parse_value()?;
                    if let Some(n) = v.as_number() {
                        ids.push(n as u32);
                    }
                }
                Ok(Untagged::Search(ids))
            }
            "ESEARCH" => self.parse_esearch(),
            "FLAGS" => {
                self.skip_ws();
                let v = self.parse_value()?;
                let flags = match v {
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
            "NAMESPACE" => self.parse_namespace(),
            "ENABLED" => {
                let mut exts = Vec::new();
                while !self.at_end() {
                    self.skip_ws();
                    if self.at_end() {
                        break;
                    }
                    exts.push(self.read_atom_string()?);
                }
                Ok(Untagged::Enabled(exts))
            }
            _ => {
                if let Ok(num) = upper.parse::<u32>() {
                    self.skip_ws();
                    let kw = self.read_atom_string()?.to_ascii_uppercase();
                    match kw.as_str() {
                        "EXISTS" => return Ok(Untagged::Exists(num)),
                        "RECENT" => return Ok(Untagged::Recent(num)),
                        "EXPUNGE" => return Ok(Untagged::Expunge(num)),
                        "FETCH" => return self.parse_fetch_body(num),
                        _ => {
                            let rest = self.read_until_crlf();
                            return Ok(Untagged::Raw(format!("{num} {kw} {rest}")));
                        }
                    }
                }
                let rest = self.read_until_crlf();
                Ok(Untagged::Raw(format!("{first} {rest}")))
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
            let key = self.read_atom_string()?.to_ascii_uppercase();
            if key == "TAG" {
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
            let key = self.read_atom_string()?.to_ascii_uppercase();
            match key.as_str() {
                "UID" => {}
                "ALL" => {
                    self.skip_ws();
                    let s = self.read_seq_set_token();
                    all = parse_seq_set(&s);
                }
                "COUNT" => {
                    self.skip_ws();
                    if let Value::Number(n) = self.parse_value()? {
                        count = Some(n as u32);
                    }
                }
                _ => {
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
        let bytes = &self.buf[start..self.pos];
        std::str::from_utf8(bytes)
            .map(|s| s.to_ascii_uppercase())
            .map_err(|e| ImapError::Parse(format!("non-utf8 fetch item name: {e}")))
    }

    fn parse_status_line(&mut self) -> Result<StatusLine, ImapError> {
        let kw = self.read_atom_string()?;
        self.skip_ws();
        let mut line = self.parse_status_line_body()?;
        line.status = match kw.to_ascii_uppercase().as_str() {
            "OK" => Status::Ok,
            "NO" => Status::No,
            "BAD" => Status::Bad,
            "BYE" => Status::Bye,
            "PREAUTH" => Status::PreAuth,
            other => return Err(ImapError::Parse(format!("unknown status: {other}"))),
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
            let inside = &self.buf[start..self.pos];
            let inside_str = std::str::from_utf8(inside)
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

    fn parse_value(&mut self) -> Result<Value, ImapError> {
        self.skip_ws();
        match self.peek() {
            Some(b'"') => self.parse_quoted(),
            Some(b'{') => self.parse_literal(),
            Some(b'(') => self.parse_list(),
            Some(b)
                if b.is_ascii_digit()
                    || b == b'-'
                    || b == b'+'
                    || is_atom_byte(b)
                    || b == b'\\' =>
            {
                self.parse_atom_or_number()
            }
            None => Err(ImapError::Parse("expected value, got EOL".into())),
            Some(other) => Err(ImapError::Parse(format!(
                "unexpected byte {other:#04x} in value"
            ))),
        }
    }

    fn parse_quoted(&mut self) -> Result<Value, ImapError> {
        self.expect(b'"')?;
        let mut raw: Vec<u8> = Vec::new();
        while let Some(b) = self.peek() {
            match b {
                b'"' => {
                    self.bump();
                    return Ok(match String::from_utf8(raw) {
                        Ok(s) => Value::Str(s),
                        Err(e) => Value::Bytes(e.into_bytes()),
                    });
                }
                b'\\' => {
                    self.bump();
                    let next = self
                        .peek()
                        .ok_or_else(|| ImapError::Parse("trailing \\ in quoted".into()))?;
                    raw.push(next);
                    self.bump();
                }
                _ => {
                    raw.push(b);
                    self.bump();
                }
            }
        }
        Err(ImapError::Parse("unterminated quoted string".into()))
    }

    fn parse_literal(&mut self) -> Result<Value, ImapError> {
        self.expect(b'{')?;
        let mut len_str = String::new();
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() {
                len_str.push(b as char);
                self.bump();
            } else {
                break;
            }
        }
        if self.peek() == Some(b'+') {
            self.bump();
        }
        self.expect(b'}')?;
        let len: usize = len_str
            .parse()
            .map_err(|e| ImapError::Parse(format!("literal length: {e}")))?;
        if !self.at_end_of_line() {
            return Err(ImapError::Parse("garbage after literal header".into()));
        }
        let mut data = vec![0u8; len];
        self.reader.read_exact(&mut data).map_err(ImapError::Io)?;
        self.buf.clear();
        self.read_line()?;
        if let Ok(s) = std::str::from_utf8(&data) {
            Ok(Value::Str(s.to_owned()))
        } else {
            Ok(Value::Bytes(data))
        }
    }

    fn parse_list(&mut self) -> Result<Value, ImapError> {
        self.expect(b'(')?;
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            if self.peek() == Some(b')') {
                self.bump();
                return Ok(Value::List(items));
            }
            if self.peek().is_none() {
                return Err(ImapError::Parse("unterminated list".into()));
            }
            items.push(self.parse_value()?);
        }
    }

    fn parse_atom_or_number(&mut self) -> Result<Value, ImapError> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if is_atom_byte(b) || b == b'\\' || b == b'*' || b == b']' || b == b'+' {
                self.bump();
            } else {
                break;
            }
        }
        let raw = &self.buf[start..self.pos];
        let s = std::str::from_utf8(raw)
            .map_err(|e| ImapError::Parse(format!("non-utf8 atom: {e}")))?
            .to_owned();
        if s.is_empty() {
            return Err(ImapError::Parse("empty atom".into()));
        }
        if s.eq_ignore_ascii_case("NIL") {
            return Ok(Value::Nil);
        }
        if let Ok(n) = s.parse::<u64>() {
            return Ok(Value::Number(n));
        }
        Ok(Value::Atom(s))
    }

    fn read_atom_string(&mut self) -> Result<String, ImapError> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if is_atom_byte(b) {
                self.bump();
            } else {
                break;
            }
        }
        if start == self.pos {
            return Err(ImapError::Parse("expected atom".into()));
        }
        let bytes = &self.buf[start..self.pos];
        std::str::from_utf8(bytes)
            .map(|s| s.to_owned())
            .map_err(|e| ImapError::Parse(format!("non-utf8 atom: {e}")))
    }

    fn read_seq_set_token(&mut self) -> String {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b == b' ' || b == b'\r' || b == b'\n' {
                break;
            }
            self.bump();
        }
        String::from_utf8_lossy(&self.buf[start..self.pos]).into_owned()
    }

    fn read_until_crlf(&mut self) -> String {
        let start = self.pos;
        let end = self.buf.len().saturating_sub(2);
        let end = end.max(start);
        self.pos = self.buf.len();
        String::from_utf8_lossy(&self.buf[start..end])
            .trim_end()
            .to_owned()
    }

    fn read_line(&mut self) -> Result<(), ImapError> {
        self.buf.clear();
        let n = self.reader.read_until(b'\n', &mut self.buf)?;
        self.pos = 0;
        if n == 0 {
            return Ok(());
        }
        Ok(())
    }

    fn peek(&self) -> Option<u8> {
        let end = self.buf.len().saturating_sub(self.tail_crlf_len());
        if self.pos >= end {
            None
        } else {
            Some(self.buf[self.pos])
        }
    }

    fn bump(&mut self) {
        self.pos += 1;
    }

    fn skip_ws(&mut self) {
        while self.peek() == Some(b' ') {
            self.bump();
        }
    }

    fn at_end(&self) -> bool {
        self.peek().is_none()
    }

    fn at_end_of_line(&self) -> bool {
        let end = self.buf.len().saturating_sub(self.tail_crlf_len());
        self.pos >= end
    }

    fn tail_crlf_len(&self) -> usize {
        if self.buf.ends_with(b"\r\n") {
            2
        } else if self.buf.ends_with(b"\n") {
            1
        } else {
            0
        }
    }

    fn expect(&mut self, b: u8) -> Result<(), ImapError> {
        if self.peek() == Some(b) {
            self.bump();
            Ok(())
        } else {
            Err(ImapError::Parse(format!(
                "expected {:?}, got {:?}",
                b as char,
                self.peek().map(|x| x as char)
            )))
        }
    }
}

fn is_atom_byte(b: u8) -> bool {
    matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
        | b'-' | b'.' | b'_' | b'!' | b'#' | b'$' | b'&'
        | b'\'' | b'+' | b'/' | b';' | b'<' | b'=' | b'>' | b'?'
        | b'@' | b'\\' | b'^' | b'`' | b'|' | b'~' | b'*')
}

fn parse_seq_set(s: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((lo, hi)) = part.split_once(':') {
            let lo: u32 = match lo.parse() {
                Ok(n) => n,
                Err(_) => continue,
            };
            let hi: u32 = match hi.parse() {
                Ok(n) => n,
                Err(_) => continue,
            };
            let (a, b) = if lo <= hi { (lo, hi) } else { (hi, lo) };
            for n in a..=b {
                out.push(n);
            }
        } else if let Ok(n) = part.parse::<u32>() {
            out.push(n);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse(input: &[u8]) -> Response {
        parse_response(&mut Cursor::new(input)).unwrap()
    }

    #[test]
    fn tagged_ok() {
        let r = parse(b"A001 OK LOGIN completed\r\n");
        match r {
            Response::Tagged { tag, line } => {
                assert_eq!(tag, "A001");
                assert_eq!(line.status, Status::Ok);
                assert_eq!(line.text, "LOGIN completed");
            }
            _ => panic!("expected Tagged"),
        }
    }

    #[test]
    fn tagged_no_with_code() {
        let r = parse(b"A002 NO [AUTHENTICATIONFAILED] bad credentials\r\n");
        match r {
            Response::Tagged { tag, line } => {
                assert_eq!(tag, "A002");
                assert_eq!(line.status, Status::No);
                assert_eq!(line.code.as_deref(), Some("AUTHENTICATIONFAILED"));
                assert_eq!(line.text, "bad credentials");
            }
            _ => panic!("expected Tagged"),
        }
    }

    #[test]
    fn untagged_capability() {
        let r = parse(b"* CAPABILITY IMAP4rev1 STARTTLS AUTH=PLAIN AUTH=LOGIN\r\n");
        match r {
            Response::Untagged(Untagged::Capability(caps)) => {
                assert_eq!(
                    caps,
                    vec!["IMAP4rev1", "STARTTLS", "AUTH=PLAIN", "AUTH=LOGIN"]
                );
            }
            _ => panic!("expected Capability"),
        }
    }

    #[test]
    fn untagged_capability_with_literal_plus() {
        let r = parse(
            b"* CAPABILITY IMAP4rev2 IMAP4rev1 ENABLE SASL-IR LITERAL+ ID UTF8=ACCEPT JMAPACCESS AUTH=PLAIN AUTH=OAUTHBEARER AUTH=XOAUTH2\r\n",
        );
        match r {
            Response::Untagged(Untagged::Capability(caps)) => {
                assert!(caps.iter().any(|c| c == "LITERAL+"));
                assert!(caps.iter().any(|c| c == "AUTH=OAUTHBEARER"));
                assert_eq!(caps.len(), 11);
            }
            _ => panic!("expected Capability"),
        }
    }

    #[test]
    fn quoted_mailbox_name_with_utf8_bytes_is_decoded_as_utf8() {
        let r = parse("* LIST (\\HasNoChildren) \"/\" \"Envoy\u{e9}s\"\r\n".as_bytes());
        match r {
            Response::Untagged(Untagged::List { name, .. }) => assert_eq!(name, "Envoyés"),
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn quoted_mailbox_name_with_modified_utf7_is_left_untouched() {
        let r =
            parse(b"* LIST (\\HasNoChildren) \"/\" \"[Gmail]/G&APY-nderilmi&AV8- Postalar\"\r\n");
        match r {
            Response::Untagged(Untagged::List { name, .. }) => {
                assert_eq!(name, "[Gmail]/G&APY-nderilmi&AV8- Postalar");
            }
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn quoted_mailbox_name_with_invalid_utf8_falls_back_to_lossy() {
        let mut line: Vec<u8> = b"* LIST (\\HasNoChildren) \"/\" \"bad".to_vec();
        line.push(0xff);
        line.extend_from_slice(b"name\"\r\n");
        let r = parse(&line);
        match r {
            Response::Untagged(Untagged::List { name, .. }) => {
                assert!(name.starts_with("bad") && name.ends_with("name"));
            }
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn untagged_list_with_special_use() {
        let r = parse(b"* LIST (\\HasNoChildren \\Sent) \"/\" \"Sent Mail\"\r\n");
        match r {
            Response::Untagged(Untagged::List {
                attributes,
                delimiter,
                name,
            }) => {
                assert_eq!(attributes, vec!["\\HasNoChildren", "\\Sent"]);
                assert_eq!(delimiter, Some('/'));
                assert_eq!(name, "Sent Mail");
            }
            _ => panic!("expected List"),
        }
    }

    #[test]
    fn untagged_list_with_nil_delimiter() {
        let r = parse(b"* LIST () NIL INBOX\r\n");
        match r {
            Response::Untagged(Untagged::List {
                attributes,
                delimiter,
                name,
            }) => {
                assert!(attributes.is_empty());
                assert_eq!(delimiter, None);
                assert_eq!(name, "INBOX");
            }
            _ => panic!("expected List"),
        }
    }

    #[test]
    fn untagged_list_with_numeric_atom_name() {
        let r = parse(b"* LIST (\\Subscribed \\HasNoChildren \\UnMarked) \"/\" 200\r\n");
        match r {
            Response::Untagged(Untagged::List {
                attributes,
                delimiter,
                name,
            }) => {
                assert_eq!(
                    attributes,
                    vec!["\\Subscribed", "\\HasNoChildren", "\\UnMarked"]
                );
                assert_eq!(delimiter, Some('/'));
                assert_eq!(name, "200");
            }
            _ => panic!("expected List"),
        }
    }

    #[test]
    fn untagged_status() {
        let r = parse(b"* STATUS INBOX (UIDVALIDITY 12345 UIDNEXT 42 MESSAGES 7)\r\n");
        match r {
            Response::Untagged(Untagged::Status { mailbox, items }) => {
                assert_eq!(mailbox, "INBOX");
                assert_eq!(items.get("UIDVALIDITY"), Some(&12345));
                assert_eq!(items.get("UIDNEXT"), Some(&42));
                assert_eq!(items.get("MESSAGES"), Some(&7));
            }
            _ => panic!("expected Status"),
        }
    }

    #[test]
    fn untagged_status_with_numeric_atom_mailbox() {
        let r = parse(b"* STATUS 200 (MESSAGES 5 UIDNEXT 6 UIDVALIDITY 1778253936)\r\n");
        match r {
            Response::Untagged(Untagged::Status { mailbox, items }) => {
                assert_eq!(mailbox, "200");
                assert_eq!(items.get("MESSAGES"), Some(&5));
                assert_eq!(items.get("UIDNEXT"), Some(&6));
                assert_eq!(items.get("UIDVALIDITY"), Some(&1778253936));
            }
            _ => panic!("expected Status"),
        }
    }

    #[test]
    fn untagged_search_with_uids() {
        let r = parse(b"* SEARCH 1 3 5 7\r\n");
        match r {
            Response::Untagged(Untagged::Search(ids)) => {
                assert_eq!(ids, vec![1, 3, 5, 7]);
            }
            _ => panic!("expected Search"),
        }
    }

    #[test]
    fn untagged_search_empty() {
        let r = parse(b"* SEARCH\r\n");
        match r {
            Response::Untagged(Untagged::Search(ids)) => {
                assert!(ids.is_empty());
            }
            _ => panic!("expected Search"),
        }
    }

    #[test]
    fn untagged_esearch_all_set() {
        let r = parse(b"* ESEARCH (TAG \"q1\") UID ALL 1:3,7\r\n");
        match r {
            Response::Untagged(Untagged::Esearch { tag, all, .. }) => {
                assert_eq!(tag.as_deref(), Some("q1"));
                assert_eq!(all, vec![1, 2, 3, 7]);
            }
            _ => panic!("expected Esearch"),
        }
    }

    #[test]
    fn untagged_exists_recent() {
        let r = parse(b"* 42 EXISTS\r\n");
        assert!(matches!(r, Response::Untagged(Untagged::Exists(42))));
        let r = parse(b"* 3 RECENT\r\n");
        assert!(matches!(r, Response::Untagged(Untagged::Recent(3))));
    }

    #[test]
    fn untagged_fetch_flags_and_uid() {
        let r = parse(
            b"* 17 FETCH (UID 100 FLAGS (\\Seen \\Flagged) RFC822.SIZE 4242 INTERNALDATE \"17-Jul-1996 02:44:25 -0700\")\r\n",
        );
        match r {
            Response::Untagged(Untagged::Fetch { seq, items }) => {
                assert_eq!(seq, 17);
                let uid = items.iter().find(|(n, _)| n == "UID").unwrap();
                assert_eq!(uid.1.as_number(), Some(100));
                let flags = items.iter().find(|(n, _)| n == "FLAGS").unwrap();
                let list = flags.1.as_list().unwrap();
                assert_eq!(list.len(), 2);
                let size = items.iter().find(|(n, _)| n == "RFC822.SIZE").unwrap();
                assert_eq!(size.1.as_number(), Some(4242));
                let internaldate = items.iter().find(|(n, _)| n == "INTERNALDATE").unwrap();
                assert_eq!(internaldate.1.as_str(), Some("17-Jul-1996 02:44:25 -0700"));
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn untagged_fetch_with_body_literal() {
        let input = b"* 1 FETCH (UID 5 BODY[] {11}\r\nHello world)\r\n";
        let r = parse(input);
        match r {
            Response::Untagged(Untagged::Fetch { seq, items }) => {
                assert_eq!(seq, 1);
                let body = items.iter().find(|(n, _)| n == "BODY[]").unwrap();
                match &body.1 {
                    Value::Str(s) => assert_eq!(s, "Hello world"),
                    Value::Bytes(b) => assert_eq!(b, b"Hello world"),
                    other => panic!("expected literal, got {other:?}"),
                }
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn untagged_fetch_with_binary_literal() {
        let mut input = b"* 1 FETCH (UID 5 BODY[] {4}\r\n".to_vec();
        input.extend_from_slice(&[0xFF, 0xFE, 0x00, 0x01]);
        input.extend_from_slice(b")\r\n");
        let r = parse_response(&mut Cursor::new(input)).unwrap();
        match r {
            Response::Untagged(Untagged::Fetch { items, .. }) => {
                let body = items.iter().find(|(n, _)| n == "BODY[]").unwrap();
                match &body.1 {
                    Value::Bytes(b) => assert_eq!(b, &[0xFF, 0xFE, 0x00, 0x01]),
                    other => panic!("expected Bytes, got {other:?}"),
                }
            }
            _ => panic!("expected Fetch"),
        }
    }

    #[test]
    fn untagged_flags() {
        let r = parse(b"* FLAGS (\\Answered \\Flagged \\Draft \\Deleted \\Seen)\r\n");
        match r {
            Response::Untagged(Untagged::Flags(flags)) => {
                assert_eq!(flags.len(), 5);
                assert!(flags.contains(&"\\Seen".to_owned()));
            }
            _ => panic!("expected Flags"),
        }
    }

    #[test]
    fn untagged_bye() {
        let r = parse(b"* BYE server logging out\r\n");
        match r {
            Response::Untagged(Untagged::Bye(text)) => {
                assert_eq!(text, "server logging out");
            }
            _ => panic!("expected Bye"),
        }
    }

    #[test]
    fn untagged_namespace() {
        let r = parse(b"* NAMESPACE ((\"INBOX.\" \".\")) NIL NIL\r\n");
        match r {
            Response::Untagged(Untagged::Namespace {
                personal,
                others,
                shared,
            }) => {
                assert_eq!(personal.len(), 1);
                assert_eq!(personal[0].prefix, "INBOX.");
                assert_eq!(personal[0].delimiter, Some('.'));
                assert!(others.is_empty());
                assert!(shared.is_empty());
            }
            _ => panic!("expected Namespace"),
        }
    }

    #[test]
    fn continuation_request() {
        let r = parse(b"+ Ready for additional command text\r\n");
        match r {
            Response::Continuation(text) => {
                assert_eq!(text, "Ready for additional command text");
            }
            _ => panic!("expected Continuation"),
        }
    }

    #[test]
    fn untagged_status_with_response_code() {
        let r = parse(b"* OK [PERMANENTFLAGS (\\Seen \\Flagged \\*)] Limited\r\n");
        match r {
            Response::Untagged(Untagged::StatusLine(line)) => {
                assert_eq!(line.status, Status::Ok);
                assert_eq!(line.code.as_deref(), Some("PERMANENTFLAGS"));
                assert!(line.code_args.as_deref().is_some());
                assert_eq!(line.text, "Limited");
            }
            _ => panic!("expected StatusLine"),
        }
    }

    #[test]
    fn untagged_ok_with_uidvalidity() {
        let r = parse(b"* OK [UIDVALIDITY 3857529045] UIDs valid\r\n");
        match r {
            Response::Untagged(Untagged::StatusLine(line)) => {
                assert_eq!(line.code.as_deref(), Some("UIDVALIDITY"));
                assert_eq!(line.code_args.as_deref(), Some("3857529045"));
            }
            _ => panic!("expected StatusLine"),
        }
    }

    #[test]
    fn quoted_string_with_escape() {
        let r = parse(b"* LIST () \"/\" \"He said \\\"hi\\\"\"\r\n");
        match r {
            Response::Untagged(Untagged::List { name, .. }) => {
                assert_eq!(name, "He said \"hi\"");
            }
            _ => panic!("expected List"),
        }
    }

    #[test]
    fn seq_set_parses_ranges_and_singletons() {
        assert_eq!(parse_seq_set("1:3,5,7:8"), vec![1, 2, 3, 5, 7, 8]);
        assert_eq!(parse_seq_set("42"), vec![42]);
        assert!(parse_seq_set("").is_empty());
    }

    #[test]
    fn empty_read_is_disconnected() {
        let mut empty: &[u8] = b"";
        let err = parse_response(&mut empty).unwrap_err();
        assert!(matches!(err, ImapError::Disconnected));
    }

    #[test]
    fn untagged_enabled_parses() {
        let r = parse(b"* ENABLED UTF8=ACCEPT CONDSTORE\r\n");
        match r {
            Response::Untagged(Untagged::Enabled(exts)) => {
                assert_eq!(exts, vec!["UTF8=ACCEPT", "CONDSTORE"]);
            }
            other => panic!("expected Enabled, got {other:?}"),
        }
    }
}
