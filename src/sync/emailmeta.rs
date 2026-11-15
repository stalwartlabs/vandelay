/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use mail_parser::{AddressList, HeaderName, HeaderValue, Message, MessageParser};

use crate::sync::keys::EmailIndex;
use crate::sync::keywords::Keywords;

const MAX_MIME_KEYWORDS: usize = 64;

#[derive(Debug, Clone)]
pub struct EmailMeta {
    pub index: EmailIndex,
    pub date: Option<String>,
}

impl EmailMeta {
    pub fn from_blob(bytes: &[u8]) -> EmailMeta {
        MessageParser::default()
            .parse(bytes)
            .map_or_else(EmailMeta::empty, |m| EmailMeta::from_message(&m))
    }

    pub fn index_with_mime_keywords(bytes: &[u8], keywords: &mut Keywords) -> EmailIndex {
        let Some(m) = MessageParser::default().parse(bytes) else {
            return EmailMeta::empty().index;
        };
        EmailMeta::push_keywords_header(&m, keywords);
        EmailMeta::from_message(&m).index
    }

    pub fn add_mime_keywords(bytes: &[u8], keywords: &mut Keywords) {
        if let Some(m) = MessageParser::default().parse_headers(bytes) {
            EmailMeta::push_keywords_header(&m, keywords);
        }
    }

    fn empty() -> EmailMeta {
        EmailMeta {
            index: EmailIndex::new(&[], &[], "", "", &[]),
            date: None,
        }
    }

    fn from_message(m: &Message<'_>) -> EmailMeta {
        let mut mids: Vec<String> = Vec::new();
        for header in m.headers().all(HeaderName::MessageId) {
            if let HeaderValue::TextList(ids) = header.value() {
                mids.extend(ids.iter().map(str::to_owned));
            }
        }
        let date = m.date().map(|d| d.to_rfc3339());
        let index = EmailIndex::new(
            &mids,
            &EmailMeta::addresses(m.from()),
            m.subject().unwrap_or(""),
            date.as_deref().unwrap_or(""),
            &EmailMeta::addresses(m.to()),
        );
        EmailMeta { index, date }
    }

    fn addresses(list: Option<AddressList<'_>>) -> Vec<String> {
        list.map_or_else(Vec::new, |list| {
            list.mailboxes()
                .filter_map(|mailbox| mailbox.address().map(str::to_owned))
                .collect()
        })
    }

    fn push_keywords_header(m: &Message<'_>, keywords: &mut Keywords) {
        let mut added = 0;
        for header in m.headers().all(HeaderName::Keywords) {
            let HeaderValue::TextList(phrases) = header.value() else {
                continue;
            };
            for keyword in phrases.iter().filter_map(Keywords::from_phrase) {
                if keywords.insert(keyword) {
                    added += 1;
                    if added == MAX_MIME_KEYWORDS {
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::keys::EmailKey;

    // Every Keywords header and every comma-separated phrase is mapped to a JMAP keyword.
    #[test]
    fn mime_keywords_are_read_from_every_keywords_header() {
        let raw = b"Message-ID: <k@h>\r\nKeywords: bearbeitet von KL, Wiedervorlage\r\nKeywords: vip, =?UTF-8?Q?Pr=C3=BCfung?=\r\n\r\nbody";
        let mut keywords = Keywords::new();
        keywords.push("$seen");
        keywords.push("VIP");
        let idx = EmailMeta::index_with_mime_keywords(raw, &mut keywords);
        assert_eq!(idx.mids, vec!["k@h".to_owned()]);
        assert!(keywords.iter().eq([
            "$seen",
            "VIP",
            "bearbeitet_von_kl",
            "wiedervorlage",
            "pr_fung"
        ]));

        let mut typed_headers = Keywords::new();
        typed_headers.push("$seen");
        typed_headers.push("VIP");
        EmailMeta::add_mime_keywords(raw, &mut typed_headers);
        assert_eq!(typed_headers, keywords);
    }

    // Phrases are reduced to the RFC 8621 keyword alphabet, without a leading "$", within 255 chars.
    #[test]
    fn jmap_keyword_follows_rfc8621_syntax() {
        assert_eq!(
            Keywords::from_phrase("  Review / Legal  ").as_deref(),
            Some("review_/_legal")
        );
        assert_eq!(
            Keywords::from_phrase("a(b)c{d]e%f*g\"h\\i").as_deref(),
            Some("a_b_c_d_e_f_g_h_i")
        );
        assert_eq!(Keywords::from_phrase("$seen").as_deref(), Some("seen"));
        assert_eq!(Keywords::from_phrase(" $$Junk").as_deref(), Some("junk"));
        assert_eq!(Keywords::from_phrase("a$b").as_deref(), Some("a$b"));
        assert_eq!(
            Keywords::from_phrase("Q4-2026.final_v2").as_deref(),
            Some("q4-2026.final_v2")
        );
        assert_eq!(Keywords::from_phrase(" ( ) "), None);
        assert_eq!(
            Keywords::from_phrase("x".repeat(300).as_str()).map(|k| k.len()),
            Some(255)
        );
        assert_eq!(
            Keywords::from_phrase(&format!("{} b", "a".repeat(254))).map(|k| k.len()),
            Some(254)
        );
    }

    // Empty phrases, messages without the header and the keyword count cap.
    #[test]
    fn mime_keywords_skip_empty_and_are_capped() {
        let mut keywords = Keywords::new();
        EmailMeta::add_mime_keywords(b"Keywords: ,  \r\n\r\nbody", &mut keywords);
        EmailMeta::add_mime_keywords(b"Subject: none\r\n\r\nbody", &mut keywords);
        assert!(keywords.is_empty());

        let many = (0..100)
            .map(|n| format!("k{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        EmailMeta::add_mime_keywords(
            format!("Keywords: {many}\r\n\r\nbody").as_bytes(),
            &mut keywords,
        );
        assert_eq!(keywords.len(), MAX_MIME_KEYWORDS);
    }

    #[test]
    fn parses_core_fields_and_keys_by_message_id() {
        let raw = b"From: Alice <a@x.test>\r\nTo: Bob <b@y.test>\r\nSubject: Hi there\r\nMessage-ID: <abc-1@host>\r\nDate: Mon, 12 May 2025 10:00:00 +0200\r\n\r\nbody\r\n";
        let idx = EmailMeta::from_blob(raw).index;
        assert_eq!(idx.mids, vec!["abc-1@host".to_owned()]);
        assert_eq!(idx.key(), EmailKey::MessageId("abc-1@host".to_owned()));
    }

    #[test]
    fn sent_at_offset_is_preserved_then_z_for_utc() {
        let off = EmailMeta::from_blob(
            b"Message-ID: <a@h>\r\nDate: Mon, 12 May 2025 10:00:00 +0200\r\n\r\nx",
        )
        .index;
        let utc = EmailMeta::from_blob(
            b"Message-ID: <b@h>\r\nDate: Mon, 12 May 2025 08:00:00 +0000\r\n\r\nx",
        )
        .index;

        assert_ne!(off.fb, utc.fb);
    }

    #[test]
    fn missing_message_id_falls_back_to_hash() {
        let idx = EmailMeta::from_blob(b"From: a@x\r\nSubject: only\r\n\r\nbody").index;
        assert!(idx.mids.is_empty());
        assert!(matches!(idx.key(), EmailKey::Fallback(_)));
    }

    #[test]
    fn multiple_message_id_values_are_all_captured() {
        let idx =
            EmailMeta::from_blob(b"Message-ID: <a@h>\r\nMessage-ID: <b@h>\r\nSubject: s\r\n\r\nx")
                .index;
        assert_eq!(idx.mids, vec!["a@h".to_owned(), "b@h".to_owned()]);
        assert_eq!(idx.key(), EmailKey::MessageId("a@h\u{1f}b@h".to_owned()));
    }

    // A multipart header with no body made mail-parser 0.11 panic.
    #[test]
    fn truncated_multipart_does_not_panic() {
        let idx = EmailMeta::from_blob(b"d\nContent-Type:multipart;boundary=b5").index;
        assert!(idx.mids.is_empty());
    }

    #[test]
    fn same_message_different_received_metadata_same_key() {
        let a = EmailMeta::from_blob(
            b"From: a@x\r\nSubject: S\r\nMessage-ID: <dup@h>\r\nDate: Mon, 12 May 2025 10:00:00 +0200\r\n\r\nbody",
        ).index;
        let b = EmailMeta::from_blob(
            b"From: a@x\r\nSubject: S\r\nMessage-ID: <DUP@h>\r\nDate: Tue, 13 May 2025 11:00:00 +0000\r\n\r\nbody",
        ).index;
        assert_eq!(a.key(), b.key());
    }
}
