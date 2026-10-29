/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{BTreeMap, HashMap, HashSet};

use regex::Regex;

use crate::imap::automap;
use crate::imap::name::{canonicalise_inbox, decode_mailbox_name_with};
use crate::imap::response::Untagged;

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

pub struct FolderFilters {
    pub include: Vec<Regex>,
    pub exclude: Vec<Regex>,
    pub exclude_special: Vec<String>,
    pub explicit: Vec<String>,
    pub subscribed_only: bool,
    pub automap_enabled: bool,
    pub namespace_prefix: String,
}

pub fn collect_from_list(
    untagged: &[Untagged],
    utf8_accept: bool,
) -> Result<Vec<DiscoveredFolder>, String> {
    let mut subscribed = HashSet::new();
    let mut statuses: BTreeMap<String, FolderStatus> = BTreeMap::new();
    let mut found = BTreeMap::new();
    for u in untagged {
        match u {
            Untagged::Lsub { name, .. } => {
                let decoded =
                    decode_mailbox_name_with(name, utf8_accept).map_err(|e| e.to_string())?;
                subscribed.insert(canonicalise_inbox(&decoded));
            }
            Untagged::List {
                attributes,
                delimiter,
                name,
            } => {
                let decoded =
                    decode_mailbox_name_with(name, utf8_accept).map_err(|e| e.to_string())?;
                let canonical = canonicalise_inbox(&decoded);
                let attrs_lower: Vec<String> =
                    attributes.iter().map(|a| a.to_ascii_lowercase()).collect();
                let selectable = !attrs_lower
                    .iter()
                    .any(|a| a == "\\noselect" || a == "\\nonexistent");
                let extended_subscribed = attrs_lower.iter().any(|a| a == "\\subscribed");
                if extended_subscribed {
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
                let decoded =
                    decode_mailbox_name_with(mailbox, utf8_accept).map_err(|e| e.to_string())?;
                let canonical = canonicalise_inbox(&decoded);
                let mut st = FolderStatus::default();
                if let Some(v) = items.get("UIDVALIDITY") {
                    st.uidvalidity = Some(*v);
                }
                if let Some(v) = items.get("UIDNEXT") {
                    st.uidnext = Some(*v);
                }
                if let Some(v) = items.get("MESSAGES") {
                    st.messages = Some(*v);
                }
                statuses.insert(canonical, st);
            }
            _ => {}
        }
    }
    let mut out: Vec<DiscoveredFolder> = found.into_values().collect();
    for f in &mut out {
        if subscribed.contains(&f.name) {
            f.subscribed = true;
        }
        if let Some(s) = statuses.remove(&f.name) {
            f.status = Some(s);
        }
    }
    Ok(out)
}

pub fn apply_filters(
    folders: Vec<DiscoveredFolder>,
    filters: &FolderFilters,
) -> Vec<ResolvedFolder> {
    let mut resolved: Vec<ResolvedFolder> = Vec::with_capacity(folders.len());
    let mut others: HashMap<String, DiscoveredFolder> = HashMap::new();
    for f in folders {
        if !f.selectable
            || !match_filters(&f.name, filters)
            || (filters.subscribed_only && !f.subscribed)
        {
            others.insert(f.name.clone(), f);
            continue;
        }
        let role = automap::role_for_folder(
            &f.name,
            &f.attributes,
            &filters.namespace_prefix,
            filters.automap_enabled,
        );
        if let Some(r) = role
            && filters
                .exclude_special
                .iter()
                .any(|s| s.eq_ignore_ascii_case(r))
        {
            others.insert(f.name.clone(), f);
            continue;
        }
        let (leaf, parent_path) = split_parent(&f.name, f.delimiter);
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
                    let (leaf, parent_path) = split_parent(&d.name, delimiter);
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
                    let wire_name = match delim {
                        Some(d) => child_wire
                            .rsplit_once(d)
                            .map_or_else(|| name.clone(), |(p, _)| p.to_owned()),
                        None => name.clone(),
                    };
                    let (leaf, parent_path) = split_parent(&name, delim);
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

fn match_filters(name: &str, filters: &FolderFilters) -> bool {
    if !filters.explicit.is_empty() {
        return filters.explicit.iter().any(|e| e == name);
    }
    if !filters.include.is_empty() && !filters.include.iter().any(|r| r.is_match(name)) {
        return false;
    }
    if filters.exclude.iter().any(|r| r.is_match(name)) {
        return false;
    }
    true
}

fn split_parent(name: &str, delim: Option<char>) -> (String, Option<String>) {
    let Some(d) = delim else {
        return (name.to_owned(), None);
    };
    if let Some(idx) = name.rfind(d) {
        let parent = &name[..idx];
        let leaf = &name[idx + d.len_utf8()..];
        (leaf.to_owned(), Some(parent.to_owned()))
    } else {
        (name.to_owned(), None)
    }
}

pub fn sort_by_depth(resolved: &mut [ResolvedFolder]) {
    resolved.sort_by(|a, b| {
        let da = a.parent_path.as_deref().map_or(0, |s| {
            a.delimiter.map(|d| s.matches(d).count() + 1).unwrap_or(0)
        });
        let db = b.parent_path.as_deref().map_or(0, |s| {
            b.delimiter.map(|d| s.matches(d).count() + 1).unwrap_or(0)
        });
        da.cmp(&db).then_with(|| a.name.cmp(&b.name))
    });
}

pub fn vanished_folders(local: &HashMap<String, i64>, server: &HashSet<String>) -> Vec<String> {
    local
        .keys()
        .filter(|k| !server.contains(*k))
        .cloned()
        .collect()
}

pub fn vanished_depth_sort(names: &mut [String], delimiter: char) {
    names.sort_by(|a, b| {
        let depth_a = a.matches(delimiter).count();
        let depth_b = b.matches(delimiter).count();
        depth_b.cmp(&depth_a).then_with(|| a.cmp(b))
    });
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
        let folders = collect_from_list(&resps, false).unwrap();
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
        let folders = collect_from_list(&resps, false).unwrap();
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
        let folders = collect_from_list(&resps, false).unwrap();
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
        let folders = collect_from_list(&resps, false).unwrap();
        assert_eq!(folders[0].name, "日本語");
    }

    fn filters_default() -> FolderFilters {
        FolderFilters {
            include: Vec::new(),
            exclude: Vec::new(),
            exclude_special: Vec::new(),
            explicit: Vec::new(),
            subscribed_only: false,
            automap_enabled: true,
            namespace_prefix: String::new(),
        }
    }

    #[test]
    fn filters_default_keep_everything() {
        let folders = collect_from_list(
            &[
                lst("INBOX", "/", &[]),
                lst("Sent", "/", &["\\Sent"]),
                lst("Drafts", "/", &[]),
            ],
            false,
        )
        .unwrap();
        let res = apply_filters(folders, &filters_default());
        assert_eq!(res.len(), 3);
        let sent = res.iter().find(|f| f.name == "Sent").unwrap();
        assert_eq!(sent.role, Some("sent"));
    }

    #[test]
    fn include_excludes_non_matching() {
        let folders = collect_from_list(
            &[
                lst("INBOX", "/", &[]),
                lst("Trash", "/", &[]),
                lst("Sent", "/", &["\\Sent"]),
            ],
            false,
        )
        .unwrap();
        let mut f = filters_default();
        f.include = vec![Regex::new(r"^(INBOX|Sent)$").unwrap()];
        let res = apply_filters(folders, &f);
        assert_eq!(res.len(), 2);
        assert!(res.iter().any(|x| x.name == "INBOX"));
        assert!(res.iter().any(|x| x.name == "Sent"));
    }

    #[test]
    fn exclude_drops_matching() {
        let folders =
            collect_from_list(&[lst("INBOX", "/", &[]), lst("Trash", "/", &[])], false).unwrap();
        let mut f = filters_default();
        f.exclude = vec![Regex::new(r"^Trash$").unwrap()];
        let res = apply_filters(folders, &f);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "INBOX");
    }

    #[test]
    fn exclude_special_drops_by_role() {
        let folders = collect_from_list(
            &[lst("INBOX", "/", &[]), lst("Trash", "/", &["\\Trash"])],
            false,
        )
        .unwrap();
        let mut f = filters_default();
        f.exclude_special = vec!["trash".to_owned()];
        let res = apply_filters(folders, &f);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "INBOX");
    }

    #[test]
    fn explicit_folder_list_overrides_include_exclude() {
        let folders = collect_from_list(
            &[
                lst("INBOX", "/", &[]),
                lst("Sent", "/", &[]),
                lst("Trash", "/", &[]),
            ],
            false,
        )
        .unwrap();
        let mut f = filters_default();
        f.explicit = vec!["Sent".to_owned()];
        let res = apply_filters(folders, &f);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "Sent");
    }

    #[test]
    fn subscribed_only_filters_unsubscribed() {
        let mut folders = collect_from_list(
            &[lst("INBOX", "/", &["\\Subscribed"]), lst("Other", "/", &[])],
            false,
        )
        .unwrap();

        let mut f = filters_default();
        f.subscribed_only = true;

        for x in &mut folders {
            if x.name == "INBOX" {
                x.subscribed = true;
            }
        }
        let res = apply_filters(folders, &f);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "INBOX");
    }

    #[test]
    fn split_parent_handles_root_and_nested() {
        assert_eq!(split_parent("INBOX", Some('/')), ("INBOX".to_owned(), None));
        assert_eq!(
            split_parent("Projects/Alpha", Some('/')),
            ("Alpha".to_owned(), Some("Projects".to_owned()))
        );
        assert_eq!(
            split_parent("Projects/Alpha/Beta", Some('/')),
            ("Beta".to_owned(), Some("Projects/Alpha".to_owned()))
        );
    }

    #[test]
    fn split_parent_with_no_delimiter_returns_root() {
        assert_eq!(
            split_parent("Anything", None),
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
        sort_by_depth(&mut r);
        assert_eq!(r[0].name, "INBOX");
        assert_eq!(r[1].name, "Projects");
        assert_eq!(r[2].name, "Projects/Alpha");
    }

    #[test]
    fn vanished_folders_is_set_minus() {
        let mut local = HashMap::new();
        local.insert("INBOX".to_owned(), 1);
        local.insert("Old".to_owned(), 2);
        local.insert("Sent".to_owned(), 3);
        let mut server = HashSet::new();
        server.insert("INBOX".to_owned());
        server.insert("Sent".to_owned());
        let v = vanished_folders(&local, &server);
        assert_eq!(v, vec!["Old".to_owned()]);
    }

    #[test]
    fn vanished_depth_sort_deepest_first() {
        let mut names = vec!["A".to_owned(), "A/B/C".to_owned(), "A/B".to_owned()];
        vanished_depth_sort(&mut names, '/');
        assert_eq!(names, vec!["A/B/C", "A/B", "A"]);
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
        let folders = collect_from_list(&resps, false).unwrap();
        assert_eq!(folders.len(), 1);
    }

    fn find<'a>(res: &'a [ResolvedFolder], name: &str) -> &'a ResolvedFolder {
        res.iter().find(|f| f.name == name).unwrap()
    }

    #[test]
    fn noselect_parent_is_kept_as_structural() {
        let folders = collect_from_list(
            &[
                lst("INBOX", "/", &["\\HasChildren"]),
                lst("INBOX/Projects", "/", &["\\Noselect", "\\HasChildren"]),
                lst("INBOX/Projects/Invoices", "/", &["\\HasNoChildren"]),
            ],
            false,
        )
        .unwrap();
        let mut res = apply_filters(folders, &filters_default());
        sort_by_depth(&mut res);
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
        let folders = collect_from_list(
            &[lst("INBOX", "/", &[]), lst("Empty", "/", &["\\Noselect"])],
            false,
        )
        .unwrap();
        let res = apply_filters(folders, &filters_default());
        assert_eq!(res.len(), 1);
    }

    #[test]
    fn consecutive_structural_ancestors_and_synthesised_ones() {
        let folders = collect_from_list(
            &[lst("A", "/", &["\\NonExistent"]), lst("A/B/C/D", "/", &[])],
            false,
        )
        .unwrap();
        let mut res = apply_filters(folders, &filters_default());
        sort_by_depth(&mut res);
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
        let folders = collect_from_list(
            &[
                lst("Work ", ".", &["\\Noselect"]),
                lst("Work", ".", &[]),
                lst("Work .Archive", ".", &[]),
                lst("Work.Archive", ".", &[]),
            ],
            false,
        )
        .unwrap();
        let res = apply_filters(folders, &filters_default());
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
        let folders = collect_from_list(&[lst("&ZeVnLIqe-/Sub", "/", &[])], false).unwrap();
        let res = apply_filters(folders, &filters_default());
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
        check(apply_filters(collect_from_list(&list, false).unwrap(), &f));
        let mut f = filters_default();
        f.exclude = vec![Regex::new("^(INBOX|Other|Projects)$").unwrap()];
        check(apply_filters(collect_from_list(&list, false).unwrap(), &f));
        let mut f = filters_default();
        f.explicit = vec!["Projects/Alpha/Docs".to_owned()];
        check(apply_filters(collect_from_list(&list, false).unwrap(), &f));
        let mut f = filters_default();
        f.subscribed_only = true;
        check(apply_filters(collect_from_list(&list, false).unwrap(), &f));
    }
}
