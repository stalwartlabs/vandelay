/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt::{self, Write as _};

use encodify::base64::STANDARD;

use crate::net::sasl::SaslPayload;

#[derive(Clone, Copy)]
pub enum Command<'a> {
    Capability,
    StartTls,
    CompressDeflate,
    Noop,
    Logout,
    Namespace,
    UidSearchAll,
    UidSearchEsearchAll,
    UidFetchAllUids,
    Enable(&'a [&'a str]),
    Login {
        user: &'a str,
        password: &'a str,
    },
    Authenticate(&'static str),
    AuthenticateIr(SaslPayload<'a>),
    List {
        reference: &'a str,
        patterns: &'a [&'a str],
        selection: &'a [&'a str],
        returns: &'a [&'a str],
    },
    Lsub {
        reference: &'a str,
        pattern: &'a str,
    },
    Select(&'a str),
    Status {
        mailbox: &'a str,
        items: &'a [&'a str],
    },
    UidFetch {
        set: &'a UidSet,
        items: &'a [&'a str],
    },
    Raw(&'a str),
}

impl Command<'_> {
    pub fn has_literal(text: &str) -> bool {
        let bytes = text.as_bytes();
        text.match_indices('{').any(|(pos, _)| {
            let Some(after) = bytes.get(pos + 1..).filter(|after| after.len() >= 2) else {
                return false;
            };
            let digits = after.iter().take_while(|b| b.is_ascii_digit()).count();
            let rest = after.get(digits..).unwrap_or_default();
            let rest = rest.strip_prefix(b"+").unwrap_or(rest);
            rest.first() == Some(&b'}')
        })
    }

    fn write_list(f: &mut fmt::Formatter<'_>, items: &[&str], quoted: Quote) -> fmt::Result {
        f.write_char('(')?;
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                f.write_char(' ')?;
            }
            match quoted {
                Quote::Astring => write!(f, "{}", Astring(item))?,
                Quote::Bare => f.write_str(item)?,
            }
        }
        f.write_char(')')
    }
}

#[derive(Clone, Copy)]
enum Quote {
    Astring,
    Bare,
}

impl fmt::Display for Command<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Command::Capability => f.write_str("CAPABILITY"),
            Command::StartTls => f.write_str("STARTTLS"),
            Command::CompressDeflate => f.write_str("COMPRESS DEFLATE"),
            Command::Noop => f.write_str("NOOP"),
            Command::Logout => f.write_str("LOGOUT"),
            Command::Namespace => f.write_str("NAMESPACE"),
            Command::UidSearchAll => f.write_str("UID SEARCH ALL"),
            Command::UidSearchEsearchAll => f.write_str("UID SEARCH RETURN (ALL) ALL"),
            Command::UidFetchAllUids => f.write_str("UID FETCH 1:* (UID)"),
            Command::Enable(extensions) => {
                f.write_str("ENABLE")?;
                extensions.iter().try_for_each(|ext| write!(f, " {ext}"))
            }
            Command::Login { user, password } => {
                write!(f, "LOGIN {} {}", Astring(user), Astring(password))
            }
            Command::Authenticate(mechanism) => write!(f, "AUTHENTICATE {mechanism}"),
            Command::AuthenticateIr(payload) => {
                let mut encoded = String::new();
                STANDARD.encode_append(payload.bytes(), &mut encoded);
                write!(f, "AUTHENTICATE {} {encoded}", payload.mechanism())
            }
            Command::List {
                reference,
                patterns,
                selection,
                returns,
            } => {
                f.write_str("LIST")?;
                if !selection.is_empty() {
                    f.write_char(' ')?;
                    Command::write_list(f, selection, Quote::Bare)?;
                }
                write!(f, " {}", Astring(reference))?;
                match patterns {
                    [single] => write!(f, " {}", Astring(single))?,
                    many => {
                        f.write_char(' ')?;
                        Command::write_list(f, many, Quote::Astring)?;
                    }
                }
                if !returns.is_empty() {
                    f.write_str(" RETURN ")?;
                    Command::write_list(f, returns, Quote::Bare)?;
                }
                Ok(())
            }
            Command::Lsub { reference, pattern } => {
                write!(f, "LSUB {} {}", Astring(reference), Astring(pattern))
            }
            Command::Select(mailbox) => write!(f, "SELECT {}", Astring(mailbox)),
            Command::Status { mailbox, items } => {
                write!(f, "STATUS {} ", Astring(mailbox))?;
                Command::write_list(f, items, Quote::Bare)
            }
            Command::UidFetch { set, items } => {
                write!(f, "UID FETCH {} ", set.0)?;
                Command::write_list(f, items, Quote::Bare)
            }
            Command::Raw(text) => f.write_str(text),
        }
    }
}

pub struct Astring<'a>(pub &'a str);

impl fmt::Display for Astring<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.0;
        if s.bytes().any(|b| !(0x20..=0x7e).contains(&b)) {
            return write!(f, "{{{}+}}\r\n{s}", s.len());
        }
        f.write_char('"')?;
        for chunk in s.split_inclusive(['"', '\\']) {
            match chunk.split_at_checked(chunk.len() - 1) {
                Some((head, last @ ("\"" | "\\"))) => {
                    f.write_str(head)?;
                    f.write_char('\\')?;
                    f.write_str(last)?;
                }
                _ => f.write_str(chunk)?,
            }
        }
        f.write_char('"')
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UidSet(String);

impl UidSet {
    pub fn from_sorted(uids: &[u32]) -> UidSet {
        let mut out = String::with_capacity(uids.len() * 8);
        let mut iter = uids.iter().copied().peekable();
        while let Some(start) = iter.next() {
            let mut end = start;
            while let Some(&next) = iter.peek() {
                if Some(next) != end.checked_add(1) {
                    break;
                }
                end = next;
                iter.next();
            }
            if !out.is_empty() {
                out.push(',');
            }
            let _ = if start == end {
                write!(out, "{start}")
            } else {
                write!(out, "{start}:{end}")
            };
        }
        UidSet(out)
    }

    pub fn from_unsorted(uids: &[u32]) -> UidSet {
        let mut sorted = uids.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        UidSet::from_sorted(&sorted)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub struct Tags {
    next: u32,
}

impl Default for Tags {
    fn default() -> Tags {
        Tags { next: 1 }
    }
}

impl Tags {
    pub fn issue(&mut self) -> String {
        let tag = format!("A{:04}", self.next);
        self.next = self.next.wrapping_add(1).max(1);
        tag
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(command: Command<'_>) -> String {
        command.to_string()
    }

    fn reference_uid_set(uids: &[u32]) -> String {
        let mut out = String::new();
        let mut i = 0;
        while i < uids.len() {
            let start = uids[i];
            let mut end = start;
            let mut j = i + 1;
            while j < uids.len() && uids[j] == end + 1 {
                end = uids[j];
                j += 1;
            }
            if !out.is_empty() {
                out.push(',');
            }
            if start == end {
                out.push_str(&start.to_string());
            } else {
                out.push_str(&format!("{start}:{end}"));
            }
            i = j;
        }
        out
    }

    fn reference_has_literal(bytes: &[u8]) -> bool {
        let mut i = 0;
        while i + 2 < bytes.len() {
            if bytes[i] == b'{' {
                let mut j = i + 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'+' {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'}' {
                    return true;
                }
            }
            i += 1;
        }
        false
    }

    #[test]
    fn tags_increment() {
        let mut tags = Tags::default();
        assert_eq!(tags.issue(), "A0001");
        assert_eq!(tags.issue(), "A0002");
        assert_eq!(tags.issue(), "A0003");
    }

    #[test]
    fn authenticate_with_ir_appends_the_base64_initial_response() {
        assert_eq!(
            text(Command::AuthenticateIr(SaslPayload::Plain {
                authcid: "foo",
                password: "bar"
            })),
            "AUTHENTICATE PLAIN AGZvbwBiYXI="
        );
    }

    #[test]
    fn astring_quotes_escapes_or_switches_to_literal() {
        assert_eq!(Astring("hello").to_string(), "\"hello\"");
        assert_eq!(Astring("INBOX").to_string(), "\"INBOX\"");
        assert_eq!(Astring("a\"b").to_string(), "\"a\\\"b\"");
        assert_eq!(Astring("a\\b").to_string(), "\"a\\\\b\"");
        assert_eq!(Astring("\"\\").to_string(), "\"\\\"\\\\\"");
        assert_eq!(Astring("éclair").to_string(), "{7+}\r\néclair");
    }

    #[test]
    fn login_command_format() {
        assert_eq!(
            text(Command::Login {
                user: "alice",
                password: "p@ss"
            }),
            "LOGIN \"alice\" \"p@ss\""
        );
    }

    #[test]
    fn list_forms() {
        let list = |patterns, selection, returns| {
            text(Command::List {
                reference: "",
                patterns,
                selection,
                returns,
            })
        };
        assert_eq!(list(&["*"], &[], &[]), "LIST \"\" \"*\"");
        assert_eq!(
            list(&["*"], &[], &["SPECIAL-USE", "SUBSCRIBED"]),
            "LIST \"\" \"*\" RETURN (SPECIAL-USE SUBSCRIBED)"
        );
        assert_eq!(
            list(&["INBOX", "Sent"], &["SUBSCRIBED"], &[]),
            "LIST (SUBSCRIBED) \"\" (\"INBOX\" \"Sent\")"
        );
        assert_eq!(
            text(Command::Lsub {
                reference: "",
                pattern: "*"
            }),
            "LSUB \"\" \"*\""
        );
    }

    #[test]
    fn status_fetch_enable_select() {
        assert_eq!(
            text(Command::Status {
                mailbox: "INBOX",
                items: &["UIDVALIDITY", "UIDNEXT", "MESSAGES"]
            }),
            "STATUS \"INBOX\" (UIDVALIDITY UIDNEXT MESSAGES)"
        );
        let set = UidSet::from_sorted(&[1, 2, 3, 100]);
        assert_eq!(
            text(Command::UidFetch {
                set: &set,
                items: &["UID", "FLAGS"]
            }),
            "UID FETCH 1:3,100 (UID FLAGS)"
        );
        let uids: Vec<u32> = (1..=100).collect();
        let set = UidSet::from_sorted(&uids);
        assert_eq!(
            text(Command::UidFetch {
                set: &set,
                items: &["UID", "FLAGS", "INTERNALDATE", "RFC822.SIZE"]
            }),
            "UID FETCH 1:100 (UID FLAGS INTERNALDATE RFC822.SIZE)"
        );
        assert_eq!(
            text(Command::Enable(&["UTF8=ACCEPT", "CONDSTORE"])),
            "ENABLE UTF8=ACCEPT CONDSTORE"
        );
        assert_eq!(text(Command::Select("INBOX")), "SELECT \"INBOX\"");
    }

    #[test]
    fn uid_set_collapses_and_sorts() {
        assert_eq!(UidSet::from_sorted(&[]).as_str(), "");
        assert_eq!(UidSet::from_sorted(&[5]).as_str(), "5");
        assert_eq!(
            UidSet::from_sorted(&[1, 2, 3, 5, 7, 8]).as_str(),
            "1:3,5,7:8"
        );
        assert_eq!(UidSet::from_sorted(&[1, 2, 3, 7, 9]).as_str(), "1:3,7,9");
        assert_eq!(UidSet::from_unsorted(&[3, 1, 2]).as_str(), "1:3");
        assert_eq!(UidSet::from_unsorted(&[1, 1, 1, 2, 2, 3]).as_str(), "1:3");
        assert_eq!(
            UidSet::from_sorted(&[u32::MAX - 1, u32::MAX]).as_str(),
            "4294967294:4294967295"
        );
    }

    #[test]
    fn uid_set_matches_reference() {
        let mut uids = Vec::new();
        let mut x: u32 = 7;
        for _ in 0..2000 {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            uids.push(x % 600);
        }
        uids.sort_unstable();
        uids.dedup();
        for window in [1, 2, 3, 17, uids.len()] {
            for chunk in uids.chunks(window) {
                assert_eq!(
                    UidSet::from_sorted(chunk).as_str(),
                    reference_uid_set(chunk)
                );
            }
        }
    }

    #[test]
    fn has_literal_matches_reference() {
        for input in [
            &b"LOGIN \"u\" {6+}\r\nsecret"[..],
            b"LOGIN \"u\" {6}\r\nsecret",
            b"LOGIN \"u\" \"p\"",
            b"{}",
            b"{+}",
            b"x{",
            b"{1",
            b"{1}",
            b"ab{12+",
            b"{{}",
            b"",
        ] {
            let text = std::str::from_utf8(input).unwrap();
            assert_eq!(
                Command::has_literal(text),
                reference_has_literal(input),
                "{text:?}"
            );
        }
    }
}
