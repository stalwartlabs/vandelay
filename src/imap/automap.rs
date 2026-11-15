/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

type Role = &'static str;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Automap {
    Enabled,
    Disabled,
}

pub struct SpecialUse;

impl SpecialUse {
    pub fn role(attributes: &[String]) -> Option<&'static str> {
        attributes.iter().find_map(|attr| {
            hashify::map_ignore_case!(attr.as_bytes(), Role,
                "\\all" => "all",
                "\\archive" => "archive",
                "\\drafts" => "drafts",
                "\\flagged" => "flagged",
                "\\important" => "important",
                "\\junk" => "junk",
                "\\sent" => "sent",
                "\\trash" => "trash",
            )
            .copied()
        })
    }
}

impl Automap {
    pub fn role(
        self,
        folder_name: &str,
        attributes: &[String],
        namespace_prefix: &str,
    ) -> Option<&'static str> {
        if folder_name.eq_ignore_ascii_case("INBOX") {
            return Some("inbox");
        }
        if let Some(role) = SpecialUse::role(attributes) {
            return Some(role);
        }
        if self == Automap::Disabled {
            return None;
        }
        let stripped = folder_name
            .get(..namespace_prefix.len())
            .filter(|head| head.eq_ignore_ascii_case(namespace_prefix))
            .and_then(|_| folder_name.get(namespace_prefix.len()..))
            .unwrap_or(folder_name);
        let leaf = stripped
            .rsplit_once(['/', '.'])
            .map_or(stripped, |(_, leaf)| leaf);
        Automap::role_from_name(leaf).or_else(|| Automap::role_from_name(stripped))
    }

    fn role_from_name(name: &str) -> Option<&'static str> {
        hashify::map_ignore_case!(name.as_bytes(), Role,
            "Sent" => "sent",
            "Sent Mail" => "sent",
            "Sent Items" => "sent",
            "Sent Messages" => "sent",
            "Sent-Mail" => "sent",
            "[Gmail]/Sent Mail" => "sent",
            "[Gmail]/Sent" => "sent",
            "Drafts" => "drafts",
            "Draft" => "drafts",
            "Brouillons" => "drafts",
            "[Gmail]/Drafts" => "drafts",
            "Trash" => "trash",
            "Deleted" => "trash",
            "Deleted Items" => "trash",
            "Deleted Messages" => "trash",
            "[Gmail]/Trash" => "trash",
            "[Gmail]/Bin" => "trash",
            "Junk" => "junk",
            "Spam" => "junk",
            "Junk E-mail" => "junk",
            "Junk Email" => "junk",
            "[Gmail]/Spam" => "junk",
            "[Gmail]/All Mail" => "all",
            "Archive" => "archive",
            "Archives" => "archive",
        )
        .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role_for_folder(
        name: &str,
        attributes: &[String],
        prefix: &str,
        enabled: bool,
    ) -> Option<&'static str> {
        let automap = if enabled {
            Automap::Enabled
        } else {
            Automap::Disabled
        };
        automap.role(name, attributes, prefix)
    }

    fn attrs(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| (*x).to_owned()).collect()
    }

    #[test]
    fn inbox_always_maps_to_inbox() {
        assert_eq!(role_for_folder("INBOX", &[], "", true), Some("inbox"));
        assert_eq!(role_for_folder("inbox", &[], "", true), Some("inbox"));
        assert_eq!(role_for_folder("Inbox", &[], "", false), Some("inbox"));
    }

    #[test]
    fn special_use_drives_role() {
        let r = role_for_folder("Whatever", &attrs(&["\\Sent"]), "", false);
        assert_eq!(r, Some("sent"));
        let r = role_for_folder("Anywhere", &attrs(&["\\Drafts"]), "", true);
        assert_eq!(r, Some("drafts"));
    }

    #[test]
    fn special_use_table_covers_all_iana_roles() {
        for (attr, expected) in [
            ("\\All", "all"),
            ("\\Archive", "archive"),
            ("\\Drafts", "drafts"),
            ("\\Flagged", "flagged"),
            ("\\Important", "important"),
            ("\\Junk", "junk"),
            ("\\Sent", "sent"),
            ("\\Trash", "trash"),
        ] {
            let r = SpecialUse::role(&attrs(&[attr]));
            assert_eq!(r, Some(expected), "for {attr}");
        }
    }

    #[test]
    fn special_use_attrs_unrelated_to_roles_yield_none() {
        for s in [
            "\\Subscribed",
            "\\HasChildren",
            "\\HasNoChildren",
            "\\Marked",
            "\\Unmarked",
            "\\Noselect",
            "\\Noinferiors",
            "\\Remote",
        ] {
            assert_eq!(SpecialUse::role(&attrs(&[s])), None);
        }
    }

    #[test]
    fn name_heuristic_when_no_special_use() {
        assert_eq!(role_for_folder("Sent", &[], "", true), Some("sent"));
        assert_eq!(role_for_folder("Sent Items", &[], "", true), Some("sent"));
        assert_eq!(role_for_folder("Drafts", &[], "", true), Some("drafts"));
        assert_eq!(role_for_folder("Trash", &[], "", true), Some("trash"));
        assert_eq!(
            role_for_folder("Deleted Items", &[], "", true),
            Some("trash")
        );
        assert_eq!(role_for_folder("Junk", &[], "", true), Some("junk"));
        assert_eq!(role_for_folder("Spam", &[], "", true), Some("junk"));
        assert_eq!(role_for_folder("Archive", &[], "", true), Some("archive"));
        assert_eq!(
            role_for_folder("[Gmail]/All Mail", &[], "", true),
            Some("all")
        );
        assert_eq!(role_for_folder("Archives", &[], "", true), Some("archive"));
    }

    #[test]
    fn name_heuristic_disabled_when_automap_off() {
        assert_eq!(role_for_folder("Sent", &[], "", false), None);
        assert_eq!(role_for_folder("Trash", &[], "", false), None);
    }

    #[test]
    fn namespace_prefix_stripped_before_name_match() {
        assert_eq!(
            role_for_folder("INBOX.Sent", &[], "INBOX.", true),
            Some("sent")
        );
        assert_eq!(
            role_for_folder("INBOX.Drafts", &[], "INBOX.", true),
            Some("drafts")
        );
    }

    #[test]
    fn unknown_folder_yields_none() {
        assert_eq!(role_for_folder("My Custom Folder", &[], "", true), None);
        assert_eq!(role_for_folder("Projects/Alpha", &[], "", true), None);
    }

    #[test]
    fn case_insensitive_name_match() {
        assert_eq!(role_for_folder("sent", &[], "", true), Some("sent"));
        assert_eq!(role_for_folder("DRAFTS", &[], "", true), Some("drafts"));
    }

    #[test]
    fn all_special_use_wins_over_heuristic_archive() {
        let r = role_for_folder("[Gmail]/All Mail", &attrs(&["\\All"]), "", true);
        assert_eq!(r, Some("all"));
    }

    #[test]
    fn gmail_all_mail_no_attribute_is_all_not_archive() {
        let r = role_for_folder("[Gmail]/All Mail", &[], "", true);
        assert_eq!(r, Some("all"));
    }

    #[test]
    fn plain_archive_name_resolves_to_archive() {
        assert_eq!(role_for_folder("Archive", &[], "", true), Some("archive"));
        assert_eq!(role_for_folder("Archives", &[], "", true), Some("archive"));
    }

    #[test]
    fn namespace_prefix_inside_a_multibyte_char_does_not_panic() {
        assert_eq!(role_for_folder("a日本", &[], "INBOX.", true), None);
        assert_eq!(role_for_folder("日本/Sent", &[], "日", true), Some("sent"));
    }
}
