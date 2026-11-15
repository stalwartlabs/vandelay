/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use encodify::utf7::{self, Utf7};

use super::error::ImapError;

const RECEIVED_NAME: Utf7 = utf7::IMAP.lenient();
const INBOX: &str = "INBOX";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameEncoding {
    ModifiedUtf7,
    Utf8,
}

impl NameEncoding {
    pub fn mailbox(self, wire: &str) -> Result<String, ImapError> {
        let mut name = self.decode(wire)?;
        if name.eq_ignore_ascii_case(INBOX) {
            name.make_ascii_uppercase();
        }
        Ok(name)
    }

    pub fn decode(self, wire: &str) -> Result<String, ImapError> {
        if self == NameEncoding::Utf8 || NameEncoding::ends_in_open_shift(wire) {
            return Ok(wire.to_owned());
        }
        RECEIVED_NAME
            .decode(wire)
            .map_err(|e| ImapError::Parse(format!("modified UTF-7 mailbox name: {e}")))
    }

    fn ends_in_open_shift(name: &str) -> bool {
        name.rsplit_once('&')
            .is_some_and(|(_, tail)| !tail.is_empty() && !tail.contains('-'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf8_mailbox(wire: &str, _: bool) -> Result<String, ImapError> {
        NameEncoding::Utf8.mailbox(wire)
    }

    fn canonicalise_inbox(name: &str) -> String {
        NameEncoding::Utf8.mailbox(name).unwrap()
    }

    #[test]
    fn ascii_passes_through_decode() {
        assert_eq!(
            NameEncoding::ModifiedUtf7.mailbox("INBOX").unwrap(),
            "INBOX"
        );
        assert_eq!(
            NameEncoding::ModifiedUtf7.mailbox("Sent/Items").unwrap(),
            "Sent/Items"
        );
    }

    #[test]
    fn ampersand_dash_decodes_to_literal_amp() {
        assert_eq!(NameEncoding::ModifiedUtf7.mailbox("R&-D").unwrap(), "R&D");
        assert_eq!(NameEncoding::ModifiedUtf7.mailbox("&-").unwrap(), "&");
    }

    #[test]
    fn non_ascii_decode_japanese() {
        let decoded = NameEncoding::ModifiedUtf7
            .mailbox("~peter/mail/&ZeVnLIqe-/&U,BTFw-")
            .unwrap();
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
            assert_eq!(NameEncoding::ModifiedUtf7.mailbox(name).unwrap(), name);
        }
    }

    #[test]
    fn trailing_lone_ampersand_is_literal() {
        assert_eq!(
            NameEncoding::ModifiedUtf7.mailbox("Sales&").unwrap(),
            "Sales&"
        );
        assert_eq!(
            NameEncoding::ModifiedUtf7.mailbox("&AOk-s&").unwrap(),
            "és&"
        );
        assert_eq!(NameEncoding::ModifiedUtf7.mailbox("&").unwrap(), "&");
    }

    #[test]
    fn encoded_name_with_a_later_escaped_ampersand_decodes() {
        assert_eq!(
            NameEncoding::ModifiedUtf7.mailbox("Envoy&AOk-s").unwrap(),
            "Envoyés"
        );
        assert_eq!(
            NameEncoding::ModifiedUtf7.mailbox("&AOk-/R&-D").unwrap(),
            "é/R&D"
        );
    }

    #[test]
    fn byte_outside_the_alphabet_inside_a_shift_errors() {
        for name in ["&AOk.s-", "&AOk&-", "&AOké-"] {
            let err = NameEncoding::ModifiedUtf7.mailbox(name).unwrap_err();
            assert!(matches!(err, ImapError::Parse(_)), "{name}: {err:?}");
        }
    }

    #[test]
    fn unpaired_surrogate_errors() {
        let err = NameEncoding::ModifiedUtf7.mailbox("&2D0-").unwrap_err();
        assert!(matches!(err, ImapError::Parse(_)));
    }

    #[test]
    fn encoded_printable_ascii_and_leftover_bits_are_accepted() {
        assert_eq!(NameEncoding::ModifiedUtf7.mailbox("&AGE-").unwrap(), "a");
        assert_eq!(NameEncoding::ModifiedUtf7.mailbox("&AOl-").unwrap(), "é");
        assert_eq!(
            NameEncoding::ModifiedUtf7.mailbox("&AOk-&AOk-").unwrap(),
            "éé"
        );
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
            let dec = NameEncoding::ModifiedUtf7.mailbox(&enc).unwrap();
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
        assert_eq!(
            NameEncoding::ModifiedUtf7.mailbox("Envoyés").unwrap(),
            "Envoyés"
        );
        assert_eq!(
            NameEncoding::ModifiedUtf7
                .mailbox("L/Le Vent Se Lève")
                .unwrap(),
            "L/Le Vent Se Lève"
        );
        assert_eq!(
            NameEncoding::ModifiedUtf7.mailbox("R&-D é").unwrap(),
            "R&D é"
        );
    }

    #[test]
    fn raw_utf8_name_round_trips_through_encode() {
        for s in ["Envoyés", "Gönderilmiş Postalar", "Çöp kutusu"] {
            assert_eq!(
                NameEncoding::ModifiedUtf7
                    .mailbox(&utf7::IMAP.encode(s))
                    .unwrap(),
                s
            );
            assert_eq!(NameEncoding::ModifiedUtf7.mailbox(s).unwrap(), s);
        }
    }

    #[test]
    fn utf8_accept_skips_mutf7_decode_so_ampersand_passes_through() {
        assert_eq!(utf8_mailbox("R&D", true).unwrap(), "R&D");
        assert_eq!(utf8_mailbox("日本語", true).unwrap(), "日本語");
    }
}
