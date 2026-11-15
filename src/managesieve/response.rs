/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt;
use std::io::{self, BufRead, Read};

use super::error::{NoError, SieveError};

const LITERAL_RESERVE: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    Atom(String),
    QStr(String),
    Literal(Vec<u8>),
    LParen,
    RParen,
}

impl Token {
    pub fn as_string(&self) -> Option<String> {
        match self {
            Token::QStr(s) | Token::Atom(s) => Some(s.clone()),
            Token::Literal(b) => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        }
    }

    fn read_line<R: BufRead>(reader: &mut R) -> Result<Vec<Token>, SieveError> {
        let mut tokens: Vec<Token> = Vec::new();
        let mut physical = Vec::with_capacity(256);
        Token::read_physical(reader, &mut physical)?;
        while let Some(len) = Token::scan(&physical, &mut tokens)? {
            tokens.push(Token::Literal(Token::read_literal(reader, len)?));
            Token::read_physical(reader, &mut physical)?;
        }
        Ok(tokens)
    }

    fn scan(line: &[u8], tokens: &mut Vec<Token>) -> Result<Option<usize>, SieveError> {
        let mut rest = line;
        loop {
            let skip = rest
                .iter()
                .take_while(|b| matches!(b, b' ' | b'\t'))
                .count();
            rest = rest.get(skip..).unwrap_or_default();
            let Some((&first, tail)) = rest.split_first() else {
                return Ok(None);
            };
            match first {
                b'"' => {
                    let (s, after) = Token::quoted(tail)?;
                    tokens.push(Token::QStr(s));
                    rest = after;
                }
                b'{' => {
                    let (n, trailing) = Token::literal_marker(tail)?;
                    if trailing.iter().any(|b| !matches!(b, b' ' | b'\t')) {
                        return Err(SieveError::Parse(format!(
                            "literal marker {{{n}}} not at end of line: trailing {:?}",
                            String::from_utf8_lossy(trailing)
                        )));
                    }
                    return Ok(Some(n));
                }
                b'(' => {
                    tokens.push(Token::LParen);
                    rest = tail;
                }
                b')' => {
                    tokens.push(Token::RParen);
                    rest = tail;
                }
                _ => {
                    let len = rest
                        .iter()
                        .position(|b| matches!(b, b' ' | b'\t' | b'"' | b'{' | b'(' | b')'))
                        .unwrap_or(rest.len());
                    let (atom, after) = rest.split_at_checked(len).unwrap_or((rest, &[]));
                    tokens.push(Token::Atom(String::from_utf8_lossy(atom).into_owned()));
                    rest = after;
                }
            }
        }
    }

    fn read_physical<R: BufRead>(reader: &mut R, buf: &mut Vec<u8>) -> Result<(), SieveError> {
        buf.clear();
        let n = reader.read_until(b'\n', buf)?;
        if n == 0 {
            return Err(SieveError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "managesieve closed stream",
            )));
        }
        while matches!(buf.last(), Some(b'\n') | Some(b'\r')) {
            buf.pop();
        }
        Ok(())
    }

    fn read_literal<R: BufRead>(reader: &mut R, len: usize) -> Result<Vec<u8>, SieveError> {
        let mut data = Vec::with_capacity(len.min(LITERAL_RESERVE));
        reader
            .take(u64::try_from(len).unwrap_or(u64::MAX))
            .read_to_end(&mut data)?;
        if data.len() != len {
            return Err(SieveError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "failed to fill whole buffer",
            )));
        }
        Ok(data)
    }

    fn quoted(buf: &[u8]) -> Result<(String, &[u8]), SieveError> {
        let mut bytes: Vec<u8> = Vec::new();
        let mut iter = buf.iter().enumerate();
        while let Some((i, &b)) = iter.next() {
            match b {
                b'"' => {
                    let s = String::from_utf8_lossy(&bytes).into_owned();
                    return Ok((s, buf.get(i + 1..).unwrap_or_default()));
                }
                b'\\' => match iter.next() {
                    Some((_, &escaped)) => bytes.push(escaped),
                    None => break,
                },
                _ => bytes.push(b),
            }
        }
        Err(SieveError::Parse("unterminated quoted-string".into()))
    }

    fn literal_marker(buf: &[u8]) -> Result<(usize, &[u8]), SieveError> {
        let digits = buf.iter().take_while(|b| b.is_ascii_digit()).count();
        let (number, rest) = buf.split_at_checked(digits).unwrap_or((buf, &[]));
        if number.is_empty() {
            return Err(SieveError::Parse("literal marker missing digits".into()));
        }
        let n = number.iter().try_fold(0usize, |n, d| {
            n.checked_mul(10)
                .and_then(|v| v.checked_add(usize::from(d - b'0')))
                .ok_or_else(|| SieveError::Parse("literal size overflow".into()))
        })?;
        let rest = rest.strip_prefix(b"+").unwrap_or(rest);
        let rest = rest
            .strip_prefix(b"}")
            .ok_or_else(|| SieveError::Parse("literal marker missing closing brace".into()))?;
        Ok((n, rest))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    No,
    Bye,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Status::Ok => "OK",
            Status::No => "NO",
            Status::Bye => "BYE",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLine {
    pub status: Status,
    pub code: Option<String>,
    pub code_args: Option<String>,
    pub text: String,
}

impl Default for StatusLine {
    fn default() -> Self {
        StatusLine {
            status: Status::Ok,
            code: None,
            code_args: None,
            text: String::new(),
        }
    }
}

impl fmt::Display for StatusLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.status, self.text)
    }
}

impl StatusLine {
    pub fn into_no_error(self) -> NoError {
        NoError::new(self.text, self.code)
    }

    pub fn from_line(line: &[u8]) -> Result<Option<StatusLine>, SieveError> {
        let mut tokens = Vec::new();
        if Token::scan(line, &mut tokens)?.is_some() {
            return Ok(None);
        }
        Ok(StatusLine::parse(&tokens))
    }

    pub fn parse(tokens: &[Token]) -> Option<StatusLine> {
        let (Token::Atom(word), mut rest) = tokens.split_first()? else {
            return None;
        };
        let status = *hashify::map!(word.as_bytes(), Status,
            "OK" => Status::Ok,
            "ok" => Status::Ok,
            "Ok" => Status::Ok,
            "NO" => Status::No,
            "no" => Status::No,
            "No" => Status::No,
            "BYE" => Status::Bye,
            "bye" => Status::Bye,
            "Bye" => Status::Bye,
        )?;
        let mut code: Option<String> = None;
        let mut code_args: Option<String> = None;
        if let Some((Token::LParen, after)) = rest.split_first() {
            let mut depth: u32 = 1;
            let mut consumed = 0usize;
            let mut inside: Vec<&Token> = Vec::new();
            for t in after {
                consumed += 1;
                match t {
                    Token::LParen => depth += 1,
                    Token::RParen => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                inside.push(t);
            }
            rest = after.get(consumed..).unwrap_or_default();
            if let Some((name, args)) = inside.split_first() {
                code = name.as_string();
                let mut joined = String::new();
                for (i, s) in args
                    .iter()
                    .enumerate()
                    .filter_map(|(i, t)| t.as_string().map(|s| (i, s)))
                {
                    if i > 0 {
                        joined.push(' ');
                    }
                    joined.push_str(&s);
                }
                if !joined.is_empty() {
                    code_args = Some(joined);
                }
            }
        }
        let text = match rest.first() {
            Some(Token::QStr(s) | Token::Atom(s)) => s.clone(),
            Some(Token::Literal(b)) => String::from_utf8_lossy(b).into_owned(),
            _ => String::new(),
        };
        Some(StatusLine {
            status,
            code,
            code_args,
            text,
        })
    }
}

#[derive(Debug, Default)]
pub struct ResponseBlock {
    pub data: Vec<Vec<Token>>,
    pub status: StatusLine,
}

impl ResponseBlock {
    pub fn read<R: BufRead>(reader: &mut R) -> Result<ResponseBlock, SieveError> {
        let mut data: Vec<Vec<Token>> = Vec::new();
        loop {
            let line = Token::read_line(reader)?;
            if line.is_empty() {
                continue;
            }
            if let Some(status) = StatusLine::parse(&line) {
                return Ok(ResponseBlock { data, status });
            }
            data.push(line);
        }
    }

    pub fn into_script(self) -> Result<Vec<u8>, SieveError> {
        let mut body: Option<Vec<u8>> = None;
        for token in self.data.into_iter().flatten() {
            let bytes = match token {
                Token::Literal(b) => b,
                Token::QStr(s) => s.into_bytes(),
                _ => continue,
            };
            if body.is_some() {
                return Err(SieveError::Parse(
                    "GETSCRIPT returned more than one body token".into(),
                ));
            }
            body = Some(bytes);
        }
        Ok(body.unwrap_or_default())
    }
}

#[derive(Debug, Default, Clone)]
pub struct ListedScript {
    pub name: String,
    pub active: bool,
}

impl ListedScript {
    pub fn parse_all(data: &[Vec<Token>]) -> Result<Vec<ListedScript>, SieveError> {
        let mut out = Vec::with_capacity(data.len());
        for line in data {
            let Some((first, rest)) = line.split_first() else {
                continue;
            };
            let name = first.as_string().ok_or_else(|| {
                SieveError::Parse(format!("LISTSCRIPTS line missing name: {line:?}"))
            })?;
            let active = rest
                .iter()
                .any(|t| matches!(t, Token::Atom(a) if a.eq_ignore_ascii_case("ACTIVE")));
            out.push(ListedScript { name, active });
        }
        Ok(out)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub implementation: Option<String>,
    pub version: Option<String>,
    pub sasl: Vec<String>,
    pub sieve: Option<String>,
    pub starttls: bool,
    pub maxredirects: Option<u32>,
    pub notify: Option<String>,
    pub language: Option<String>,
    pub owner: Option<String>,
    pub raw: Vec<(String, String)>,
}

#[derive(Clone, Copy)]
enum CapabilityName {
    Implementation,
    Version,
    Sasl,
    Sieve,
    StartTls,
    MaxRedirects,
    Notify,
    Language,
    Owner,
}

impl Capabilities {
    pub fn parse(data: &[Vec<Token>]) -> Capabilities {
        let mut caps = Capabilities::default();
        for line in data {
            let Some((first, values)) = line.split_first() else {
                continue;
            };
            let Some(name) = first.as_string() else {
                continue;
            };
            let mut value = String::new();
            for (i, s) in values.iter().filter_map(Token::as_string).enumerate() {
                if i > 0 {
                    value.push(' ');
                }
                value.push_str(&s);
            }
            let Some(known) = hashify::map_ignore_case!(name.as_bytes(), CapabilityName,
                "IMPLEMENTATION" => CapabilityName::Implementation,
                "VERSION" => CapabilityName::Version,
                "SASL" => CapabilityName::Sasl,
                "SIEVE" => CapabilityName::Sieve,
                "STARTTLS" => CapabilityName::StartTls,
                "MAXREDIRECTS" => CapabilityName::MaxRedirects,
                "NOTIFY" => CapabilityName::Notify,
                "LANGUAGE" => CapabilityName::Language,
                "OWNER" => CapabilityName::Owner,
            ) else {
                caps.raw.push((name.to_ascii_uppercase(), value));
                continue;
            };
            match known {
                CapabilityName::Implementation => caps.implementation = Some(value),
                CapabilityName::Version => caps.version = Some(value),
                CapabilityName::Sasl => {
                    caps.sasl = value
                        .split_ascii_whitespace()
                        .map(str::to_ascii_uppercase)
                        .collect();
                }
                CapabilityName::Sieve => caps.sieve = Some(value),
                CapabilityName::StartTls => caps.starttls = true,
                CapabilityName::MaxRedirects => caps.maxredirects = value.parse().ok(),
                CapabilityName::Notify => caps.notify = Some(value),
                CapabilityName::Language => caps.language = Some(value),
                CapabilityName::Owner => caps.owner = Some(value),
            }
        }
        caps
    }

    pub fn has_sasl(&self, mech: &str) -> bool {
        self.sasl.iter().any(|m| m.eq_ignore_ascii_case(mech))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn cursor(s: &[u8]) -> Cursor<Vec<u8>> {
        Cursor::new(s.to_vec())
    }

    #[test]
    fn parses_greeting_with_quoted_pairs_then_ok() {
        let input =
            b"\"IMPLEMENTATION\" \"Test Server\"\r\n\"VERSION\" \"1.0\"\r\n\"STARTTLS\"\r\n\"SASL\" \"PLAIN LOGIN\"\r\nOK \"hello\"\r\n";
        let mut c = cursor(input);
        let r = ResponseBlock::read(&mut c).unwrap();
        assert_eq!(r.status.status, Status::Ok);
        assert_eq!(r.status.text, "hello");
        let caps = Capabilities::parse(&r.data);
        assert_eq!(caps.implementation, Some("Test Server".to_owned()));
        assert_eq!(caps.version, Some("1.0".to_owned()));
        assert!(caps.starttls);
        assert!(caps.has_sasl("PLAIN"));
        assert!(caps.has_sasl("login"));
        assert!(!caps.has_sasl("XOAUTH2"));
    }

    #[test]
    fn parses_listscripts_with_active_marker() {
        let input = b"\"vacation\"\r\n\"out-of-office\" ACTIVE\r\n\"forward\"\r\nOK\r\n";
        let mut c = cursor(input);
        let r = ResponseBlock::read(&mut c).unwrap();
        assert_eq!(r.status.status, Status::Ok);
        let list = ListedScript::parse_all(&r.data).unwrap();
        assert_eq!(list.len(), 3);
        assert!(!list[0].active);
        assert!(list[1].active);
        assert_eq!(list[1].name, "out-of-office");
        assert!(!list[2].active);
    }

    #[test]
    fn parses_listscripts_with_literal_name_then_active_on_same_logical_line() {
        let input = b"{12+}\r\nfun\xc3\xa7y-name SP ACTIVE\r\nOK\r\n";
        let _ = input;
        let body = b"\x67\xc3\xa9\x6e\xc3\xa9\x72\x69\x71\x75\x65";
        let mut data: Vec<u8> = Vec::new();
        data.extend_from_slice(format!("{{{}}}\r\n", body.len()).as_bytes());
        data.extend_from_slice(body);
        data.extend_from_slice(b" ACTIVE\r\n");
        data.extend_from_slice(b"OK\r\n");
        let mut c = cursor(&data);
        let r = ResponseBlock::read(&mut c).unwrap();
        assert_eq!(r.status.status, Status::Ok);
        let list = ListedScript::parse_all(&r.data).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name.as_bytes(), body);
        assert!(list[0].active);
    }

    #[test]
    fn parses_getscript_literal_body() {
        let script = b"require [\"fileinto\"];\nif true { fileinto \"INBOX\"; }\n";
        let mut data: Vec<u8> = Vec::new();
        data.extend_from_slice(format!("{{{}}}\r\n", script.len()).as_bytes());
        data.extend_from_slice(script);
        data.extend_from_slice(b"\r\nOK\r\n");
        let mut c = cursor(&data);
        let r = ResponseBlock::read(&mut c).unwrap();
        let body = r.into_script().unwrap();
        assert_eq!(body, script);
    }

    #[test]
    fn parses_getscript_quoted_one_line_body() {
        let data = b"\"stop;\"\r\nOK\r\n";
        let mut c = cursor(data);
        let r = ResponseBlock::read(&mut c).unwrap();
        let body = r.into_script().unwrap();
        assert_eq!(body, b"stop;");
    }

    #[test]
    fn parses_no_with_code_and_text() {
        let data = b"NO (NONEXISTENT) \"no such script\"\r\n";
        let mut c = cursor(data);
        let r = ResponseBlock::read(&mut c).unwrap();
        assert_eq!(r.status.status, Status::No);
        assert_eq!(r.status.code.as_deref(), Some("NONEXISTENT"));
        assert_eq!(r.status.text, "no such script");
    }

    #[test]
    fn parses_no_without_code_and_quoted_text() {
        let data = b"NO \"failed\"\r\n";
        let mut c = cursor(data);
        let r = ResponseBlock::read(&mut c).unwrap();
        assert_eq!(r.status.status, Status::No);
        assert!(r.status.code.is_none());
        assert_eq!(r.status.text, "failed");
    }

    #[test]
    fn parses_bye_referral_with_code_args() {
        let data = b"BYE (REFERRAL \"sieve://other.example/\") \"go away\"\r\n";
        let mut c = cursor(data);
        let r = ResponseBlock::read(&mut c).unwrap();
        assert_eq!(r.status.status, Status::Bye);
        assert_eq!(r.status.code.as_deref(), Some("REFERRAL"));
        assert_eq!(r.status.text, "go away");
        assert!(r.status.code_args.unwrap().contains("other.example"));
    }

    #[test]
    fn parses_bare_ok_response() {
        let data = b"OK\r\n";
        let mut c = cursor(data);
        let r = ResponseBlock::read(&mut c).unwrap();
        assert_eq!(r.status.status, Status::Ok);
        assert!(r.status.text.is_empty());
        assert!(r.data.is_empty());
    }

    #[test]
    fn quoted_string_escapes_backslash_and_quote() {
        let data = b"\"a \\\"b\\\\\" \r\nOK\r\n";
        let _ = data;
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"\"a \\\"b\\\\\"\r\nOK\r\n");
        let mut c = cursor(&buf);
        let r = ResponseBlock::read(&mut c).unwrap();
        assert_eq!(r.data.len(), 1);
        match &r.data[0][0] {
            Token::QStr(s) => assert_eq!(s, "a \"b\\"),
            other => panic!("expected QStr, got {other:?}"),
        }
    }

    #[test]
    fn literal_size_overflow_is_parse_error() {
        let data = b"{99999999999999999999}\r\nbody\r\nOK\r\n";
        let mut c = cursor(data);
        let err = ResponseBlock::read(&mut c).unwrap_err();
        assert!(matches!(err, SieveError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn unterminated_quoted_string_is_parse_error() {
        let data = b"\"unterminated\r\n";
        let mut c = cursor(data);
        let err = ResponseBlock::read(&mut c).unwrap_err();
        assert!(matches!(err, SieveError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn capability_atoms_recognised_without_quotes() {
        let data = b"IMPLEMENTATION \"X\"\r\nSTARTTLS\r\nSASL PLAIN\r\nOK\r\n";
        let mut c = cursor(data);
        let r = ResponseBlock::read(&mut c).unwrap();
        let caps = Capabilities::parse(&r.data);
        assert_eq!(caps.implementation.as_deref(), Some("X"));
        assert!(caps.starttls);
        assert!(caps.has_sasl("PLAIN"));
    }

    #[test]
    fn capability_sasl_as_multiple_unquoted_atoms_captures_every_mechanism() {
        let data = b"IMPLEMENTATION \"X\"\r\nSASL PLAIN LOGIN OAUTHBEARER\r\nOK\r\n";
        let mut c = cursor(data);
        let r = ResponseBlock::read(&mut c).unwrap();
        let caps = Capabilities::parse(&r.data);
        assert!(caps.has_sasl("PLAIN"), "PLAIN missing: {:?}", caps.sasl);
        assert!(caps.has_sasl("LOGIN"), "LOGIN missing: {:?}", caps.sasl);
        assert!(
            caps.has_sasl("OAUTHBEARER"),
            "OAUTHBEARER missing: {:?}",
            caps.sasl
        );
    }

    #[test]
    fn capability_sasl_as_single_quoted_string_captures_every_mechanism() {
        let data = b"\"SASL\" \"PLAIN LOGIN OAUTHBEARER\"\r\nOK\r\n";
        let mut c = cursor(data);
        let r = ResponseBlock::read(&mut c).unwrap();
        let caps = Capabilities::parse(&r.data);
        assert!(caps.has_sasl("PLAIN"));
        assert!(caps.has_sasl("LOGIN"));
        assert!(caps.has_sasl("OAUTHBEARER"));
    }

    #[test]
    fn quoted_string_preserves_non_ascii_utf8_bytes() {
        let name_bytes: &[u8] = b"fancy\xc3\xa7y";
        let mut data: Vec<u8> = Vec::new();
        data.push(b'"');
        data.extend_from_slice(name_bytes);
        data.push(b'"');
        data.extend_from_slice(b"\r\nOK\r\n");
        let mut c = cursor(&data);
        let r = ResponseBlock::read(&mut c).unwrap();
        assert_eq!(r.data.len(), 1);
        match &r.data[0][0] {
            Token::QStr(s) => assert_eq!(s.as_bytes(), name_bytes),
            other => panic!("expected QStr, got {other:?}"),
        }
    }

    #[test]
    fn listscripts_quoted_name_with_non_ascii_is_byte_exact() {
        let mut data: Vec<u8> = Vec::new();
        data.extend_from_slice(b"\"fun\xc3\xa7y\"\r\nOK\r\n");
        let mut c = cursor(&data);
        let r = ResponseBlock::read(&mut c).unwrap();
        let list = ListedScript::parse_all(&r.data).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name.as_bytes(), b"fun\xc3\xa7y");
    }

    #[test]
    fn parses_two_logical_lines_with_intervening_blank_skipped() {
        let data = b"\"a\"\r\n\r\n\"b\"\r\nOK\r\n";
        let mut c = cursor(data);
        let r = ResponseBlock::read(&mut c).unwrap();
        assert_eq!(r.data.len(), 2);
    }

    #[test]
    fn oversized_literal_header_fails_without_preallocating() {
        // a hostile {n} must not allocate n bytes before the data arrives
        let mut c = cursor(b"{4294967295}\r\nabc");
        assert!(matches!(
            ResponseBlock::read(&mut c),
            Err(SieveError::Io(_))
        ));
    }
}
