/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use mail_parser::{Address, HeaderName, HeaderValue, MessageParser};

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

pub fn mime_keywords_from_blob(bytes: &[u8]) -> Vec<String> {
    let Some(message) = MessageParser::default().parse(bytes) else {
        return Vec::new();
    };
    let mut keywords = Vec::new();
    for value in message.header_values(HeaderName::Other("Keywords".into())) {
        match value {
            HeaderValue::Text(value) => add_mime_keywords(value.as_ref(), &mut keywords),
            HeaderValue::TextList(values) => {
                for value in values {
                    add_mime_keywords(value.as_ref(), &mut keywords);
                }
            }
            _ => {}
        }
    }
    keywords
}

fn add_mime_keywords(value: &str, keywords: &mut Vec<String>) {
    for keyword in value.split(',').map(str::trim).filter(|v| !v.is_empty()) {
        if !keywords
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(keyword))
        {
            keywords.push(keyword.to_owned());
        }
    }
}

pub fn email_meta_from_blob(bytes: &[u8]) -> (EmailIndex, Option<String>) {
    let Some(m) = MessageParser::default().parse(bytes) else {
        return (email_index(&[], &[], "", "", &[]), None);
    };
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
    fn parses_mime_keywords() {
        let keywords = mime_keywords_from_blob(
            b"Keywords: bearbeitet von KL, Wiedervorlage\r\nKeywords: VIP\r\n\r\nbody",
        );
        assert_eq!(keywords, vec!["bearbeitet von KL", "Wiedervorlage", "VIP"]);
    }

    #[test]
    fn ignores_empty_mime_keywords() {
        let keywords = mime_keywords_from_blob(b"Keywords: ,  \r\n\r\nbody");
        assert!(keywords.is_empty());
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
