/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt::Write as _;

use encodify::base64::STANDARD;

const AUTHENTICATE: &str = "AUTHENTICATE ";
const CRLF: &[u8] = b"\r\n";

pub struct CommandBuilder {
    next_tag: u32,
}

impl Default for CommandBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandBuilder {
    pub fn new() -> Self {
        Self { next_tag: 1 }
    }

    pub fn next_tag(&mut self) -> String {
        let tag = format!("A{:04}", self.next_tag);
        self.next_tag = self.next_tag.wrapping_add(1).max(1);
        tag
    }

    pub fn build(&mut self, command: &str) -> (String, Vec<u8>) {
        let tag = self.next_tag();
        let mut out = String::with_capacity(tag.len() + command.len() + 4);
        out.push_str(&tag);
        out.push(' ');
        out.push_str(command);
        out.push_str("\r\n");
        (tag, out.into_bytes())
    }
}

pub fn quote_astring(s: &str) -> String {
    let needs_literal = s.bytes().any(|b| !(0x20..=0x7e).contains(&b));
    if needs_literal {
        format!("{{{}+}}\r\n{}", s.len(), s)
    } else {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            if c == '"' || c == '\\' {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
        out
    }
}

pub fn contains_literal(bytes: &[u8]) -> bool {
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

pub fn login(user: &str, password: &str) -> String {
    format!("LOGIN {} {}", quote_astring(user), quote_astring(password))
}

pub fn authenticate(mechanism: &str) -> String {
    format!("AUTHENTICATE {mechanism}")
}

pub fn authenticate_with_ir(mechanism: &str, initial_response: &[u8]) -> String {
    let mut out = String::with_capacity(
        AUTHENTICATE.len() + mechanism.len() + 1 + STANDARD.encoded_len(initial_response.len()),
    );
    out.push_str(AUTHENTICATE);
    out.push_str(mechanism);
    out.push(' ');
    STANDARD.encode_append(initial_response, &mut out);
    out
}

pub fn sasl_response(response: &[u8]) -> Vec<u8> {
    let mut line = Vec::with_capacity(STANDARD.encoded_len(response.len()) + CRLF.len());
    STANDARD.encode_append(response, &mut line);
    line.extend_from_slice(CRLF);
    line
}

pub fn capability() -> &'static str {
    "CAPABILITY"
}

pub fn starttls() -> &'static str {
    "STARTTLS"
}

pub fn compress_deflate() -> &'static str {
    "COMPRESS DEFLATE"
}

pub fn noop() -> &'static str {
    "NOOP"
}

pub fn logout() -> &'static str {
    "LOGOUT"
}

pub fn namespace() -> &'static str {
    "NAMESPACE"
}

pub fn enable(extensions: &[&str]) -> String {
    let mut out = String::from("ENABLE");
    for ext in extensions {
        out.push(' ');
        out.push_str(ext);
    }
    out
}

pub fn list(reference: &str, pattern: &str) -> String {
    format!(
        "LIST {} {}",
        quote_astring(reference),
        quote_astring(pattern)
    )
}

pub fn list_extended(
    reference: &str,
    patterns: &[&str],
    selection: &[&str],
    return_options: &[&str],
) -> String {
    let mut out = String::from("LIST");
    if !selection.is_empty() {
        out.push_str(" (");
        for (i, s) in selection.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            out.push_str(s);
        }
        out.push(')');
    }
    out.push(' ');
    out.push_str(&quote_astring(reference));
    if patterns.len() == 1 {
        out.push(' ');
        out.push_str(&quote_astring(patterns[0]));
    } else {
        out.push_str(" (");
        for (i, p) in patterns.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            out.push_str(&quote_astring(p));
        }
        out.push(')');
    }
    if !return_options.is_empty() {
        out.push_str(" RETURN (");
        for (i, r) in return_options.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            out.push_str(r);
        }
        out.push(')');
    }
    out
}

pub fn lsub(reference: &str, pattern: &str) -> String {
    format!(
        "LSUB {} {}",
        quote_astring(reference),
        quote_astring(pattern)
    )
}

pub fn select(mailbox: &str) -> String {
    format!("SELECT {}", quote_astring(mailbox))
}

pub fn examine(mailbox: &str) -> String {
    format!("EXAMINE {}", quote_astring(mailbox))
}

pub fn status(mailbox: &str, items: &[&str]) -> String {
    let mut out = String::from("STATUS ");
    out.push_str(&quote_astring(mailbox));
    out.push_str(" (");
    for (i, s) in items.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(s);
    }
    out.push(')');
    out
}

pub fn uid_search_esearch_all() -> &'static str {
    "UID SEARCH RETURN (ALL) ALL"
}

pub fn uid_search_all() -> &'static str {
    "UID SEARCH ALL"
}

pub fn uid_fetch_all_uids() -> &'static str {
    "UID FETCH 1:* (UID)"
}

pub fn uid_fetch(set: &str, items: &[&str]) -> String {
    let mut out = String::from("UID FETCH ");
    out.push_str(set);
    out.push_str(" (");
    for (i, s) in items.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(s);
    }
    out.push(')');
    out
}

pub fn format_uid_set(uids: &[u32], is_sorted: bool) -> String {
    if uids.is_empty() {
        return String::new();
    }
    let owned_sorted: Vec<u32>;
    let slice: &[u32] = if is_sorted {
        uids
    } else {
        let mut s = uids.to_vec();
        s.sort_unstable();
        s.dedup();
        owned_sorted = s;
        &owned_sorted[..]
    };

    let mut out = String::with_capacity(slice.len() * 8);
    let mut i = 0;
    let mut first = true;
    while i < slice.len() {
        let start = slice[i];
        let mut end = start;
        let mut j = i + 1;
        while j < slice.len() && slice[j] == end + 1 {
            end = slice[j];
            j += 1;
        }
        if !first {
            out.push(',');
        }
        if start == end {
            let _ = write!(&mut out, "{start}");
        } else {
            let _ = write!(&mut out, "{start}:{end}");
        }
        first = false;
        i = j;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_tag_increments() {
        let mut b = CommandBuilder::new();
        assert_eq!(b.next_tag(), "A0001");
        assert_eq!(b.next_tag(), "A0002");
        assert_eq!(b.next_tag(), "A0003");
    }

    #[test]
    fn authenticate_with_ir_appends_the_base64_initial_response() {
        assert_eq!(
            authenticate_with_ir("PLAIN", b"\0foo\0bar"),
            "AUTHENTICATE PLAIN AGZvbwBiYXI="
        );
        assert_eq!(
            authenticate_with_ir("XOAUTH2", b""),
            "AUTHENTICATE XOAUTH2 "
        );
    }

    #[test]
    fn sasl_response_is_base64_with_crlf() {
        assert_eq!(sasl_response(b"\0foo\0bar"), b"AGZvbwBiYXI=\r\n");
        assert_eq!(sasl_response(b""), b"\r\n");
    }

    #[test]
    fn build_appends_crlf() {
        let mut b = CommandBuilder::new();
        let (tag, bytes) = b.build("NOOP");
        assert_eq!(tag, "A0001");
        assert_eq!(bytes, b"A0001 NOOP\r\n");
    }

    #[test]
    fn quote_astring_quotes_safe_text() {
        assert_eq!(quote_astring("hello"), "\"hello\"");
        assert_eq!(quote_astring("INBOX"), "\"INBOX\"");
    }

    #[test]
    fn quote_astring_escapes_quote_and_backslash() {
        assert_eq!(quote_astring("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_astring("a\\b"), "\"a\\\\b\"");
    }

    #[test]
    fn quote_astring_switches_to_literal_for_non_ascii() {
        let q = quote_astring("éclair");
        assert!(q.starts_with("{"));
        assert!(q.contains("\r\n"));
        assert!(q.ends_with("éclair"));
    }

    #[test]
    fn login_command_format() {
        let s = login("alice", "p@ss");
        assert_eq!(s, "LOGIN \"alice\" \"p@ss\"");
    }

    #[test]
    fn list_extended_with_return_clause() {
        let s = list_extended("", &["*"], &[], &["SPECIAL-USE", "SUBSCRIBED"]);
        assert_eq!(s, "LIST \"\" \"*\" RETURN (SPECIAL-USE SUBSCRIBED)");
    }

    #[test]
    fn list_extended_with_selection_and_multi_pattern() {
        let s = list_extended("", &["INBOX", "Sent"], &["SUBSCRIBED"], &[]);
        assert_eq!(s, "LIST (SUBSCRIBED) \"\" (\"INBOX\" \"Sent\")");
    }

    #[test]
    fn status_items_serialise_in_order() {
        let s = status("INBOX", &["UIDVALIDITY", "UIDNEXT", "MESSAGES"]);
        assert_eq!(s, "STATUS \"INBOX\" (UIDVALIDITY UIDNEXT MESSAGES)");
    }

    #[test]
    fn uid_fetch_with_items() {
        let s = uid_fetch("1:100", &["UID", "FLAGS", "INTERNALDATE", "RFC822.SIZE"]);
        assert_eq!(s, "UID FETCH 1:100 (UID FLAGS INTERNALDATE RFC822.SIZE)");
    }

    #[test]
    fn format_uid_set_collapses_consecutive_sorted_input() {
        assert_eq!(format_uid_set(&[], true), "");
        assert_eq!(format_uid_set(&[5], true), "5");
        assert_eq!(format_uid_set(&[1, 2, 3, 5, 7, 8], true), "1:3,5,7:8");
    }

    #[test]
    fn format_uid_set_sorts_when_caller_unsure() {
        assert_eq!(format_uid_set(&[3, 1, 2], false), "1:3");
        assert_eq!(format_uid_set(&[1, 1, 1, 2, 2, 3], false), "1:3");
    }

    #[test]
    fn enable_with_multiple_extensions() {
        assert_eq!(
            enable(&["UTF8=ACCEPT", "CONDSTORE"]),
            "ENABLE UTF8=ACCEPT CONDSTORE"
        );
    }

    #[test]
    fn quote_astring_uses_literal_plus_for_non_ascii() {
        let q = quote_astring("éclair");
        assert!(q.starts_with("{"));
        assert!(q.contains("+}\r\n"));
        assert!(q.ends_with("éclair"));
    }

    #[test]
    fn contains_literal_detects_literal_plus_and_sync() {
        assert!(contains_literal(b"LOGIN \"u\" {6+}\r\nsecret"));
        assert!(contains_literal(b"LOGIN \"u\" {6}\r\nsecret"));
        assert!(!contains_literal(b"LOGIN \"u\" \"p\""));
    }

    #[test]
    fn format_uid_set_trusts_sorted_flag() {
        let s = format_uid_set(&[1, 2, 3, 7, 9], true);
        assert_eq!(s, "1:3,7,9");
    }
}
