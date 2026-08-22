/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use super::error::ImapError;

const MODIFIED_UTF7_ALPHABET: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+,";

pub fn canonicalise_inbox(name: &str) -> String {
    if name.eq_ignore_ascii_case("INBOX") {
        "INBOX".to_owned()
    } else {
        name.to_owned()
    }
}

pub fn decode_mailbox_name(input: &str) -> Result<String, ImapError> {
    decode_mailbox_name_with(input, false)
}

pub fn decode_mailbox_name_with(input: &str, utf8_accept: bool) -> Result<String, ImapError> {
    if utf8_accept {
        return Ok(input.to_owned());
    }
    if input.is_ascii() && !input.contains('&') {
        return Ok(input.to_owned());
    }
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'&' {
            let start = i;
            while i < bytes.len() && bytes[i] != b'&' {
                i += 1;
            }
            out.push_str(&input[start..i]);
            continue;
        }
        if i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            out.push('&');
            i += 2;
            continue;
        }
        let mut end = i + 1;
        while end < bytes.len() && bytes[end] != b'-' {
            end += 1;
        }
        if end == bytes.len() {
            return Err(ImapError::Parse(
                "unterminated modified UTF-7 sequence".into(),
            ));
        }
        let encoded = &bytes[i + 1..end];
        let decoded = decode_b64_modified(encoded)?;
        let u16s: Vec<u16> = decoded
            .chunks(2)
            .filter(|c| c.len() == 2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        let s = String::from_utf16(&u16s).map_err(|e| ImapError::Parse(format!("utf-16: {e}")))?;
        out.push_str(&s);
        i = end + 1;
    }
    Ok(out)
}

pub fn alternate_mailbox_name(input: &str, utf8_accept: bool) -> Option<String> {
    let primary = encode_mailbox_name_with(input, utf8_accept);
    let alternate = encode_mailbox_name_with(input, !utf8_accept);
    if alternate == primary {
        None
    } else {
        Some(alternate)
    }
}

pub fn encode_mailbox_name(input: &str) -> String {
    encode_mailbox_name_with(input, false)
}

pub fn encode_mailbox_name_with(input: &str, utf8_accept: bool) -> String {
    if utf8_accept {
        return input.to_owned();
    }
    let mut out = String::with_capacity(input.len());
    let mut pending: Vec<u16> = Vec::new();
    let flush = |pending: &mut Vec<u16>, out: &mut String| {
        if pending.is_empty() {
            return;
        }
        out.push('&');
        let mut raw = Vec::with_capacity(pending.len() * 2);
        for w in pending.iter() {
            raw.extend_from_slice(&w.to_be_bytes());
        }
        out.push_str(&encode_b64_modified(&raw));
        out.push('-');
        pending.clear();
    };
    for c in input.chars() {
        let cp = c as u32;
        if c == '&' {
            flush(&mut pending, &mut out);
            out.push_str("&-");
        } else if (0x20..=0x7E).contains(&cp) {
            flush(&mut pending, &mut out);
            out.push(c);
        } else {
            let mut buf = [0u16; 2];
            let units = c.encode_utf16(&mut buf);
            pending.extend_from_slice(units);
        }
    }
    flush(&mut pending, &mut out);
    out
}

fn decode_b64_modified(bytes: &[u8]) -> Result<Vec<u8>, ImapError> {
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &b in bytes {
        let v = match MODIFIED_UTF7_ALPHABET.iter().position(|&x| x == b) {
            Some(idx) => idx as u32,
            None => {
                return Err(ImapError::Parse(format!(
                    "invalid modified UTF-7 byte {b:#04x}"
                )));
            }
        };
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            let byte = ((acc >> bits) & 0xFF) as u8;
            out.push(byte);
        }
    }
    Ok(out)
}

fn encode_b64_modified(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 4 / 3 + 1);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &b in bytes {
        acc = (acc << 8) | b as u32;
        bits += 8;
        while bits >= 6 {
            bits -= 6;
            let idx = ((acc >> bits) & 0x3F) as usize;
            out.push(MODIFIED_UTF7_ALPHABET[idx] as char);
        }
    }
    if bits > 0 {
        let idx = ((acc << (6 - bits)) & 0x3F) as usize;
        out.push(MODIFIED_UTF7_ALPHABET[idx] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_passes_through_decode() {
        assert_eq!(decode_mailbox_name("INBOX").unwrap(), "INBOX");
        assert_eq!(decode_mailbox_name("Sent/Items").unwrap(), "Sent/Items");
    }

    #[test]
    fn ampersand_dash_decodes_to_literal_amp() {
        assert_eq!(decode_mailbox_name("R&-D").unwrap(), "R&D");
        assert_eq!(decode_mailbox_name("&-").unwrap(), "&");
    }

    #[test]
    fn non_ascii_decode_japanese() {
        let decoded = decode_mailbox_name("~peter/mail/&ZeVnLIqe-/&U,BTFw-").unwrap();
        assert_eq!(decoded, "~peter/mail/日本語/台北");
    }

    #[test]
    fn unterminated_sequence_errors() {
        let err = decode_mailbox_name("&ZeVnLIqe").unwrap_err();
        assert!(matches!(err, ImapError::Parse(_)));
    }

    #[test]
    fn ascii_passes_through_encode() {
        assert_eq!(encode_mailbox_name("INBOX"), "INBOX");
        assert_eq!(encode_mailbox_name("Sent Items"), "Sent Items");
    }

    #[test]
    fn literal_amp_encodes_to_amp_dash() {
        assert_eq!(encode_mailbox_name("R&D"), "R&-D");
        assert_eq!(encode_mailbox_name("&"), "&-");
    }

    #[test]
    fn non_ascii_encode_japanese() {
        let encoded = encode_mailbox_name("~peter/mail/日本語/台北");
        assert_eq!(encoded, "~peter/mail/&ZeVnLIqe-/&U,BTFw-");
    }

    #[test]
    fn encode_decode_roundtrip_various() {
        for s in [
            "INBOX",
            "Sent Mail",
            "R&D",
            "日本語",
            "Hé llo",
            "INBOX.Projects.Alpha",
            "Junk E-mail",
        ] {
            let enc = encode_mailbox_name(s);
            let dec = decode_mailbox_name(&enc).unwrap();
            assert_eq!(dec, s, "roundtrip failed for {s:?}, enc={enc:?}");
        }
    }

    #[test]
    fn inbox_canonical_uppercase() {
        assert_eq!(canonicalise_inbox("inbox"), "INBOX");
        assert_eq!(canonicalise_inbox("Inbox"), "INBOX");
        assert_eq!(canonicalise_inbox("INBOX"), "INBOX");
        assert_eq!(canonicalise_inbox("Inbox/Sub"), "Inbox/Sub");
    }

    #[test]
    fn raw_utf8_name_survives_mutf7_decode_untouched() {
        assert_eq!(decode_mailbox_name("Envoyés").unwrap(), "Envoyés");
        assert_eq!(
            decode_mailbox_name("L/Le Vent Se Lève").unwrap(),
            "L/Le Vent Se Lève"
        );
        assert_eq!(decode_mailbox_name("R&-D é").unwrap(), "R&D é");
    }

    #[test]
    fn raw_utf8_name_round_trips_through_encode() {
        for s in ["Envoyés", "Gönderilmiş Postalar", "Çöp kutusu"] {
            assert_eq!(decode_mailbox_name(&encode_mailbox_name(s)).unwrap(), s);
            assert_eq!(decode_mailbox_name(s).unwrap(), s);
        }
    }

    #[test]
    fn alternate_encoding_offered_only_for_non_ascii_names() {
        assert_eq!(
            alternate_mailbox_name("Envoyés", false).as_deref(),
            Some("Envoyés")
        );
        assert_eq!(
            alternate_mailbox_name("Envoyés", true).as_deref(),
            Some("Envoy&AOk-s")
        );
        assert_eq!(alternate_mailbox_name("INBOX", false), None);
        assert_eq!(alternate_mailbox_name("Sent Items", true), None);
    }

    #[test]
    fn utf8_accept_skips_mutf7_decode_so_ampersand_passes_through() {
        assert_eq!(decode_mailbox_name_with("R&D", true).unwrap(), "R&D");
        assert_eq!(decode_mailbox_name_with("日本語", true).unwrap(), "日本語");
    }

    #[test]
    fn utf8_accept_skips_mutf7_encode_so_input_passes_through() {
        assert_eq!(encode_mailbox_name_with("日本語", true), "日本語");
        assert_eq!(encode_mailbox_name_with("R&D", true), "R&D");
    }
}
