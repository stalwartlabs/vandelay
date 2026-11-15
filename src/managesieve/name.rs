/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt;

const MAX_QUOTED_LEN: usize = 1024;

#[derive(Debug, Clone, Copy)]
pub struct SieveString<'a>(pub &'a str);

impl SieveString<'_> {
    pub fn can_be_quoted(self) -> bool {
        let bytes = self.0.as_bytes();
        bytes.len() <= MAX_QUOTED_LEN
            && bytes
                .iter()
                .all(|&b| b == b'\t' || (0x20..=0x7e).contains(&b))
    }
}

impl fmt::Display for SieveString<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.can_be_quoted() {
            f.write_str("\"")?;
            for part in self.0.split_inclusive(['"', '\\']) {
                match part.strip_suffix(['"', '\\']) {
                    Some(head) => {
                        f.write_str(head)?;
                        f.write_str("\\")?;
                        f.write_str(part.get(head.len()..).unwrap_or_default())?;
                    }
                    None => f.write_str(part)?,
                }
            }
            f.write_str("\"")
        } else {
            write!(f, "{{{}+}}\r\n{}", self.0.len(), self.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(s: &str) -> String {
        SieveString(s).to_string()
    }

    #[test]
    fn ascii_round_trips_as_quoted_string() {
        assert_eq!(encoded("vacation"), "\"vacation\"");
        assert_eq!(encoded(""), "\"\"");
    }

    #[test]
    fn quotes_and_backslashes_are_escaped() {
        assert_eq!(encoded("a\"b"), "\"a\\\"b\"");
        assert_eq!(encoded("a\\b"), "\"a\\\\b\"");
    }

    #[test]
    fn non_ascii_promotes_to_literal_with_byte_exact_body() {
        let name = "fancy\u{00e7}y";
        let out = encoded(name);
        let in_bytes = name.as_bytes();
        let header = format!("{{{}+}}\r\n", in_bytes.len());
        assert!(out.starts_with(&header), "got {out:?}");
        let body = &out.as_bytes()[header.len()..];
        assert_eq!(body, in_bytes, "literal body must be byte-exact UTF-8");
        assert_eq!(body.len(), in_bytes.len());
    }

    #[test]
    fn literal_length_matches_payload_byte_count_for_multibyte_runs() {
        for s in [
            "\u{00e7}",
            "na\u{00ef}ve",
            "\u{4e2d}\u{6587}",
            "mix-\u{00e9}\u{00e0}\u{00f6}",
        ] {
            let out = encoded(s);
            let wire = out.as_bytes();
            let header = format!("{{{}+}}\r\n", s.len());
            assert!(
                wire.starts_with(header.as_bytes()),
                "expected header {header:?} for {s:?}, got {out:?}"
            );
            let body = &wire[header.len()..];
            assert_eq!(body, s.as_bytes(), "literal body must equal input bytes");
        }
    }

    #[test]
    fn crlf_in_name_promotes_to_literal() {
        let out = encoded("with\nline");
        assert!(out.starts_with('{'));
        assert!(out.contains("+}\r\n"));
        let header = format!("{{{}+}}\r\n", "with\nline".len());
        assert!(out.starts_with(&header));
    }

    #[test]
    fn tab_is_allowed_in_quoted_string() {
        assert!(SieveString("abc\tdef").can_be_quoted());
    }

    #[test]
    fn nul_byte_forces_literal() {
        assert!(!SieveString("a\0b").can_be_quoted());
    }

    #[test]
    fn names_over_quoted_threshold_use_literal() {
        let long = "x".repeat(2048);
        assert!(!SieveString(&long).can_be_quoted());
    }
}
