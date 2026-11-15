/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::BTreeMap;
use std::fmt;
use std::io::BufRead;

use super::error::{ImapError, NoError};

mod lexer;
mod parser;

use parser::Parser;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

    pub fn into_result(self) -> Result<StatusLine, ImapError> {
        match self.status {
            Status::Ok => Ok(self),
            Status::No => Err(ImapError::No(self.into_no_error())),
            Status::Bad => Err(ImapError::Bad(self.text)),
            Status::Bye => Err(ImapError::Bye(self.text)),
            Status::PreAuth => Err(ImapError::Protocol(
                "unexpected PREAUTH on tagged response".into(),
            )),
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Status::Ok => "OK",
            Status::No => "NO",
            Status::Bad => "BAD",
            Status::Bye => "BYE",
            Status::PreAuth => "PREAUTH",
        })
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

    fn into_name(self) -> Option<String> {
        match self {
            Value::Str(s) | Value::Atom(s) => Some(s),
            Value::Number(n) => Some(n.to_string()),
            Value::Bytes(b) => Some(String::from_utf8_lossy(&b).into_owned()),
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

impl Response {
    pub fn read<R: BufRead>(reader: &mut R) -> Result<Response, ImapError> {
        Parser::new(reader).parse_response()
    }
}

#[cfg(test)]
mod tests {
    use super::lexer::{AtomByte, SeqSet};
    use super::*;
    use std::io::Cursor;

    fn parse(input: &[u8]) -> Response {
        Response::read(&mut Cursor::new(input)).unwrap()
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
    fn untagged_capability_accepts_uri_valued_tokens() {
        let r = parse(
            b"* CAPABILITY IMAP4rev1 SASL-IR SPECIAL-USE IMAPSIEVE=sieve://127.0.0.1:4190 QUOTA ACL RIGHTS=texk\r\n",
        );
        match r {
            Response::Untagged(Untagged::Capability(caps)) => assert_eq!(
                caps,
                vec![
                    "IMAP4rev1",
                    "SASL-IR",
                    "SPECIAL-USE",
                    "IMAPSIEVE=sieve://127.0.0.1:4190",
                    "QUOTA",
                    "ACL",
                    "RIGHTS=texk",
                ]
            ),
            other => panic!("expected Capability, got {other:?}"),
        }
    }

    #[test]
    fn untagged_enabled_accepts_non_atom_tokens() {
        let r = parse(b"* ENABLED UTF8=ACCEPT X-VENDOR=a:b,c\r\n");
        match r {
            Response::Untagged(Untagged::Enabled(exts)) => {
                assert_eq!(exts, vec!["UTF8=ACCEPT", "X-VENDOR=a:b,c"]);
            }
            other => panic!("expected Enabled, got {other:?}"),
        }
    }

    #[test]
    fn atom_accepts_colon_comma_and_brackets() {
        let r = parse(b"* LIST (\\HasNoChildren) \"/\" Work:2026,Q1[a}\r\n");
        match r {
            Response::Untagged(Untagged::List { name, .. }) => {
                assert_eq!(name, "Work:2026,Q1[a}");
            }
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn atom_stops_at_atom_specials() {
        for b in b"(){%\"] \x7f\x00" {
            assert!(!AtomByte::is(*b), "{b:#04x} must not be an atom byte");
        }
        for b in b":,[}=/.-_+" {
            assert!(AtomByte::is(*b), "{:?} must be an atom byte", *b as char);
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
        let r = Response::read(&mut Cursor::new(input)).unwrap();
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
        assert_eq!(SeqSet::expand("1:3,5,7:8").unwrap(), vec![1, 2, 3, 5, 7, 8]);
        assert_eq!(SeqSet::expand("42").unwrap(), vec![42]);
        assert!(SeqSet::expand("").unwrap().is_empty());
    }

    #[test]
    fn empty_read_is_disconnected() {
        let mut empty: &[u8] = b"";
        let err = Response::read(&mut empty).unwrap_err();
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

    #[test]
    fn oversized_literal_header_fails_without_preallocating() {
        let err = Response::read(&mut Cursor::new(
            &b"* 1 FETCH (BODY[] {4294967295}\r\nshort"[..],
        ))
        .unwrap_err();
        assert!(matches!(err, ImapError::Io(_)), "{err:?}");
    }

    #[test]
    fn seq_set_refuses_to_expand_past_the_cap() {
        assert!(SeqSet::expand("1:4294967295").is_err());
        assert_eq!(SeqSet::expand("4294967290:4294967295").unwrap().len(), 6);
    }
}
