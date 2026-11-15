/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt;
use std::str;

use serde::ser::{Serialize, Serializer};
use serde_json::Value;

const FIELD_SEPARATOR: &[u8] = b"\x1f";
const MESSAGE_ID_SEPARATOR: &str = "\x1f";
const KEY_LEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Key32(pub [u8; KEY_LEN]);

impl Key32 {
    pub const ZERO: Key32 = Key32([0; KEY_LEN]);

    pub fn of_fields(fields: &[&str]) -> Key32 {
        let mut hasher = blake3::Hasher::new();
        let mut fields = fields.iter();
        if let Some(first) = fields.next() {
            hasher.update(first.as_bytes());
        }
        for field in fields {
            hasher.update(FIELD_SEPARATOR);
            hasher.update(field.as_bytes());
        }
        Key32(*hasher.finalize().as_bytes())
    }

    pub fn identity(name: &str, email: &str) -> Key32 {
        Key32::of_fields(&[name, &email.to_lowercase()])
    }

    pub fn participant_identity(calendar_address: &str, name: &str) -> Key32 {
        Key32::of_fields(&[&calendar_address.to_lowercase(), name])
    }

    pub fn from_hex(text: &str) -> Option<Key32> {
        let bytes = text.as_bytes();
        if bytes.len() != KEY_LEN * 2 {
            return None;
        }
        let mut out = [0u8; KEY_LEN];
        for (slot, pair) in out.iter_mut().zip(bytes.as_chunks::<2>().0) {
            *slot = u8::from_str_radix(str::from_utf8(pair).ok()?, 16).ok()?;
        }
        Some(Key32(out))
    }
}

impl fmt::Display for Key32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl Serialize for Key32 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MessageId<'a>(pub &'a str);

impl MessageId<'_> {
    pub fn normalized(self) -> String {
        let t = self.0.trim();
        let t = t.strip_prefix('<').unwrap_or(t);
        let t = t.strip_suffix('>').unwrap_or(t);
        t.trim().to_lowercase()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FoldedName(String);

impl FoldedName {
    pub fn new(name: &str) -> FoldedName {
        FoldedName(name.trim().to_lowercase())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EmailKey {
    MessageId(String),
    Fallback(Key32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailIndex {
    pub mids: Vec<String>,
    pub fb: Key32,
}

#[derive(serde::Serialize)]
struct StoredIndex<'a> {
    f: Key32,
    m: &'a [String],
}

impl EmailIndex {
    pub fn new(
        message_ids: &[String],
        from: &[String],
        subject: &str,
        sent_at: &str,
        to: &[String],
    ) -> EmailIndex {
        let mut mids: Vec<String> = message_ids
            .iter()
            .map(|m| MessageId(m).normalized())
            .filter(|s| !s.is_empty())
            .collect();
        mids.sort_unstable();
        mids.dedup();
        let fb = Key32::of_fields(&[
            &EmailIndex::address_list(from),
            subject,
            sent_at,
            &EmailIndex::address_list(to),
        ]);
        EmailIndex { mids, fb }
    }

    fn address_list(addrs: &[String]) -> String {
        let mut out = String::with_capacity(addrs.iter().map(|a| a.len() + 1).sum());
        for (i, addr) in addrs.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&addr.to_lowercase());
        }
        out
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(&StoredIndex {
            f: self.fb,
            m: &self.mids,
        })
        .unwrap_or_default()
    }

    pub fn from_json(text: &str) -> EmailIndex {
        let v: Value = serde_json::from_str(text).unwrap_or(Value::Null);
        let mids = v
            .get("m")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let fb = v
            .get("f")
            .and_then(Value::as_str)
            .and_then(Key32::from_hex)
            .unwrap_or(Key32::ZERO);
        EmailIndex { mids, fb }
    }

    pub fn key(&self) -> EmailKey {
        if self.mids.is_empty() {
            EmailKey::Fallback(self.fb)
        } else {
            EmailKey::MessageId(self.mids.join(MESSAGE_ID_SEPARATOR))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // address_list matches the old lowercase-and-join, empty entries included
    #[test]
    fn address_list_matches_the_old_join() {
        let reference = |addrs: &[String]| {
            addrs
                .iter()
                .map(|a| a.to_lowercase())
                .collect::<Vec<_>>()
                .join(",")
        };
        let cases: [&[&str]; 7] = [
            &[],
            &[""],
            &["", ""],
            &["", "B@x"],
            &["a@X", ""],
            &["A@x", "", "c@Y"],
            &["ΣΑΣ@x"],
        ];
        for case in cases {
            let addrs: Vec<String> = case.iter().map(|a| (*a).to_owned()).collect();
            assert_eq!(
                EmailIndex::address_list(&addrs),
                reference(&addrs),
                "{case:?}"
            );
        }
    }

    #[test]
    fn message_id_normalization_strips_brackets_and_lowercases() {
        assert_eq!(MessageId("  <ABC@Host.COM> ").normalized(), "abc@host.com");
        assert_eq!(MessageId("plain@x").normalized(), "plain@x");
    }

    #[test]
    fn name_fold_is_case_insensitive() {
        assert_eq!(FoldedName::new(" Sales "), FoldedName::new("sales"));
        assert_ne!(FoldedName::new("Sales"), FoldedName::new("sale"));
    }

    #[test]
    fn identity_collapse_ignores_signature() {
        assert_eq!(
            Key32::identity("Alice", "A@x.test"),
            Key32::identity("Alice", "a@x.test")
        );
    }

    #[test]
    fn participant_identity_key_lowercases_address_only() {
        let a = Key32::participant_identity("MailTo:User@Host", "Bob");
        let b = Key32::participant_identity("mailto:user@host", "Bob");
        assert_eq!(a, b);
        assert_ne!(a, Key32::participant_identity("mailto:user@host", "bob"));
    }

    fn idx(mids: &[&str], from: &[&str], subj: &str, sent: &str, to: &[&str]) -> EmailIndex {
        let m: Vec<String> = mids.iter().map(|s| (*s).to_owned()).collect();
        let f: Vec<String> = from.iter().map(|s| (*s).to_owned()).collect();
        let t: Vec<String> = to.iter().map(|s| (*s).to_owned()).collect();
        EmailIndex::new(&m, &f, subj, sent, &t)
    }

    #[test]
    fn message_id_array_is_normalized_sorted_and_order_independent() {
        let a = idx(&["<B@h>", "<a@H>"], &[], "", "", &[]);
        let b = idx(&["<a@h>", "<b@h>"], &[], "", "", &[]);
        assert_eq!(a.mids, vec!["a@h".to_owned(), "b@h".to_owned()]);
        assert_eq!(a.mids, b.mids);
    }

    #[test]
    fn unique_single_message_id_uses_message_id_key() {
        let keys: Vec<EmailKey> = [
            idx(
                &["<m1@h>"],
                &["a@x"],
                "Hi",
                "2020-01-01T00:00:00Z",
                &["b@y"],
            ),
            idx(
                &["<m2@h>"],
                &["a@x"],
                "Yo",
                "2020-01-02T00:00:00Z",
                &["b@y"],
            ),
        ]
        .iter()
        .map(EmailIndex::key)
        .collect();
        assert_eq!(keys[0], EmailKey::MessageId("m1@h".to_owned()));
        assert_eq!(keys[1], EmailKey::MessageId("m2@h".to_owned()));
    }

    #[test]
    fn shared_message_id_matches_regardless_of_other_fields() {
        let keys: Vec<EmailKey> = [
            idx(
                &["<dup@h>"],
                &["a@x"],
                "S",
                "2020-01-01T00:00:00Z",
                &["b@y"],
            ),
            idx(
                &["<dup@h>"],
                &["c@x"],
                "T",
                "2020-01-02T00:00:00Z",
                &["d@y"],
            ),
        ]
        .iter()
        .map(EmailIndex::key)
        .collect();
        assert_eq!(keys[0], EmailKey::MessageId("dup@h".to_owned()));
        assert_eq!(keys[0], keys[1], "same Message-ID is the same message");
    }

    #[test]
    fn only_absent_message_id_falls_back_multi_is_still_message_id() {
        let keys: Vec<EmailKey> = [
            idx(&[], &["a@x"], "S", "2020-01-01T00:00:00Z", &["b@y"]),
            idx(
                &["<a@h>", "<b@h>"],
                &["a@x"],
                "S",
                "2020-01-01T00:00:00Z",
                &["b@y"],
            ),
        ]
        .iter()
        .map(EmailIndex::key)
        .collect();
        assert!(matches!(keys[0], EmailKey::Fallback(_)));
        assert_eq!(keys[1], EmailKey::MessageId("a@h\u{1f}b@h".to_owned()));
    }

    #[test]
    fn fallback_is_case_insensitive_on_addresses_and_offset_preserving_on_sent_at() {
        let a = idx(&[], &["A@X"], "Subj", "2020-01-01T10:00:00+02:00", &["B@Y"]);
        let b = idx(&[], &["a@x"], "Subj", "2020-01-01T10:00:00+02:00", &["b@y"]);
        let c = idx(&[], &["a@x"], "Subj", "2020-01-01T08:00:00Z", &["b@y"]);
        assert_eq!(a.fb, b.fb);
        assert_ne!(a.fb, c.fb);
    }

    #[test]
    fn index_json_roundtrips() {
        let i = idx(&["<m@h>"], &["a@x"], "S", "2020-01-01T00:00:00Z", &["b@y"]);
        let j = i.to_json();
        let back = EmailIndex::from_json(&j);
        assert_eq!(i, back);
        assert_eq!(i.key(), back.key());
    }

    #[test]
    fn malformed_stored_json_degrades_to_empty_fallback() {
        let back = EmailIndex::from_json("{}");
        assert!(back.mids.is_empty());
        assert_eq!(back.fb, Key32::ZERO);
    }

    // A stored fallback key with multibyte text degrades to the zero key instead of panicking.
    #[test]
    fn non_ascii_fallback_hex_is_rejected_without_panicking() {
        let text = format!("{{\"f\":\"a{}a\",\"m\":[]}}", "\u{e9}".repeat(31));
        assert_eq!(EmailIndex::from_json(&text).fb, Key32::ZERO);
    }

    #[test]
    fn stored_json_layout_is_unchanged() {
        let i = idx(&["<m@h>"], &[], "", "", &[]);
        let hex = i.fb.to_string();
        assert_eq!(i.to_json(), format!("{{\"f\":\"{hex}\",\"m\":[\"m@h\"]}}"));
        assert_eq!(Key32::from_hex(&hex), Some(i.fb));
    }
}
