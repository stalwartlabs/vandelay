/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use mail_parser::mailbox::maildir::Flag;
use time::OffsetDateTime;

use crate::import::imap::DeletedMessages;
use crate::sync::keywords::Keywords;

const MAX_DELIVERY_DIGITS: usize = 12;
const DELIVERY_CLOCK_SKEW_SECS: u64 = 86_400;

#[derive(Debug, Default)]
pub(super) struct Translation {
    pub keywords: Keywords,
    pub has_trashed_flag: bool,
}

impl Translation {
    pub fn from_flags(flags: &[Flag], deleted: DeletedMessages) -> Translation {
        let mut out = Translation {
            keywords: Keywords::with_capacity(flags.len()),
            has_trashed_flag: false,
        };
        for flag in flags {
            match flag {
                Flag::Passed => out.keywords.push("$forwarded"),
                Flag::Replied => out.keywords.push("$answered"),
                Flag::Seen => out.keywords.push("$seen"),
                Flag::Draft => out.keywords.push("$draft"),
                Flag::Flagged => out.keywords.push("$flagged"),
                Flag::Trashed => {
                    out.has_trashed_flag = true;
                    if deleted == DeletedMessages::Keep {
                        out.keywords.push("$deleted");
                    }
                }
            }
        }
        out.keywords.sort();
        out
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct MaildirFilename<'a>(pub &'a str);

impl<'a> MaildirFilename<'a> {
    pub fn flags(self) -> Vec<Flag> {
        let Some((_, info)) = self.0.rsplit_once("2,") else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for &ch in info.as_bytes() {
            match ch {
                b'P' => out.push(Flag::Passed),
                b'R' => out.push(Flag::Replied),
                b'S' => out.push(Flag::Seen),
                b'T' => out.push(Flag::Trashed),
                b'D' => out.push(Flag::Draft),
                b'F' => out.push(Flag::Flagged),
                other if !other.is_ascii_alphanumeric() => break,
                _ => {}
            }
        }
        out
    }

    pub fn unique_id(self) -> &'a str {
        self.0.split_once(':').map_or(self.0, |(unique, _)| unique)
    }

    pub fn delivery_time(self) -> Option<u64> {
        let digits = self
            .0
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap_or_default();
        if digits.is_empty() || digits.len() > MAX_DELIVERY_DIGITS {
            return None;
        }
        let secs: u64 = digits.parse().ok()?;
        if secs == 0 {
            return None;
        }
        let now = u64::try_from(OffsetDateTime::now_utc().unix_timestamp()).unwrap_or(0);
        if secs > now.saturating_add(DELIVERY_CLOCK_SKEW_SECS) {
            return None;
        }
        Some(secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_flags_yields_empty_keywords() {
        let t = Translation::from_flags(&[], DeletedMessages::Skip);
        assert!(t.keywords.is_empty());
        assert!(!t.has_trashed_flag);
    }

    #[test]
    fn each_flag_maps_to_expected_jmap_keyword() {
        let t = Translation::from_flags(
            &[
                Flag::Passed,
                Flag::Replied,
                Flag::Seen,
                Flag::Draft,
                Flag::Flagged,
            ],
            DeletedMessages::Skip,
        );
        let mut expected = vec!["$forwarded", "$answered", "$seen", "$draft", "$flagged"];
        expected.sort();
        assert_eq!(t.keywords.iter().collect::<Vec<_>>(), expected);
        assert!(!t.has_trashed_flag);
    }

    #[test]
    fn trashed_dropped_by_default_marks_flag() {
        let t = Translation::from_flags(&[Flag::Trashed, Flag::Seen], DeletedMessages::Skip);
        assert!(t.has_trashed_flag);
        assert_eq!(t.keywords.iter().collect::<Vec<_>>(), ["$seen"]);
    }

    #[test]
    fn trashed_with_include_deleted_adds_dollar_deleted() {
        let t = Translation::from_flags(&[Flag::Trashed, Flag::Seen], DeletedMessages::Keep);
        assert!(t.has_trashed_flag);
        let mut expected = vec!["$seen", "$deleted"];
        expected.sort();
        assert_eq!(t.keywords.iter().collect::<Vec<_>>(), expected);
    }

    #[test]
    fn order_independent_keywords_are_stable() {
        let a = Translation::from_flags(&[Flag::Seen, Flag::Flagged], DeletedMessages::Skip);
        let b = Translation::from_flags(&[Flag::Flagged, Flag::Seen], DeletedMessages::Skip);
        assert_eq!(a.keywords.to_json(), b.keywords.to_json());
    }

    #[test]
    fn duplicate_flags_dedup() {
        let t =
            Translation::from_flags(&[Flag::Seen, Flag::Seen, Flag::Seen], DeletedMessages::Skip);
        assert_eq!(t.keywords.iter().collect::<Vec<_>>(), ["$seen"]);
    }

    #[test]
    fn unique_id_strips_at_first_colon() {
        assert_eq!(
            MaildirFilename("1739471123.M001P01234V0.host:2,RS").unique_id(),
            "1739471123.M001P01234V0.host"
        );
    }

    #[test]
    fn unique_id_no_colon_is_whole_basename() {
        assert_eq!(
            MaildirFilename("1739471123.M001P0.host").unique_id(),
            "1739471123.M001P0.host"
        );
    }

    #[test]
    fn unique_id_strips_legacy_one_form() {
        assert_eq!(
            MaildirFilename("1234.M5.host:1,experimental").unique_id(),
            "1234.M5.host"
        );
    }

    #[test]
    fn flags_from_filename_recognises_full_set() {
        let mut flags = MaildirFilename("name:2,DFPRST").flags();
        flags.sort();
        let mut want = vec![
            Flag::Draft,
            Flag::Flagged,
            Flag::Passed,
            Flag::Replied,
            Flag::Seen,
            Flag::Trashed,
        ];
        want.sort();
        assert_eq!(flags, want);
    }

    #[test]
    fn flags_from_filename_no_info_section_yields_empty() {
        assert!(MaildirFilename("plain").flags().is_empty());
        assert!(MaildirFilename("name:").flags().is_empty());
        assert!(MaildirFilename("name:1,X").flags().is_empty());
    }

    #[test]
    fn flags_from_filename_dovecot_extension_metadata_passes_through() {
        let mut flags = MaildirFilename("uid,S=1234,W=1300:2,RS").flags();
        flags.sort();
        let mut want = vec![Flag::Replied, Flag::Seen];
        want.sort();
        assert_eq!(flags, want);
    }

    #[test]
    fn flags_from_filename_unknown_alpha_chars_skipped() {
        let mut flags = MaildirFilename("uid:2,SaT").flags();
        flags.sort();
        let mut want = vec![Flag::Seen, Flag::Trashed];
        want.sort();
        assert_eq!(flags, want);
    }
}
