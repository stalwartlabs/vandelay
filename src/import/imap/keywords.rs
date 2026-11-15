/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::sync::keywords::{Keywords, SystemKeyword};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletedMessages {
    Skip,
    Keep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagAction {
    Keep(&'static str),
    Drop,
    Verbatim,
    Deleted,
}

impl FlagAction {
    pub fn of(flag: &str) -> FlagAction {
        let Some(system) = flag.strip_prefix('\\') else {
            return FlagAction::Verbatim;
        };
        hashify::map_ignore_case!(system.as_bytes(), FlagAction,
            "seen" => FlagAction::Keep("$seen"),
            "flagged" => FlagAction::Keep("$flagged"),
            "answered" => FlagAction::Keep("$answered"),
            "draft" => FlagAction::Keep("$draft"),
            "recent" => FlagAction::Drop,
            "deleted" => FlagAction::Deleted,
        )
        .copied()
        .unwrap_or(FlagAction::Verbatim)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Translation {
    pub keywords: Keywords,
    pub has_deleted_flag: bool,
}

impl Translation {
    pub fn from_flags(imap_flags: &[String], deleted: DeletedMessages) -> Translation {
        let mut out = Translation {
            keywords: Keywords::with_capacity(imap_flags.len()),
            has_deleted_flag: false,
        };
        for f in imap_flags {
            if f.is_empty() {
                continue;
            }
            match FlagAction::of(f) {
                FlagAction::Keep(k) => out.keywords.push(k),
                FlagAction::Drop => {}
                FlagAction::Verbatim => out.keywords.push(f),
                FlagAction::Deleted => {
                    out.has_deleted_flag = true;
                    if deleted == DeletedMessages::Keep {
                        out.keywords.push_system(SystemKeyword::Deleted);
                    }
                }
            }
        }
        out
    }

    pub fn skips(&self, deleted: DeletedMessages) -> bool {
        self.has_deleted_flag && deleted == DeletedMessages::Skip
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| (*x).to_owned()).collect()
    }

    #[test]
    fn system_flag_mapping() {
        assert!(matches!(
            FlagAction::of("\\Seen"),
            FlagAction::Keep("$seen")
        ));
        assert!(matches!(
            FlagAction::of("\\Flagged"),
            FlagAction::Keep("$flagged")
        ));
        assert!(matches!(
            FlagAction::of("\\Answered"),
            FlagAction::Keep("$answered")
        ));
        assert!(matches!(
            FlagAction::of("\\Draft"),
            FlagAction::Keep("$draft")
        ));
        assert!(matches!(FlagAction::of("\\Recent"), FlagAction::Drop));
        assert!(matches!(FlagAction::of("\\Deleted"), FlagAction::Deleted));
    }

    #[test]
    fn unknown_system_flag_passes_verbatim() {
        assert!(matches!(
            FlagAction::of("\\Forwarded"),
            FlagAction::Verbatim
        ));
        assert!(matches!(FlagAction::of("\\MDNSent"), FlagAction::Verbatim));
    }

    #[test]
    fn custom_keywords_pass_verbatim_case() {
        let t = Translation::from_flags(
            &flags(&["$Junk", "NotJunk", "MyLabel"]),
            DeletedMessages::Skip,
        );
        assert_eq!(
            t.keywords.iter().collect::<Vec<_>>(),
            vec!["$Junk", "NotJunk", "MyLabel"]
        );
        assert!(!t.has_deleted_flag);
    }

    #[test]
    fn translates_seen_and_flagged() {
        let t = Translation::from_flags(&flags(&["\\Seen", "\\Flagged"]), DeletedMessages::Skip);
        assert_eq!(
            t.keywords.iter().collect::<Vec<_>>(),
            vec!["$seen", "$flagged"]
        );
    }

    #[test]
    fn drops_recent() {
        let t = Translation::from_flags(&flags(&["\\Seen", "\\Recent"]), DeletedMessages::Skip);
        assert_eq!(t.keywords.iter().collect::<Vec<_>>(), vec!["$seen"]);
    }

    #[test]
    fn deleted_flag_without_include_marks_only() {
        let t = Translation::from_flags(&flags(&["\\Deleted", "\\Seen"]), DeletedMessages::Skip);
        assert!(t.has_deleted_flag);
        assert_eq!(t.keywords.iter().collect::<Vec<_>>(), vec!["$seen"]);
    }

    #[test]
    fn deleted_flag_with_include_adds_dollar_deleted() {
        let t = Translation::from_flags(&flags(&["\\Deleted", "\\Seen"]), DeletedMessages::Keep);
        assert!(t.has_deleted_flag);
        assert_eq!(
            t.keywords.iter().collect::<Vec<_>>(),
            vec!["$deleted", "$seen"]
        );
    }

    #[test]
    fn duplicate_keywords_deduped_case_insensitively() {
        let t =
            Translation::from_flags(&flags(&["mykey", "MYKEY", "MyKey"]), DeletedMessages::Skip);
        assert_eq!(t.keywords.iter().collect::<Vec<_>>(), vec!["mykey"]);
    }

    #[test]
    fn empty_flag_strings_ignored() {
        let t = Translation::from_flags(&flags(&["", "\\Seen", ""]), DeletedMessages::Skip);
        assert_eq!(t.keywords.iter().collect::<Vec<_>>(), vec!["$seen"]);
    }

    #[test]
    fn order_preserved() {
        let t = Translation::from_flags(
            &flags(&["MyLabel", "\\Seen", "$Important", "\\Flagged"]),
            DeletedMessages::Skip,
        );
        assert_eq!(
            t.keywords.iter().collect::<Vec<_>>(),
            vec!["MyLabel", "$seen", "$Important", "$flagged"]
        );
    }
}
