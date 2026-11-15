/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{BTreeMap, HashMap, HashSet};

use regex::Regex;

use crate::imap::automap::Automap;
use crate::imap::name::NameEncoding;
use crate::imap::response::Untagged;
use crate::sync::tree::FolderPath;

#[derive(Debug, Clone, Default)]
pub struct FolderStatus {
    pub uidvalidity: Option<u64>,
    pub uidnext: Option<u64>,
    pub messages: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct DiscoveredFolder {
    pub name: String,
    pub wire_name: String,
    pub delimiter: Option<char>,
    pub attributes: Vec<String>,
    pub subscribed: bool,
    pub selectable: bool,
    pub status: Option<FolderStatus>,
}

#[derive(Debug, Clone)]
pub struct ResolvedFolder {
    pub name: String,
    pub wire_name: String,
    pub leaf: String,
    pub parent_path: Option<String>,
    pub delimiter: Option<char>,
    pub role: Option<&'static str>,
    pub subscribed: bool,
    pub selectable: bool,
    pub status: Option<FolderStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderScope {
    All,
    Subscribed,
}

pub struct FolderFilters {
    pub include: Vec<Regex>,
    pub exclude: Vec<Regex>,
    pub exclude_special: Vec<String>,
    pub explicit: Vec<String>,
    pub scope: FolderScope,
    pub automap: Automap,
    pub namespace_prefix: String,
}

impl DiscoveredFolder {
    pub fn collect(
        untagged: &[Untagged],
        encoding: NameEncoding,
    ) -> Result<Vec<DiscoveredFolder>, String> {
        let mut subscribed = HashSet::new();
        let mut statuses: HashMap<String, FolderStatus> = HashMap::new();
        let mut found = BTreeMap::new();
        for u in untagged {
            match u {
                Untagged::Lsub { name, .. } => {
                    subscribed.insert(encoding.mailbox(name).map_err(|e| e.to_string())?);
                }
                Untagged::List {
                    attributes,
                    delimiter,
                    name,
                } => {
                    let canonical = encoding.mailbox(name).map_err(|e| e.to_string())?;
                    let selectable = !attributes.iter().any(|a| {
                        hashify::set_ignore_case!(a.as_bytes(), "\\noselect", "\\nonexistent")
                    });
                    if attributes
                        .iter()
                        .any(|a| a.eq_ignore_ascii_case("\\subscribed"))
                    {
                        subscribed.insert(canonical.clone());
                    }
                    found.insert(
                        canonical.clone(),
                        DiscoveredFolder {
                            name: canonical,
                            wire_name: name.clone(),
                            delimiter: *delimiter,
                            attributes: attributes.clone(),
                            subscribed: false,
                            selectable,
                            status: None,
                        },
                    );
                }
                Untagged::Status { mailbox, items } => {
                    let canonical = encoding.mailbox(mailbox).map_err(|e| e.to_string())?;
                    statuses.insert(
                        canonical,
                        FolderStatus {
                            uidvalidity: items.get("UIDVALIDITY").copied(),
                            uidnext: items.get("UIDNEXT").copied(),
                            messages: items.get("MESSAGES").copied(),
                        },
                    );
                }
                _ => {}
            }
        }
        Ok(found
            .into_values()
            .map(|mut f| {
                f.subscribed = subscribed.contains(&f.name);
                f.status = statuses.remove(&f.name);
                f
            })
            .collect())
    }
}

impl ResolvedFolder {
    fn path_parts(name: &str, delimiter: Option<char>) -> (String, Option<String>) {
        match delimiter {
            Some(d) => {
                let path = FolderPath::new(name, d);
                (path.leaf().to_owned(), path.parent().map(str::to_owned))
            }
            None => (name.to_owned(), None),
        }
    }

    fn depth(&self) -> usize {
        match (self.parent_path.as_deref(), self.delimiter) {
            (Some(parent), Some(d)) => FolderPath::new(parent, d).depth() + 1,
            _ => 0,
        }
    }

    pub fn sort_parents_first(resolved: &mut [ResolvedFolder]) {
        resolved.sort_by(|a, b| a.depth().cmp(&b.depth()).then_with(|| a.name.cmp(&b.name)));
    }
}

impl FolderFilters {
    pub fn apply(&self, folders: Vec<DiscoveredFolder>) -> Vec<ResolvedFolder> {
        let mut resolved: Vec<ResolvedFolder> = Vec::with_capacity(folders.len());
        let mut others: HashMap<String, DiscoveredFolder> = HashMap::new();
        for f in folders {
            if !f.selectable
                || !self.matches(&f.name)
                || (self.scope == FolderScope::Subscribed && !f.subscribed)
            {
                others.insert(f.name.clone(), f);
                continue;
            }
            let role = self
                .automap
                .role(&f.name, &f.attributes, &self.namespace_prefix);
            if let Some(r) = role
                && self
                    .exclude_special
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(r))
            {
                others.insert(f.name.clone(), f);
                continue;
            }
            let (leaf, parent_path) = ResolvedFolder::path_parts(&f.name, f.delimiter);
            resolved.push(ResolvedFolder {
                name: f.name,
                wire_name: f.wire_name,
                leaf,
                parent_path,
                delimiter: f.delimiter,
                role,
                subscribed: f.subscribed,
                selectable: true,
                status: f.status,
            });
        }
        let mut present: HashSet<String> = resolved.iter().map(|f| f.name.clone()).collect();
        let mut structural: Vec<ResolvedFolder> = Vec::new();
        for child in &resolved {
            let mut parent = child.parent_path.clone();
            let mut child_wire = child.wire_name.clone();
            let mut delim = child.delimiter;
            while let Some(name) = parent {
                if present.contains(&name) {
                    break;
                }
                let ancestor = match others.remove(&name) {
                    Some(d) => {
                        let delimiter = d.delimiter.or(delim);
                        let (leaf, parent_path) = ResolvedFolder::path_parts(&d.name, delimiter);
                        ResolvedFolder {
                            name: d.name,
                            wire_name: d.wire_name,
                            leaf,
                            parent_path,
                            delimiter,
                            role: None,
                            subscribed: d.subscribed,
                            selectable: false,
                            status: None,
                        }
                    }
                    None => {
                        let wire_name = match delim.and_then(|d| child_wire.rsplit_once(d)) {
                            Some((p, _)) => p.to_owned(),
                            None => name.clone(),
                        };
                        let (leaf, parent_path) = ResolvedFolder::path_parts(&name, delim);
                        ResolvedFolder {
                            name,
                            wire_name,
                            leaf,
                            parent_path,
                            delimiter: delim,
                            role: None,
                            subscribed: false,
                            selectable: false,
                            status: None,
                        }
                    }
                };
                present.insert(ancestor.name.clone());
                parent = ancestor.parent_path.clone();
                child_wire.clone_from(&ancestor.wire_name);
                delim = ancestor.delimiter;
                structural.push(ancestor);
            }
        }
        resolved.extend(structural);
        resolved
    }

    fn matches(&self, name: &str) -> bool {
        if !self.explicit.is_empty() {
            return self.explicit.iter().any(|e| e == name);
        }
        if !self.include.is_empty() && !self.include.iter().any(|r| r.is_match(name)) {
            return false;
        }
        !self.exclude.iter().any(|r| r.is_match(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lst(name: &str, delim: &str, attrs: &[&str]) -> Untagged {
        Untagged::List {
            attributes: attrs.iter().map(|s| (*s).to_owned()).collect(),
            delimiter: delim.chars().next(),
            name: name.to_owned(),
        }
    }

    #[test]
    fn collect_filters_noselect_and_canonicalises_inbox() {
        let resps = vec![
            lst("Inbox", "/", &[]),
            lst("Sent", "/", &["\\Sent"]),
            lst("Hidden", "/", &["\\Noselect"]),
        ];
        let folders = DiscoveredFolder::collect(&resps, NameEncoding::ModifiedUtf7).unwrap();
        assert_eq!(folders.len(), 3);
        let inbox = folders.iter().find(|f| f.name == "INBOX").unwrap();
        assert!(inbox.selectable);
        let hidden = folders.iter().find(|f| f.name == "Hidden").unwrap();
        assert!(!hidden.selectable);
    }

    #[test]
    fn collect_marks_subscribed_via_lsub_or_extended() {
        let resps = vec![
            lst("Inbox", "/", &[]),
            lst("Sent", "/", &["\\Subscribed"]),
            Untagged::Lsub {
                attributes: vec![],
                delimiter: Some('/'),
                name: "Inbox".to_owned(),
            },
        ];
        let folders = DiscoveredFolder::collect(&resps, NameEncoding::ModifiedUtf7).unwrap();
        let inbox = folders.iter().find(|f| f.name == "INBOX").unwrap();
        assert!(inbox.subscribed);
        let sent = folders.iter().find(|f| f.name == "Sent").unwrap();
        assert!(sent.subscribed);
    }

    #[test]
    fn collect_attaches_list_status_data_per_mailbox() {
        let mut inbox_status: std::collections::BTreeMap<String, u64> =
            std::collections::BTreeMap::new();
        inbox_status.insert("UIDVALIDITY".into(), 12345);
        inbox_status.insert("UIDNEXT".into(), 42);
        inbox_status.insert("MESSAGES".into(), 7);
        let resps = vec![
            lst("INBOX", "/", &[]),
            lst("Sent", "/", &["\\Sent"]),
            Untagged::Status {
                mailbox: "INBOX".to_owned(),
                items: inbox_status,
            },
        ];
        let folders = DiscoveredFolder::collect(&resps, NameEncoding::ModifiedUtf7).unwrap();
        let inbox = folders.iter().find(|f| f.name == "INBOX").unwrap();
        let st = inbox.status.as_ref().expect("INBOX has status");
        assert_eq!(st.uidvalidity, Some(12345));
        assert_eq!(st.uidnext, Some(42));
        assert_eq!(st.messages, Some(7));

        let sent = folders.iter().find(|f| f.name == "Sent").unwrap();
        assert!(sent.status.is_none());
    }

    #[test]
    fn collect_decodes_modified_utf7_names() {
        let resps = vec![lst("&ZeVnLIqe-", "/", &[])];
        let folders = DiscoveredFolder::collect(&resps, NameEncoding::ModifiedUtf7).unwrap();
        assert_eq!(folders[0].name, "日本語");
    }

    fn filters_default() -> FolderFilters {
        FolderFilters {
            include: Vec::new(),
            exclude: Vec::new(),
            exclude_special: Vec::new(),
            explicit: Vec::new(),
            scope: FolderScope::All,
            automap: Automap::Enabled,
            namespace_prefix: String::new(),
        }
    }

    #[test]
    fn filters_default_keep_everything() {
        let folders = DiscoveredFolder::collect(
            &[
                lst("INBOX", "/", &[]),
                lst("Sent", "/", &["\\Sent"]),
                lst("Drafts", "/", &[]),
            ],
            NameEncoding::ModifiedUtf7,
        )
        .unwrap();
        let res = filters_default().apply(folders);
        assert_eq!(res.len(), 3);
        let sent = res.iter().find(|f| f.name == "Sent").unwrap();
        assert_eq!(sent.role, Some("sent"));
    }

    #[test]
    fn include_excludes_non_matching() {
        let folders = DiscoveredFolder::collect(
            &[
                lst("INBOX", "/", &[]),
                lst("Trash", "/", &[]),
                lst("Sent", "/", &["\\Sent"]),
            ],
            NameEncoding::ModifiedUtf7,
        )
        .unwrap();
        let mut f = filters_default();
        f.include = vec![Regex::new(r"^(INBOX|Sent)$").unwrap()];
        let res = f.apply(folders);
        assert_eq!(res.len(), 2);
        assert!(res.iter().any(|x| x.name == "INBOX"));
        assert!(res.iter().any(|x| x.name == "Sent"));
    }

    #[test]
    fn exclude_drops_matching() {
        let folders = DiscoveredFolder::collect(
            &[lst("INBOX", "/", &[]), lst("Trash", "/", &[])],
            NameEncoding::ModifiedUtf7,
        )
        .unwrap();
        let mut f = filters_default();
        f.exclude = vec![Regex::new(r"^Trash$").unwrap()];
        let res = f.apply(folders);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "INBOX");
    }

    #[test]
    fn exclude_special_drops_by_role() {
        let folders = DiscoveredFolder::collect(
            &[lst("INBOX", "/", &[]), lst("Trash", "/", &["\\Trash"])],
            NameEncoding::ModifiedUtf7,
        )
        .unwrap();
        let mut f = filters_default();
        f.exclude_special = vec!["trash".to_owned()];
        let res = f.apply(folders);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "INBOX");
    }

    #[test]
    fn explicit_folder_list_overrides_include_exclude() {
        let folders = DiscoveredFolder::collect(
            &[
                lst("INBOX", "/", &[]),
                lst("Sent", "/", &[]),
                lst("Trash", "/", &[]),
            ],
            NameEncoding::ModifiedUtf7,
        )
        .unwrap();
        let mut f = filters_default();
        f.explicit = vec!["Sent".to_owned()];
        let res = f.apply(folders);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "Sent");
    }

    #[test]
    fn subscribed_only_filters_unsubscribed() {
        let mut folders = DiscoveredFolder::collect(
            &[lst("INBOX", "/", &["\\Subscribed"]), lst("Other", "/", &[])],
            NameEncoding::ModifiedUtf7,
        )
        .unwrap();

        let mut f = filters_default();
        f.scope = FolderScope::Subscribed;

        for x in &mut folders {
            if x.name == "INBOX" {
                x.subscribed = true;
            }
        }
        let res = f.apply(folders);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "INBOX");
    }

    #[test]
    fn split_parent_handles_root_and_nested() {
        assert_eq!(
            ResolvedFolder::path_parts("INBOX", Some('/')),
            ("INBOX".to_owned(), None)
        );
        assert_eq!(
            ResolvedFolder::path_parts("Projects/Alpha", Some('/')),
            ("Alpha".to_owned(), Some("Projects".to_owned()))
        );
        assert_eq!(
            ResolvedFolder::path_parts("Projects/Alpha/Beta", Some('/')),
            ("Beta".to_owned(), Some("Projects/Alpha".to_owned()))
        );
    }

    #[test]
    fn split_parent_with_no_delimiter_returns_root() {
        assert_eq!(
            ResolvedFolder::path_parts("Anything", None),
            ("Anything".to_owned(), None)
        );
    }

    #[test]
    fn sort_by_depth_places_roots_first() {
        let mut r = vec![
            ResolvedFolder {
                name: "Projects/Alpha".into(),
                wire_name: "Projects/Alpha".into(),
                leaf: "Alpha".into(),
                parent_path: Some("Projects".into()),
                delimiter: Some('/'),
                role: None,
                subscribed: false,
                selectable: true,
                status: None,
            },
            ResolvedFolder {
                name: "INBOX".into(),
                wire_name: "INBOX".into(),
                leaf: "INBOX".into(),
                parent_path: None,
                delimiter: Some('/'),
                role: Some("inbox"),
                subscribed: true,
                selectable: true,
                status: None,
            },
            ResolvedFolder {
                name: "Projects".into(),
                wire_name: "Projects".into(),
                leaf: "Projects".into(),
                parent_path: None,
                delimiter: Some('/'),
                role: None,
                subscribed: false,
                selectable: true,
                status: None,
            },
        ];
        ResolvedFolder::sort_parents_first(&mut r);
        assert_eq!(r[0].name, "INBOX");
        assert_eq!(r[1].name, "Projects");
        assert_eq!(r[2].name, "Projects/Alpha");
    }

    #[test]
    fn untagged_status_does_not_become_a_folder() {
        let resps = vec![
            lst("INBOX", "/", &[]),
            Untagged::Status {
                mailbox: "INBOX".to_owned(),
                items: BTreeMap::new(),
            },
        ];
        let folders = DiscoveredFolder::collect(&resps, NameEncoding::ModifiedUtf7).unwrap();
        assert_eq!(folders.len(), 1);
    }

    fn find<'a>(res: &'a [ResolvedFolder], name: &str) -> &'a ResolvedFolder {
        res.iter().find(|f| f.name == name).unwrap()
    }

    #[test]
    fn noselect_parent_is_kept_as_structural() {
        let folders = DiscoveredFolder::collect(
            &[
                lst("INBOX", "/", &["\\HasChildren"]),
                lst("INBOX/Projects", "/", &["\\Noselect", "\\HasChildren"]),
                lst("INBOX/Projects/Invoices", "/", &["\\HasNoChildren"]),
            ],
            NameEncoding::ModifiedUtf7,
        )
        .unwrap();
        let mut res = filters_default().apply(folders);
        ResolvedFolder::sort_parents_first(&mut res);
        let names: Vec<&str> = res.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            ["INBOX", "INBOX/Projects", "INBOX/Projects/Invoices"]
        );
        let projects = find(&res, "INBOX/Projects");
        assert!(!projects.selectable);
        assert_eq!(projects.role, None);
        assert_eq!(projects.leaf, "Projects");
        assert_eq!(projects.parent_path.as_deref(), Some("INBOX"));
        assert!(find(&res, "INBOX/Projects/Invoices").selectable);
    }

    #[test]
    fn noselect_without_kept_children_is_dropped() {
        let folders = DiscoveredFolder::collect(
            &[lst("INBOX", "/", &[]), lst("Empty", "/", &["\\Noselect"])],
            NameEncoding::ModifiedUtf7,
        )
        .unwrap();
        let res = filters_default().apply(folders);
        assert_eq!(res.len(), 1);
    }

    #[test]
    fn consecutive_structural_ancestors_and_synthesised_ones() {
        let folders = DiscoveredFolder::collect(
            &[lst("A", "/", &["\\NonExistent"]), lst("A/B/C/D", "/", &[])],
            NameEncoding::ModifiedUtf7,
        )
        .unwrap();
        let mut res = filters_default().apply(folders);
        ResolvedFolder::sort_parents_first(&mut res);
        let names: Vec<&str> = res.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["A", "A/B", "A/B/C", "A/B/C/D"]);
        assert!(
            res.iter()
                .take(3)
                .all(|f| !f.selectable && f.role.is_none())
        );
        assert_eq!(find(&res, "A/B/C").wire_name, "A/B/C");
        assert_eq!(find(&res, "A/B").leaf, "B");
    }

    #[test]
    fn trailing_space_and_same_leaf_under_different_parents() {
        let folders = DiscoveredFolder::collect(
            &[
                lst("Work ", ".", &["\\Noselect"]),
                lst("Work", ".", &[]),
                lst("Work .Archive", ".", &[]),
                lst("Work.Archive", ".", &[]),
            ],
            NameEncoding::ModifiedUtf7,
        )
        .unwrap();
        let res = filters_default().apply(folders);
        assert_eq!(res.len(), 4);
        let spaced = find(&res, "Work ");
        assert!(!spaced.selectable);
        assert_eq!(spaced.leaf, "Work ");
        assert_eq!(
            find(&res, "Work .Archive").parent_path.as_deref(),
            Some("Work ")
        );
        assert_eq!(
            find(&res, "Work.Archive").parent_path.as_deref(),
            Some("Work")
        );
        assert!(find(&res, "Work").selectable);
    }

    #[test]
    fn synthesised_ancestor_derives_wire_name_from_child() {
        let folders = DiscoveredFolder::collect(
            &[lst("&ZeVnLIqe-/Sub", "/", &[])],
            NameEncoding::ModifiedUtf7,
        )
        .unwrap();
        let res = filters_default().apply(folders);
        let parent = find(&res, "日本語");
        assert_eq!(parent.wire_name, "&ZeVnLIqe-");
        assert!(!parent.selectable);
    }

    #[test]
    fn filters_keep_needed_ancestors_as_structural() {
        let list = [
            lst("INBOX", "/", &[]),
            lst("Projects", "/", &[]),
            lst("Projects/Alpha", "/", &["\\Noselect"]),
            lst("Projects/Alpha/Docs", "/", &["\\Subscribed"]),
            lst("Other", "/", &[]),
        ];
        let check = |res: Vec<ResolvedFolder>| {
            assert_eq!(res.len(), 3, "{res:?}");
            assert!(!find(&res, "Projects").selectable);
            assert!(!find(&res, "Projects/Alpha").selectable);
            assert!(find(&res, "Projects/Alpha/Docs").selectable);
        };
        let mut f = filters_default();
        f.include = vec![Regex::new("Docs$").unwrap()];
        check(f.apply(DiscoveredFolder::collect(&list, NameEncoding::ModifiedUtf7).unwrap()));
        let mut f = filters_default();
        f.exclude = vec![Regex::new("^(INBOX|Other|Projects)$").unwrap()];
        check(f.apply(DiscoveredFolder::collect(&list, NameEncoding::ModifiedUtf7).unwrap()));
        let mut f = filters_default();
        f.explicit = vec!["Projects/Alpha/Docs".to_owned()];
        check(f.apply(DiscoveredFolder::collect(&list, NameEncoding::ModifiedUtf7).unwrap()));
        let mut f = filters_default();
        f.scope = FolderScope::Subscribed;
        check(f.apply(DiscoveredFolder::collect(&list, NameEncoding::ModifiedUtf7).unwrap()));
    }
}
