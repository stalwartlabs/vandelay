/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::slice;

use mail_parser::{Address, HeaderName, HeaderValue, Message, MessageParser};

use crate::sync::keys::{EmailIndex, email_index};

fn addrs(a: Option<&Address>) -> Vec<String> {
    match a {
        Some(addr) => addr
            .iter()
            .filter_map(|x| x.address().map(|s| s.to_owned()))
            .collect(),
        None => Vec::new(),
    }
}

fn message_ids(value: &HeaderValue) -> Vec<String> {
    match value {
        HeaderValue::Text(s) => vec![s.to_string()],
        HeaderValue::TextList(l) => l.iter().map(|s| s.to_string()).collect(),
        _ => Vec::new(),
    }
}

pub fn email_index_from_blob(bytes: &[u8]) -> EmailIndex {
    email_meta_from_blob(bytes).0
}

const MAX_KEYWORD_LEN: usize = 255;
const MAX_MIME_KEYWORDS: usize = 64;

pub fn email_index_and_mime_keywords(bytes: &[u8], keywords: &mut Vec<String>) -> EmailIndex {
    let Some(m) = MessageParser::default().parse(bytes) else {
        return empty_meta().0;
    };
    add_keywords_header(&m, keywords);
    email_meta(&m).0
}

pub fn add_mime_keywords(bytes: &[u8], keywords: &mut Vec<String>) {
    if let Some(m) = MessageParser::default().parse_headers(bytes) {
        add_keywords_header(&m, keywords);
    }
}

fn add_keywords_header(m: &Message<'_>, keywords: &mut Vec<String>) {
    let mut added = 0;
    for value in m.header_values(HeaderName::Keywords) {
        let phrases = match value {
            HeaderValue::Text(phrase) => slice::from_ref(phrase),
            HeaderValue::TextList(phrases) => phrases.as_slice(),
            _ => &[],
        };
        for keyword in phrases.iter().filter_map(|phrase| jmap_keyword(phrase)) {
            if !keywords.iter().any(|k| k.eq_ignore_ascii_case(&keyword)) {
                keywords.push(keyword);
                added += 1;
                if added == MAX_MIME_KEYWORDS {
                    return;
                }
            }
        }
    }
}

fn jmap_keyword(phrase: &str) -> Option<String> {
    let mut keyword = String::with_capacity(phrase.len().min(MAX_KEYWORD_LEN));
    let mut separator = false;
    for ch in phrase.chars() {
        if ch.is_ascii_graphic()
            && !matches!(ch, '(' | ')' | '{' | ']' | '%' | '*' | '"' | '\\')
            && !(ch == '$' && keyword.is_empty())
        {
            let sep = separator && !keyword.is_empty();
            if keyword.len() + usize::from(sep) >= MAX_KEYWORD_LEN {
                break;
            }
            if sep {
                keyword.push('_');
            }
            keyword.push(ch.to_ascii_lowercase());
            separator = false;
        } else {
            separator = true;
        }
    }
    (!keyword.is_empty()).then_some(keyword)
}

fn empty_meta() -> (EmailIndex, Option<String>) {
    (email_index(&[], &[], "", "", &[]), None)
}

pub fn email_meta_from_blob(bytes: &[u8]) -> (EmailIndex, Option<String>) {
    MessageParser::default()
        .parse(bytes)
        .map_or_else(empty_meta, |m| email_meta(&m))
}

fn email_meta(m: &Message<'_>) -> (EmailIndex, Option<String>) {
    let mut mids: Vec<String> = Vec::new();
    for v in m.header_values(HeaderName::MessageId) {
        mids.extend(message_ids(v));
    }
    if mids.is_empty()
        && let Some(single) = m.message_id()
    {
        mids.push(single.to_owned());
    }
    let date = m.date().map(|d| d.to_rfc3339());
    let idx = email_index(
        &mids,
        &addrs(m.from()),
        m.subject().unwrap_or(""),
        date.as_deref().unwrap_or(""),
        &addrs(m.to()),
    );
    (idx, date)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::keys::{EmailKey, email_key};

    // Every Keywords header and every comma-separated phrase is mapped to a JMAP keyword.
    #[test]
    fn mime_keywords_are_read_from_every_keywords_header() {
        let mut keywords = vec!["$seen".to_owned(), "VIP".to_owned()];
        let raw = b"Message-ID: <k@h>\r\nKeywords: bearbeitet von KL, Wiedervorlage\r\nKeywords: vip, =?UTF-8?Q?Pr=C3=BCfung?=\r\n\r\nbody";
        let idx = email_index_and_mime_keywords(raw, &mut keywords);
        assert_eq!(idx.mids, vec!["k@h".to_owned()]);
        assert_eq!(
            keywords,
            vec![
                "$seen",
                "VIP",
                "bearbeitet_von_kl",
                "wiedervorlage",
                "pr_fung"
            ]
        );

        let mut from_headers = vec!["$seen".to_owned(), "VIP".to_owned()];
        add_mime_keywords(raw, &mut from_headers);
        assert_eq!(from_headers, keywords);
    }

    // Phrases are reduced to the RFC 8621 keyword alphabet, without a leading "$", within 255 chars.
    #[test]
    fn jmap_keyword_follows_rfc8621_syntax() {
        assert_eq!(
            jmap_keyword("  Review / Legal  ").as_deref(),
            Some("review_/_legal")
        );
        assert_eq!(
            jmap_keyword("a(b)c{d]e%f*g\"h\\i").as_deref(),
            Some("a_b_c_d_e_f_g_h_i")
        );
        assert_eq!(jmap_keyword("$seen").as_deref(), Some("seen"));
        assert_eq!(jmap_keyword(" $$Junk").as_deref(), Some("junk"));
        assert_eq!(jmap_keyword("a$b").as_deref(), Some("a$b"));
        assert_eq!(
            jmap_keyword("Q4-2026.final_v2").as_deref(),
            Some("q4-2026.final_v2")
        );
        assert_eq!(jmap_keyword(" ( ) "), None);
        assert_eq!(
            jmap_keyword("x".repeat(300).as_str()).map(|k| k.len()),
            Some(255)
        );
        assert_eq!(
            jmap_keyword(&format!("{} b", "a".repeat(254))).map(|k| k.len()),
            Some(254)
        );
    }

    // Empty phrases, messages without the header and the keyword count cap.
    #[test]
    fn mime_keywords_skip_empty_and_are_capped() {
        let mut keywords = Vec::new();
        add_mime_keywords(b"Keywords: ,  \r\n\r\nbody", &mut keywords);
        add_mime_keywords(b"Subject: none\r\n\r\nbody", &mut keywords);
        assert!(keywords.is_empty());

        let many = (0..100)
            .map(|n| format!("k{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        add_mime_keywords(
            format!("Keywords: {many}\r\n\r\nbody").as_bytes(),
            &mut keywords,
        );
        assert_eq!(keywords.len(), MAX_MIME_KEYWORDS);
    }

    #[test]
    fn parses_core_fields_and_keys_by_message_id() {
        let raw = b"From: Alice <a@x.test>\r\nTo: Bob <b@y.test>\r\nSubject: Hi there\r\nMessage-ID: <abc-1@host>\r\nDate: Mon, 12 May 2025 10:00:00 +0200\r\n\r\nbody\r\n";
        let idx = email_index_from_blob(raw);
        assert_eq!(idx.mids, vec!["abc-1@host".to_owned()]);
        assert_eq!(
            email_key(&idx),
            EmailKey::MessageId("abc-1@host".to_owned())
        );
    }

    #[test]
    fn sent_at_offset_is_preserved_then_z_for_utc() {
        let off = email_index_from_blob(
            b"Message-ID: <a@h>\r\nDate: Mon, 12 May 2025 10:00:00 +0200\r\n\r\nx",
        );
        let utc = email_index_from_blob(
            b"Message-ID: <b@h>\r\nDate: Mon, 12 May 2025 08:00:00 +0000\r\n\r\nx",
        );

        assert_ne!(off.fb, utc.fb);
    }

    #[test]
    fn missing_message_id_falls_back_to_hash() {
        let idx = email_index_from_blob(b"From: a@x\r\nSubject: only\r\n\r\nbody");
        assert!(idx.mids.is_empty());
        assert!(matches!(email_key(&idx), EmailKey::Fallback(_)));
    }

    #[test]
    fn multiple_message_id_values_are_all_captured() {
        let idx =
            email_index_from_blob(b"Message-ID: <a@h>\r\nMessage-ID: <b@h>\r\nSubject: s\r\n\r\nx");
        assert_eq!(idx.mids, vec!["a@h".to_owned(), "b@h".to_owned()]);
        assert_eq!(
            email_key(&idx),
            EmailKey::MessageId("a@h\u{1f}b@h".to_owned())
        );
    }

    #[test]
    fn same_message_different_received_metadata_same_key() {
        let a = email_index_from_blob(
            b"From: a@x\r\nSubject: S\r\nMessage-ID: <dup@h>\r\nDate: Mon, 12 May 2025 10:00:00 +0200\r\n\r\nbody",
        );
        let b = email_index_from_blob(
            b"From: a@x\r\nSubject: S\r\nMessage-ID: <DUP@h>\r\nDate: Tue, 13 May 2025 11:00:00 +0000\r\n\r\nbody",
        );
        assert_eq!(email_key(&a), email_key(&b));
    }
}
