/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::borrow::Cow;
use std::str;

use mail_parser::{HeaderForm, HeaderValue};

use crate::imap::automap::Automap;
use crate::sync::keywords::Keywords;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelClassification {
    pub mailboxes: Vec<String>,
    pub keywords: Keywords,
    pub opened_won_over_unread: bool,
}

pub struct GmailLabels;

impl GmailLabels {
    pub fn parse(value: &str) -> Vec<String> {
        GmailLabels::decode_encoded_words(value)
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
            .collect()
    }

    pub fn header(bytes: &[u8]) -> Option<String> {
        let mut joined: Option<String> = None;
        let mut current: Option<String> = None;
        for line in bytes.split(|&b| b == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() {
                break;
            }
            if let [b' ' | b'\t', ..] = line {
                if let Some(buf) = current.as_mut() {
                    let extra = str::from_utf8(line).unwrap_or("").trim_start();
                    if !extra.is_empty() {
                        buf.push(' ');
                        buf.push_str(extra);
                    }
                }
                continue;
            }
            GmailLabels::append(&mut joined, current.take());
            let mut parts = line.splitn(2, |&b| b == b':');
            if let (Some(name), Some(value)) = (parts.next(), parts.next())
                && name.eq_ignore_ascii_case(b"X-Gmail-Labels")
            {
                current = Some(str::from_utf8(value).unwrap_or("").trim().to_owned());
            }
        }
        GmailLabels::append(&mut joined, current);
        joined
    }

    fn append(joined: &mut Option<String>, value: Option<String>) {
        let Some(value) = value else {
            return;
        };
        match joined {
            Some(out) => {
                out.push(',');
                out.push_str(&value);
            }
            None => *joined = Some(value),
        }
    }

    fn decode_encoded_words(value: &str) -> Cow<'_, str> {
        if !value.contains("=?") {
            return Cow::Borrowed(value);
        }
        match HeaderForm::Text.parse(value.as_bytes()).value() {
            HeaderValue::Text(text) => Cow::Owned(text.to_owned()),
            _ => Cow::Borrowed(value),
        }
    }

    pub fn role(canonical_path: &str, automap: Automap) -> Option<&'static str> {
        if automap == Automap::Disabled || canonical_path.contains('/') {
            return None;
        }
        hashify::map!(canonical_path.as_bytes(), Role,
            "Inbox" => "inbox",
            "Sent" => "sent",
            "Drafts" => "drafts",
            "Trash" => "trash",
            "Spam" => "junk",
            "Archive" => "archive",
        )
        .copied()
    }
}

type Role = &'static str;

#[derive(Debug, Clone, Copy)]
enum SystemLabel {
    Starred,
    Important,
    Opened,
    Unread,
    Category,
    Mailbox(&'static str),
}

impl SystemLabel {
    fn parse(token: &str) -> Option<SystemLabel> {
        hashify::map!(
            token.as_bytes(),
            SystemLabel,
            "Starred" => SystemLabel::Starred,
            "Important" => SystemLabel::Important,
            "Opened" => SystemLabel::Opened,
            "Unread" => SystemLabel::Unread,
            "Archived" => SystemLabel::Mailbox("Archive"),
            "Category Personal" => SystemLabel::Category,
            "Category Promotions" => SystemLabel::Category,
            "Category Social" => SystemLabel::Category,
            "Category Updates" => SystemLabel::Category,
            "Category Forums" => SystemLabel::Category,
            "Com estrela" => SystemLabel::Starred,
            "Importante" => SystemLabel::Important,
            "Aberto" => SystemLabel::Opened,
            "Não lidas" => SystemLabel::Unread,
            "Caixa de entrada" => SystemLabel::Mailbox("Inbox"),
            "Enviado" => SystemLabel::Mailbox("Sent"),
            "Rascunhos" => SystemLabel::Mailbox("Drafts"),
            "Lixeira" => SystemLabel::Mailbox("Trash"),
            "Arquivada" => SystemLabel::Mailbox("Archive"),
            "Categoria: pessoal" => SystemLabel::Category,
            "Categoria: promoções" => SystemLabel::Category,
            "Categoria: social" => SystemLabel::Category,
            "Categoria: atualizações" => SystemLabel::Category,
            "Categoria: fóruns" => SystemLabel::Category,
        )
        .copied()
    }
}

impl LabelClassification {
    pub fn classify(tokens: &[String]) -> LabelClassification {
        let mut mailboxes: Vec<String> = Vec::new();
        let mut keywords = Keywords::new();
        let mut had_opened = false;
        let mut had_unread = false;

        for tok in tokens {
            let mailbox = match SystemLabel::parse(tok) {
                Some(SystemLabel::Category) => continue,
                Some(SystemLabel::Starred) => {
                    keywords.push("$flagged");
                    continue;
                }
                Some(SystemLabel::Important) => {
                    keywords.push("$important");
                    continue;
                }
                Some(SystemLabel::Opened) => {
                    had_opened = true;
                    continue;
                }
                Some(SystemLabel::Unread) => {
                    had_unread = true;
                    continue;
                }
                Some(SystemLabel::Mailbox(name)) => name,
                None => tok.as_str(),
            };
            if !mailboxes.iter().any(|m| m == mailbox) {
                mailboxes.push(mailbox.to_owned());
            }
        }

        if had_opened || (!had_unread && !tokens.is_empty()) {
            keywords.push("$seen");
        }
        keywords.sort();

        LabelClassification {
            mailboxes,
            keywords,
            opened_won_over_unread: had_opened && had_unread,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Assignment<'a> {
        canonical_path: &'a str,
        role: Option<&'static str>,
    }

    fn assignments_for(c: &LabelClassification, automap: Automap) -> Vec<Assignment<'_>> {
        c.mailboxes
            .iter()
            .map(|p| Assignment {
                canonical_path: p,
                role: GmailLabels::role(p, automap),
            })
            .collect()
    }

    fn classify_str(s: &str) -> LabelClassification {
        LabelClassification::classify(&GmailLabels::parse(s))
    }

    #[test]
    fn parse_header_splits_on_comma_and_trims() {
        let v = GmailLabels::parse("Inbox, Opened ,Starred,,Important");
        assert_eq!(v, vec!["Inbox", "Opened", "Starred", "Important"]);
    }

    #[test]
    fn parse_header_decodes_rfc2047_encoded_words() {
        let v = GmailLabels::parse("=?UTF-8?Q?=C3=85pnet,Kategori:_Reklame?=");
        assert_eq!(v, vec!["Åpnet", "Kategori: Reklame"]);
    }

    #[test]
    fn parse_header_decodes_mixed_plain_and_encoded_tokens() {
        let v = GmailLabels::parse("Inbox,=?UTF-8?Q?=C3=85pnet?=,Github");
        assert_eq!(v, vec!["Inbox", "Åpnet", "Github"]);
    }

    // A character split across two encoded words is joined before decoding.
    #[test]
    fn parse_header_joins_a_character_split_across_encoded_words() {
        let v = GmailLabels::parse("=?utf-8?q?caf=C3?= =?UTF-8?q?=A9?=,Inbox");
        assert_eq!(v, vec!["café", "Inbox"]);
    }

    #[test]
    fn parse_header_leaves_plain_ascii_untouched() {
        let v = GmailLabels::parse("Inbox,Opened,Label_001/Label_002");
        assert_eq!(v, vec!["Inbox", "Opened", "Label_001/Label_002"]);
    }

    #[test]
    fn parse_header_empty_yields_empty() {
        assert!(GmailLabels::parse("").is_empty());
        assert!(GmailLabels::parse(",, ,, ").is_empty());
    }

    #[test]
    fn inbox_token_becomes_inbox_mailbox_with_role() {
        let c = classify_str("Inbox");
        assert_eq!(c.mailboxes, vec!["Inbox".to_owned()]);
        let a = assignments_for(&c, Automap::Enabled);
        assert_eq!(a[0].role, Some("inbox"));
    }

    #[test]
    fn sent_drafts_trash_spam_archive_map_to_roles() {
        for (token, role) in [
            ("Sent", "sent"),
            ("Drafts", "drafts"),
            ("Trash", "trash"),
            ("Spam", "junk"),
            ("Archived", "archive"),
        ] {
            let c = classify_str(token);
            let a = assignments_for(&c, Automap::Enabled);
            assert_eq!(a[0].role, Some(role), "token {token}");
        }
        let c = classify_str("Archived");
        assert_eq!(c.mailboxes, vec!["Archive".to_owned()]);
    }

    #[test]
    fn starred_important_become_keywords_not_mailboxes() {
        let c = classify_str("Starred,Important");
        assert!(c.mailboxes.is_empty());
        assert!(c.keywords.contains("$flagged"));
        assert!(c.keywords.contains("$important"));
    }

    #[test]
    fn opened_yields_seen_keyword() {
        let c = classify_str("Inbox,Opened");
        assert!(c.keywords.contains("$seen"));
    }

    #[test]
    fn unread_alone_means_no_seen() {
        let c = classify_str("Inbox,Unread");
        assert!(!c.keywords.contains("$seen"));
    }

    #[test]
    fn neither_opened_nor_unread_means_seen() {
        let c = classify_str("Inbox,Important");
        assert!(c.keywords.contains("$seen"));
        let c = classify_str("Sent");
        assert!(c.keywords.contains("$seen"));
    }

    #[test]
    fn no_labels_sets_no_keywords() {
        assert!(LabelClassification::classify(&[]).keywords.is_empty());
    }

    #[test]
    fn pt_br_system_labels_map_like_english() {
        let c = classify_str(
            "Caixa de entrada,Enviado,Rascunhos,Lixeira,Arquivada,Importante,Com estrela,\
             Aberto,Categoria: pessoal,Categoria: atualizações,Categoria: promoções,Spam,Clientes",
        );
        assert_eq!(
            c.mailboxes,
            [
                "Inbox", "Sent", "Drafts", "Trash", "Archive", "Spam", "Clientes"
            ]
        );
        assert_eq!(
            c.keywords.iter().collect::<Vec<_>>(),
            ["$flagged", "$important", "$seen"]
        );
        let roles: Vec<_> = assignments_for(&c, Automap::Enabled)
            .into_iter()
            .map(|a| a.role)
            .collect();
        assert_eq!(
            roles,
            [
                Some("inbox"),
                Some("sent"),
                Some("drafts"),
                Some("trash"),
                Some("archive"),
                Some("junk"),
                None
            ]
        );
    }

    #[test]
    fn pt_br_unread_label_means_no_seen() {
        let c = classify_str("Caixa de entrada,Não lidas");
        assert!(!c.keywords.contains("$seen"));
    }

    #[test]
    fn opened_plus_unread_marks_collision_and_keeps_seen() {
        let c = classify_str("Inbox,Opened,Unread");
        assert!(c.keywords.contains("$seen"));
        assert!(c.opened_won_over_unread);
    }

    #[test]
    fn category_tokens_are_dropped_silently() {
        let c = classify_str("Inbox,Category Promotions,Category Updates,Opened");
        assert_eq!(c.mailboxes, vec!["Inbox".to_owned()]);
        assert!(c.keywords.contains("$seen"));
    }

    #[test]
    fn nested_labels_preserve_full_path() {
        let c = classify_str("Inbox,Label_001/Label_002_under_Label_001");
        assert!(
            c.mailboxes
                .contains(&"Label_001/Label_002_under_Label_001".to_owned())
        );
        let a = assignments_for(&c, Automap::Enabled);
        let nested = a
            .iter()
            .find(|m| m.canonical_path == "Label_001/Label_002_under_Label_001")
            .unwrap();
        assert_eq!(nested.role, None);
    }

    #[test]
    fn custom_labels_get_no_role() {
        let c = classify_str("Inbox,Github,Newsletter");
        let a = assignments_for(&c, Automap::Enabled);
        let github = a.iter().find(|m| m.canonical_path == "Github").unwrap();
        assert_eq!(github.role, None);
    }

    #[test]
    fn realistic_fixture_label_sets_classify_as_expected() {
        let c = classify_str("Archived,Important,Opened,Category Social,Github");
        assert!(c.mailboxes.contains(&"Archive".to_owned()));
        assert!(c.mailboxes.contains(&"Github".to_owned()));
        assert!(c.keywords.contains("$important"));
        assert!(c.keywords.contains("$seen"));
        assert!(!c.mailboxes.iter().any(|m| m.contains("Category")));

        let c = classify_str("Important,Trash,Category Social,Unread,Github");
        assert!(c.mailboxes.contains(&"Trash".to_owned()));
        assert!(c.mailboxes.contains(&"Github".to_owned()));
        assert!(c.keywords.contains("$important"));
        assert!(!c.keywords.contains("$seen"));
    }

    #[test]
    fn noautomap_suppresses_roles_but_keeps_names() {
        let c = classify_str("Inbox,Sent,Github");
        let a = assignments_for(&c, Automap::Disabled);
        assert!(a.iter().all(|m| m.role.is_none()));
        let names: Vec<&str> = a.iter().map(|m| m.canonical_path).collect();
        assert!(names.contains(&"Inbox"));
        assert!(names.contains(&"Sent"));
        assert!(names.contains(&"Github"));
    }

    #[test]
    fn keywords_sorted_returns_deterministic_order() {
        let c = classify_str("Starred,Important,Opened");
        let k = c.keywords.iter().collect::<Vec<_>>();
        assert_eq!(k, ["$flagged", "$important", "$seen"]);
    }

    #[test]
    fn duplicate_mailbox_token_dedups() {
        let c = classify_str("Github,Github,Inbox");
        let count = c
            .mailboxes
            .iter()
            .filter(|m| m.as_str() == "Github")
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn extract_labels_from_simple_message() {
        let bytes = b"From: a@b\r\n\
            X-Gmail-Labels: Inbox,Opened\r\n\
            Subject: hi\r\n\
            \r\n\
            body\r\n";
        assert_eq!(GmailLabels::header(bytes), Some("Inbox,Opened".to_owned()));
    }

    #[test]
    fn extract_labels_handles_lf_only_line_endings() {
        let bytes = b"From: a@b\nX-Gmail-Labels: Inbox\n\nbody\n";
        assert_eq!(GmailLabels::header(bytes), Some("Inbox".to_owned()));
    }

    #[test]
    fn extract_labels_handles_folded_continuation() {
        let bytes = b"From: a@b\r\n\
            X-Gmail-Labels: Inbox,\r\n\
            \tImportant,Github\r\n\
            Subject: hi\r\n\
            \r\n\
            body\r\n";
        assert_eq!(
            GmailLabels::header(bytes),
            Some("Inbox, Important,Github".to_owned())
        );
    }

    #[test]
    fn extract_labels_returns_none_when_header_absent() {
        let bytes = b"From: a@b\r\nSubject: hi\r\n\r\nbody\r\n";
        assert_eq!(GmailLabels::header(bytes), None);
    }

    #[test]
    fn extract_labels_case_insensitive_name_match() {
        let bytes = b"x-gmail-labels: Sent\r\n\r\n";
        assert_eq!(GmailLabels::header(bytes), Some("Sent".to_owned()));
    }

    #[test]
    fn extract_labels_concatenates_multiple_header_occurrences() {
        let bytes = b"X-Gmail-Labels: Inbox\r\nX-Gmail-Labels: Github\r\n\r\nbody";
        assert_eq!(GmailLabels::header(bytes), Some("Inbox,Github".to_owned()));
    }

    #[test]
    fn extract_labels_stops_at_blank_line() {
        let bytes = b"X-Gmail-Labels: Inbox\r\n\r\nX-Gmail-Labels: NOT-A-HEADER\r\n";
        assert_eq!(GmailLabels::header(bytes), Some("Inbox".to_owned()));
    }
}
