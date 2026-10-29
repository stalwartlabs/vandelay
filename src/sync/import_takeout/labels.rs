/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::borrow::Cow;
use std::collections::BTreeSet;

use mail_parser::HeaderValue;
use mail_parser::parsers::MessageStream;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelClassification {
    pub mailboxes: Vec<String>,
    pub keywords: BTreeSet<String>,
    pub had_explicit_seen_signal: bool,
    pub had_unread_signal: bool,
    pub opened_won_over_unread: bool,
}

impl LabelClassification {
    pub fn keywords_sorted(&self) -> Vec<String> {
        self.keywords.iter().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.mailboxes.is_empty() && self.keywords.is_empty()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MappingOptions {
    pub automap: bool,
}

impl Default for MappingOptions {
    fn default() -> Self {
        Self { automap: true }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxAssignment {
    pub canonical_path: String,
    pub role: Option<&'static str>,
}

pub fn parse_header(value: &str) -> Vec<String> {
    let decoded = decode_encoded_words(value);
    decoded
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

fn decode_encoded_words(value: &str) -> Cow<'_, str> {
    if !value.contains("=?") {
        return Cow::Borrowed(value);
    }
    let mut buf = Vec::with_capacity(value.len() + 1);
    buf.extend_from_slice(value.as_bytes());
    buf.push(b'\n');
    match MessageStream::new(&buf).parse_unstructured() {
        HeaderValue::Text(text) => Cow::Owned(text.into_owned()),
        _ => Cow::Borrowed(value),
    }
}

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

pub fn classify(tokens: &[String]) -> LabelClassification {
    let mut mailboxes: Vec<String> = Vec::new();
    let mut keywords: BTreeSet<String> = BTreeSet::new();
    let mut had_opened = false;
    let mut had_unread = false;

    for tok in tokens {
        let mailbox = match SystemLabel::parse(tok) {
            Some(SystemLabel::Category) => continue,
            Some(SystemLabel::Starred) => {
                keywords.insert("$flagged".to_owned());
                continue;
            }
            Some(SystemLabel::Important) => {
                keywords.insert("$important".to_owned());
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
        keywords.insert("$seen".to_owned());
    }

    let opened_won_over_unread = had_opened && had_unread;
    LabelClassification {
        mailboxes,
        keywords,
        had_explicit_seen_signal: had_opened,
        had_unread_signal: had_unread,
        opened_won_over_unread,
    }
}

pub fn role_for_mailbox(canonical_path: &str, opts: MappingOptions) -> Option<&'static str> {
    if !opts.automap {
        return None;
    }
    if canonical_path.contains('/') {
        return None;
    }
    match canonical_path {
        "Inbox" => Some("inbox"),
        "Sent" => Some("sent"),
        "Drafts" => Some("drafts"),
        "Trash" => Some("trash"),
        "Spam" => Some("junk"),
        "Archive" => Some("archive"),
        _ => None,
    }
}

pub fn assignments_for(
    classification: &LabelClassification,
    opts: MappingOptions,
) -> Vec<MailboxAssignment> {
    classification
        .mailboxes
        .iter()
        .map(|p| MailboxAssignment {
            canonical_path: p.clone(),
            role: role_for_mailbox(p, opts),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify_str(s: &str) -> LabelClassification {
        classify(&parse_header(s))
    }

    #[test]
    fn parse_header_splits_on_comma_and_trims() {
        let v = parse_header("Inbox, Opened ,Starred,,Important");
        assert_eq!(v, vec!["Inbox", "Opened", "Starred", "Important"]);
    }

    #[test]
    fn parse_header_decodes_rfc2047_encoded_words() {
        let v = parse_header("=?UTF-8?Q?=C3=85pnet,Kategori:_Reklame?=");
        assert_eq!(v, vec!["Åpnet", "Kategori: Reklame"]);
    }

    #[test]
    fn parse_header_decodes_mixed_plain_and_encoded_tokens() {
        let v = parse_header("Inbox,=?UTF-8?Q?=C3=85pnet?=,Github");
        assert_eq!(v, vec!["Inbox", "Åpnet", "Github"]);
    }

    #[test]
    fn parse_header_leaves_plain_ascii_untouched() {
        let v = parse_header("Inbox,Opened,Label_001/Label_002");
        assert_eq!(v, vec!["Inbox", "Opened", "Label_001/Label_002"]);
    }

    #[test]
    fn parse_header_empty_yields_empty() {
        assert!(parse_header("").is_empty());
        assert!(parse_header(",, ,, ").is_empty());
    }

    #[test]
    fn inbox_token_becomes_inbox_mailbox_with_role() {
        let c = classify_str("Inbox");
        assert_eq!(c.mailboxes, vec!["Inbox".to_owned()]);
        let a = assignments_for(&c, MappingOptions::default());
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
            let a = assignments_for(&c, MappingOptions::default());
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
        assert!(c.had_unread_signal);
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
        assert!(classify(&[]).keywords.is_empty());
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
        assert_eq!(c.keywords_sorted(), ["$flagged", "$important", "$seen"]);
        let roles: Vec<_> = assignments_for(&c, MappingOptions::default())
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
        assert!(c.had_unread_signal);
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
        let a = assignments_for(&c, MappingOptions::default());
        let nested = a
            .iter()
            .find(|m| m.canonical_path == "Label_001/Label_002_under_Label_001")
            .unwrap();
        assert_eq!(nested.role, None);
    }

    #[test]
    fn custom_labels_get_no_role() {
        let c = classify_str("Inbox,Github,Newsletter");
        let a = assignments_for(&c, MappingOptions::default());
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
        assert!(c.had_unread_signal);
    }

    #[test]
    fn noautomap_suppresses_roles_but_keeps_names() {
        let c = classify_str("Inbox,Sent,Github");
        let a = assignments_for(&c, MappingOptions { automap: false });
        assert!(a.iter().all(|m| m.role.is_none()));
        let names: Vec<&str> = a.iter().map(|m| m.canonical_path.as_str()).collect();
        assert!(names.contains(&"Inbox"));
        assert!(names.contains(&"Sent"));
        assert!(names.contains(&"Github"));
    }

    #[test]
    fn keywords_sorted_returns_deterministic_order() {
        let c = classify_str("Starred,Important,Opened");
        let k = c.keywords_sorted();
        assert_eq!(
            k,
            vec![
                "$flagged".to_owned(),
                "$important".to_owned(),
                "$seen".to_owned()
            ]
        );
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
}
