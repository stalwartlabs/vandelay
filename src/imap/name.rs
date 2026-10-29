/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use encodify::utf7::{self, Utf7};

use super::error::ImapError;

const RECEIVED_NAME: Utf7 = utf7::IMAP.lenient();

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
    if utf8_accept || ends_in_open_shift(input) {
        return Ok(input.to_owned());
    }
    RECEIVED_NAME
        .decode(input)
        .map_err(|e| ImapError::Parse(format!("modified UTF-7 mailbox name: {e}")))
}

fn ends_in_open_shift(name: &str) -> bool {
    name.rsplit_once('&')
        .is_some_and(|(_, tail)| !tail.is_empty() && !tail.contains('-'))
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
    fn raw_ampersand_without_a_closing_dash_keeps_the_name_verbatim() {
        for name in [
            "R&D",
            "AT&T",
            "Tom&Jerry",
            "A&-B&C",
            "R&D Team",
            "&ZeVnLIqe",
            "Envoy&AOk",
        ] {
            assert_eq!(decode_mailbox_name(name).unwrap(), name);
        }
    }

    #[test]
    fn trailing_lone_ampersand_is_literal() {
        assert_eq!(decode_mailbox_name("Sales&").unwrap(), "Sales&");
        assert_eq!(decode_mailbox_name("&AOk-s&").unwrap(), "és&");
        assert_eq!(decode_mailbox_name("&").unwrap(), "&");
    }

    #[test]
    fn encoded_name_with_a_later_escaped_ampersand_decodes() {
        assert_eq!(decode_mailbox_name("Envoy&AOk-s").unwrap(), "Envoyés");
        assert_eq!(decode_mailbox_name("&AOk-/R&-D").unwrap(), "é/R&D");
    }

    #[test]
    fn byte_outside_the_alphabet_inside_a_shift_errors() {
        for name in ["&AOk.s-", "&AOk&-", "&AOké-"] {
            let err = decode_mailbox_name(name).unwrap_err();
            assert!(matches!(err, ImapError::Parse(_)), "{name}: {err:?}");
        }
    }

    #[test]
    fn unpaired_surrogate_errors() {
        let err = decode_mailbox_name("&2D0-").unwrap_err();
        assert!(matches!(err, ImapError::Parse(_)));
    }

    #[test]
    fn encoded_printable_ascii_and_leftover_bits_are_accepted() {
        assert_eq!(decode_mailbox_name("&AGE-").unwrap(), "a");
        assert_eq!(decode_mailbox_name("&AOl-").unwrap(), "é");
        assert_eq!(decode_mailbox_name("&AOk-&AOk-").unwrap(), "éé");
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
            let enc = utf7::IMAP.encode(s);
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
            assert_eq!(decode_mailbox_name(&utf7::IMAP.encode(s)).unwrap(), s);
            assert_eq!(decode_mailbox_name(s).unwrap(), s);
        }
    }

    #[test]
    fn utf8_accept_skips_mutf7_decode_so_ampersand_passes_through() {
        assert_eq!(decode_mailbox_name_with("R&D", true).unwrap(), "R&D");
        assert_eq!(decode_mailbox_name_with("日本語", true).unwrap(), "日本語");
    }
}
