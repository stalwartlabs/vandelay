/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::io::{self, BufRead, Read};
use std::str;

use super::Value;
use super::parser::{LITERAL_RESERVE, MAX_SEQ_SET, Parser};
use crate::imap::error::ImapError;

impl<R: BufRead> Parser<'_, R> {
    pub(super) fn parse_value(&mut self) -> Result<Value, ImapError> {
        self.skip_ws();
        match self.peek() {
            Some(b'"') => self.parse_quoted(),
            Some(b'{') => self.parse_literal(),
            Some(b'(') => self.parse_list(),
            Some(b)
                if b.is_ascii_digit()
                    || b == b'-'
                    || b == b'+'
                    || AtomByte::is(b)
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

    pub(super) fn parse_quoted(&mut self) -> Result<Value, ImapError> {
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

    pub(super) fn parse_literal(&mut self) -> Result<Value, ImapError> {
        self.expect(b'{')?;
        let start = self.pos;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.bump();
        }
        let len_str = String::from_utf8_lossy(self.span(start)).into_owned();
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
        let mut data = Vec::with_capacity(len.min(LITERAL_RESERVE));
        self.reader
            .by_ref()
            .take(u64::try_from(len).unwrap_or(u64::MAX))
            .read_to_end(&mut data)?;
        if data.len() != len {
            return Err(ImapError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "failed to fill whole buffer",
            )));
        }
        self.buf.clear();
        self.read_line()?;
        Ok(match String::from_utf8(data) {
            Ok(s) => Value::Str(s),
            Err(e) => Value::Bytes(e.into_bytes()),
        })
    }

    pub(super) fn parse_list(&mut self) -> Result<Value, ImapError> {
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

    pub(super) fn parse_atom_or_number(&mut self) -> Result<Value, ImapError> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if AtomByte::is(b) || b == b'\\' || b == b'*' || b == b']' || b == b'+' {
                self.bump();
            } else {
                break;
            }
        }
        let s = str::from_utf8(self.span(start))
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

    pub(super) fn read_atom_string(&mut self) -> Result<String, ImapError> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if AtomByte::is(b) {
                self.bump();
            } else {
                break;
            }
        }
        if start == self.pos {
            return Err(ImapError::Parse("expected atom".into()));
        }
        str::from_utf8(self.span(start))
            .map(str::to_owned)
            .map_err(|e| ImapError::Parse(format!("non-utf8 atom: {e}")))
    }

    pub(super) fn read_space_separated(&mut self) -> Result<Vec<String>, ImapError> {
        let end = self.buf.len().saturating_sub(self.tail_crlf_len());
        let rest = self.buf.get(self.pos..end).unwrap_or_default();
        self.pos = self.pos.max(end);
        str::from_utf8(rest)
            .map(|s| s.split_ascii_whitespace().map(str::to_owned).collect())
            .map_err(|e| ImapError::Parse(format!("non-utf8 token: {e}")))
    }

    pub(super) fn read_seq_set(&mut self) -> Result<Vec<u32>, ImapError> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b == b' ' || b == b'\r' || b == b'\n' {
                break;
            }
            self.bump();
        }
        SeqSet::expand(&String::from_utf8_lossy(self.span(start)))
    }

    pub(super) fn span(&self, start: usize) -> &[u8] {
        self.buf.get(start..self.pos).unwrap_or_default()
    }

    pub(super) fn read_until_crlf(&mut self) -> String {
        let start = self.pos;
        let end = self.buf.len().saturating_sub(2).max(start);
        self.pos = self.buf.len();
        String::from_utf8_lossy(self.buf.get(start..end).unwrap_or_default())
            .trim_end()
            .to_owned()
    }

    pub(super) fn read_line(&mut self) -> Result<(), ImapError> {
        self.buf.clear();
        self.reader.read_until(b'\n', &mut self.buf)?;
        self.pos = 0;
        Ok(())
    }

    pub(super) fn peek(&self) -> Option<u8> {
        let end = self.buf.len().saturating_sub(self.tail_crlf_len());
        self.buf.get(self.pos..end)?.first().copied()
    }

    pub(super) fn bump(&mut self) {
        self.pos += 1;
    }

    pub(super) fn skip_ws(&mut self) {
        while self.peek() == Some(b' ') {
            self.bump();
        }
    }

    pub(super) fn at_end(&self) -> bool {
        self.peek().is_none()
    }

    pub(super) fn at_end_of_line(&self) -> bool {
        let end = self.buf.len().saturating_sub(self.tail_crlf_len());
        self.pos >= end
    }

    pub(super) fn tail_crlf_len(&self) -> usize {
        if self.buf.ends_with(b"\r\n") {
            2
        } else if self.buf.ends_with(b"\n") {
            1
        } else {
            0
        }
    }

    pub(super) fn expect(&mut self, b: u8) -> Result<(), ImapError> {
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

pub(super) struct SeqSet;

impl SeqSet {
    pub(super) fn expand(s: &str) -> Result<Vec<u32>, ImapError> {
        let mut out = Vec::new();
        for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            if let Some((lo, hi)) = part.split_once(':') {
                let (Ok(lo), Ok(hi)) = (lo.parse::<u32>(), hi.parse::<u32>()) else {
                    continue;
                };
                let (a, b) = if lo <= hi { (lo, hi) } else { (hi, lo) };
                if out.len() + (b - a) as usize >= MAX_SEQ_SET {
                    return Err(ImapError::Parse(format!(
                        "sequence set expands beyond {MAX_SEQ_SET} ids"
                    )));
                }
                out.extend(a..=b);
            } else if let Ok(n) = part.parse::<u32>() {
                out.push(n);
            }
        }
        Ok(out)
    }
}

pub(super) struct AtomByte;

impl AtomByte {
    pub(super) fn is(b: u8) -> bool {
        b.is_ascii_graphic() && !matches!(b, b'(' | b')' | b'{' | b'%' | b'"' | b']')
    }
}
