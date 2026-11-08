/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

use regex::Regex;

use crate::imap::{automap, name::decode_mailbox_name};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFolder {
    pub name: String,
    pub decoded_name: String,
    pub leaf: String,
    pub parent_path: Option<String>,
    pub path: PathBuf,
    pub role: Option<&'static str>,
    pub ephemeral: bool,
}

pub struct FolderFilters {
    pub include: Vec<Regex>,
    pub exclude: Vec<Regex>,
    pub explicit: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoverError {
    #[error("path does not exist: {0}")]
    NotFound(PathBuf),
    #[error("path is not a directory: {0}")]
    NotADirectory(PathBuf),
    #[error("not a Maildir: {0} (missing cur/ subdirectory)")]
    NotAMaildir(PathBuf),
    #[error(
        "only Maildir++ layout is supported: found directory {0:?} that looks like a non-prefixed \
         subfolder (subfolders must be named with a leading '.', e.g. '.Sent')"
    )]
    NotMaildirPlus(String),
    #[error("io error walking {0}: {1}")]
    Io(PathBuf, std::io::Error),
}

pub fn discover(root: &Path, automap_enabled: bool) -> Result<Vec<ResolvedFolder>, DiscoverError> {
    if !root.exists() {
        return Err(DiscoverError::NotFound(root.to_path_buf()));
    }
    let meta = std::fs::metadata(root).map_err(|e| DiscoverError::Io(root.to_path_buf(), e))?;
    if !meta.is_dir() {
        return Err(DiscoverError::NotADirectory(root.to_path_buf()));
    }
    if !root.join("cur").is_dir() {
        return Err(DiscoverError::NotAMaildir(root.to_path_buf()));
    }
    let mut folders = vec![ResolvedFolder {
        name: "INBOX".to_owned(),
        decoded_name: "INBOX".to_owned(),
        leaf: "INBOX".to_owned(),
        parent_path: None,
        path: root.to_path_buf(),
        role: Some("inbox"),
        ephemeral: false,
    }];

    let mut subfolders: Vec<(String, PathBuf)> = Vec::new();
    let entries = std::fs::read_dir(root).map_err(|e| DiscoverError::Io(root.to_path_buf(), e))?;
    for entry in entries {
        let entry = entry.map_err(|e| DiscoverError::Io(root.to_path_buf(), e))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(raw_name) = path.file_name() else {
            continue;
        };
        let name: String = raw_name.to_string_lossy().into_owned();
        if matches!(name.as_str(), "cur" | "new" | "tmp") {
            continue;
        }
        let has_cur = path.join("cur").is_dir();
        if let Some(stripped) = name.strip_prefix('.') {
            if has_cur && !stripped.is_empty() {
                subfolders.push((stripped.to_owned(), path));
            }
        } else if has_cur {
            return Err(DiscoverError::NotMaildirPlus(name));
        }
    }

    subfolders.sort_by(|a, b| a.0.cmp(&b.0));

    let mut known: Vec<String> = vec!["INBOX".to_owned()];
    for (canonical, path) in subfolders {
        let parent_path = split_parent(&canonical);
        if let Some(p) = parent_path.as_deref() {
            ensure_ephemeral_parents(p, &mut known, &mut folders, root);
        }
        let decoded_name = decode_name(&canonical);
        let role = if automap_enabled {
            automap::role_for_folder(&decoded_name, &[], "", true)
        } else {
            None
        };
        folders.push(ResolvedFolder {
            leaf: decode_segment(leaf_of(&canonical)).into_owned(),
            name: canonical.clone(),
            decoded_name,
            parent_path,
            path,
            role,
            ephemeral: false,
        });
        known.push(canonical);
    }
    Ok(folders)
}

fn ensure_ephemeral_parents(
    parent_path: &str,
    known: &mut Vec<String>,
    folders: &mut Vec<ResolvedFolder>,
    root: &Path,
) {
    for ancestor in ancestor_chain(parent_path) {
        if known.iter().any(|n| n == &ancestor) {
            continue;
        }
        let parent_path = split_parent(&ancestor);
        folders.push(ResolvedFolder {
            decoded_name: decode_name(&ancestor),
            leaf: decode_segment(leaf_of(&ancestor)).into_owned(),
            name: ancestor.clone(),
            parent_path,
            path: root.to_path_buf(),
            role: None,
            ephemeral: true,
        });
        known.push(ancestor);
    }
}

fn ancestor_chain(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut acc = String::new();
    for part in name.split('.') {
        if !acc.is_empty() {
            acc.push('.');
        }
        acc.push_str(part);
        out.push(acc.clone());
    }
    out
}

fn split_parent(name: &str) -> Option<String> {
    name.rfind('.').map(|i| name[..i].to_owned())
}

fn decode_segment(segment: &str) -> Cow<'_, str> {
    if !segment.contains('&') {
        return Cow::Borrowed(segment);
    }
    decode_mailbox_name(segment).map_or(Cow::Borrowed(segment), Cow::Owned)
}

fn decode_name(name: &str) -> String {
    let mut decoded = String::with_capacity(name.len());
    for (i, segment) in name.split('.').enumerate() {
        if i > 0 {
            decoded.push('.');
        }
        decoded.push_str(&decode_segment(segment));
    }
    decoded
}

fn leaf_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) => &name[i + 1..],
        None => name,
    }
}

pub fn apply_filters(folders: Vec<ResolvedFolder>, filters: &FolderFilters) -> Vec<ResolvedFolder> {
    folders
        .into_iter()
        .filter(|f| match_filters(&f.decoded_name, filters))
        .collect()
}

pub fn restore_ephemeral_parents(folders: &mut Vec<ResolvedFolder>, root: &Path) {
    let known: std::collections::HashSet<String> = folders.iter().map(|f| f.name.clone()).collect();
    let mut to_add: Vec<ResolvedFolder> = Vec::new();
    for f in folders.iter() {
        if let Some(parent) = f.parent_path.as_deref() {
            for ancestor in ancestor_chain(parent) {
                if known.contains(&ancestor) || to_add.iter().any(|x| x.name == ancestor) {
                    continue;
                }
                let parent_path = split_parent(&ancestor);
                to_add.push(ResolvedFolder {
                    decoded_name: decode_name(&ancestor),
                    leaf: decode_segment(leaf_of(&ancestor)).into_owned(),
                    name: ancestor,
                    parent_path,
                    path: root.to_path_buf(),
                    role: None,
                    ephemeral: true,
                });
            }
        }
    }
    if to_add.is_empty() {
        return;
    }
    folders.extend(to_add);
    folders.sort_by_key(|f| f.name.matches('.').count());
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

pub fn vanished_folders<'a, I>(local: I, server: &[ResolvedFolder]) -> Vec<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let server_set: std::collections::HashSet<&str> =
        server.iter().map(|f| f.name.as_str()).collect();
    local
        .into_iter()
        .filter(|n| !server_set.contains(*n))
        .map(str::to_owned)
        .collect()
}

pub fn vanished_depth_sort(names: &mut [String]) {
    names.sort_by(|a, b| {
        let da = a.matches('.').count();
        let db = b.matches('.').count();
        db.cmp(&da).then_with(|| a.cmp(b))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_maildir(root: &Path, folders: &[&str]) {
        for sub in ["cur", "new", "tmp"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        for name in folders {
            let p = root.join(name);
            for sub in ["cur", "new", "tmp"] {
                fs::create_dir_all(p.join(sub)).unwrap();
            }
        }
    }

    #[test]
    fn discover_just_inbox_when_root_alone() {
        let td = tempfile::tempdir().unwrap();
        make_maildir(td.path(), &[]);
        let folders = discover(td.path(), true).unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].name, "INBOX");
        assert_eq!(folders[0].role, Some("inbox"));
        assert!(folders[0].parent_path.is_none());
    }

    #[test]
    fn discover_dovecot_style_subfolders() {
        let td = tempfile::tempdir().unwrap();
        make_maildir(td.path(), &[".Sent", ".Drafts", ".Trash", ".Junk"]);
        let folders = discover(td.path(), true).unwrap();
        let names: Vec<&str> = folders.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"INBOX"));
        assert!(names.contains(&"Sent"));
        assert!(names.contains(&"Drafts"));
        let sent = folders.iter().find(|f| f.name == "Sent").unwrap();
        assert_eq!(sent.role, Some("sent"));
        let drafts = folders.iter().find(|f| f.name == "Drafts").unwrap();
        assert_eq!(drafts.role, Some("drafts"));
        let trash = folders.iter().find(|f| f.name == "Trash").unwrap();
        assert_eq!(trash.role, Some("trash"));
        let junk = folders.iter().find(|f| f.name == "Junk").unwrap();
        assert_eq!(junk.role, Some("junk"));
    }

    #[test]
    fn discover_courier_maildir_plus_deep_hierarchy() {
        let td = tempfile::tempdir().unwrap();
        make_maildir(td.path(), &[".Archive", ".Archive.2024", ".Archive.2025"]);
        let folders = discover(td.path(), true).unwrap();
        let names: Vec<&str> = folders.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"Archive"));
        assert!(names.contains(&"Archive.2024"));
        assert!(names.contains(&"Archive.2025"));
        let a2025 = folders.iter().find(|f| f.name == "Archive.2025").unwrap();
        assert_eq!(a2025.leaf, "2025");
        assert_eq!(a2025.parent_path.as_deref(), Some("Archive"));
        let archive = folders.iter().find(|f| f.name == "Archive").unwrap();
        assert_eq!(archive.role, Some("archive"));

        assert!(a2025.role.is_none());
    }

    #[test]
    fn discover_inserts_ephemeral_parent_for_orphan_branch() {
        let td = tempfile::tempdir().unwrap();
        make_maildir(td.path(), &[".Lists.maildir-dev"]);
        let folders = discover(td.path(), true).unwrap();
        let lists = folders.iter().find(|f| f.name == "Lists").unwrap();
        assert!(lists.ephemeral);
        assert!(lists.role.is_none());
        assert!(lists.parent_path.is_none());
        let leaf = folders
            .iter()
            .find(|f| f.name == "Lists.maildir-dev")
            .unwrap();
        assert!(!leaf.ephemeral);
        assert_eq!(leaf.parent_path.as_deref(), Some("Lists"));
        assert_eq!(leaf.leaf, "maildir-dev");
    }

    #[test]
    fn discover_skips_non_dot_subfolders_silently_only_if_no_cur() {
        let td = tempfile::tempdir().unwrap();
        make_maildir(td.path(), &[".Sent"]);

        fs::create_dir(td.path().join(".dovecot.imap")).unwrap();
        fs::write(td.path().join("dovecot.index.log"), "").unwrap();
        let folders = discover(td.path(), true).unwrap();
        assert_eq!(folders.len(), 2);
        assert!(folders.iter().any(|f| f.name == "INBOX"));
        assert!(folders.iter().any(|f| f.name == "Sent"));
    }

    #[test]
    fn discover_rejects_dovecot_layout_fs_tree() {
        let td = tempfile::tempdir().unwrap();
        make_maildir(td.path(), &["Sent"]);
        let err = discover(td.path(), true).unwrap_err();
        assert!(matches!(err, DiscoverError::NotMaildirPlus(_)));
    }

    #[test]
    fn discover_rejects_path_without_cur_subdir() {
        let td = tempfile::tempdir().unwrap();
        fs::create_dir_all(td.path().join("new")).unwrap();
        let err = discover(td.path(), true).unwrap_err();
        assert!(matches!(err, DiscoverError::NotAMaildir(_)));
    }

    #[test]
    fn discover_rejects_nonexistent_path() {
        let err = discover(Path::new("/definitely/not/a/maildir/at/all"), true).unwrap_err();
        assert!(matches!(err, DiscoverError::NotFound(_)));
    }

    #[test]
    fn discover_returns_parents_before_children() {
        let td = tempfile::tempdir().unwrap();
        make_maildir(
            td.path(),
            &[".Archive.2025", ".Archive", ".Lists.maildir-dev"],
        );
        let folders = discover(td.path(), true).unwrap();
        let index = |name: &str| folders.iter().position(|f| f.name == name).unwrap();
        assert!(index("Archive") < index("Archive.2025"));
        assert!(index("Lists") < index("Lists.maildir-dev"));
        assert_eq!(index("INBOX"), 0);
    }

    #[test]
    fn ancestor_chain_walks_dot_segments() {
        assert_eq!(ancestor_chain("A"), vec!["A".to_owned()]);
        assert_eq!(
            ancestor_chain("A.B.C"),
            vec!["A".to_owned(), "A.B".to_owned(), "A.B.C".to_owned()]
        );
    }

    #[test]
    fn leaf_and_split_handle_root_and_nested() {
        assert_eq!(leaf_of("INBOX"), "INBOX");
        assert_eq!(leaf_of("Archive.2025"), "2025");
        assert_eq!(split_parent("INBOX"), None);
        assert_eq!(split_parent("Archive.2025"), Some("Archive".to_owned()));
    }

    fn folders_for(names: &[&str]) -> Vec<ResolvedFolder> {
        names
            .iter()
            .map(|n| ResolvedFolder {
                name: (*n).to_owned(),
                decoded_name: (*n).to_owned(),
                leaf: (*n).to_owned(),
                parent_path: None,
                path: PathBuf::new(),
                role: None,
                ephemeral: false,
            })
            .collect()
    }

    #[test]
    fn include_filter_keeps_only_matches() {
        let folders = folders_for(&["INBOX", "Sent", "Trash"]);
        let filters = FolderFilters {
            include: vec![Regex::new("^(INBOX|Sent)$").unwrap()],
            exclude: Vec::new(),
            explicit: Vec::new(),
        };
        let kept = apply_filters(folders, &filters);
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().any(|f| f.name == "INBOX"));
        assert!(kept.iter().any(|f| f.name == "Sent"));
    }

    #[test]
    fn exclude_filter_drops_matches() {
        let folders = folders_for(&["INBOX", "Trash"]);
        let filters = FolderFilters {
            include: Vec::new(),
            exclude: vec![Regex::new("^Trash$").unwrap()],
            explicit: Vec::new(),
        };
        let kept = apply_filters(folders, &filters);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name, "INBOX");
    }

    #[test]
    fn explicit_folder_overrides_include_exclude() {
        let folders = folders_for(&["INBOX", "Sent", "Trash"]);
        let filters = FolderFilters {
            include: Vec::new(),
            exclude: Vec::new(),
            explicit: vec!["Sent".to_owned()],
        };
        let kept = apply_filters(folders, &filters);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name, "Sent");
    }

    #[test]
    fn discover_with_automap_off_clears_roles_except_inbox() {
        let td = tempfile::tempdir().unwrap();
        make_maildir(td.path(), &[".Sent", ".Drafts"]);
        let folders = discover(td.path(), false).unwrap();
        let inbox = folders.iter().find(|f| f.name == "INBOX").unwrap();
        assert_eq!(inbox.role, Some("inbox"));
        let sent = folders.iter().find(|f| f.name == "Sent").unwrap();
        assert_eq!(sent.role, None);
        let drafts = folders.iter().find(|f| f.name == "Drafts").unwrap();
        assert_eq!(drafts.role, None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn discover_renders_non_utf8_folder_name_lossy() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let td = tempfile::tempdir().unwrap();
        make_maildir(td.path(), &[]);
        let mut bytes = b".Foo".to_vec();
        bytes.extend_from_slice(&[0xff, 0xfe]);
        let dotted = OsStr::from_bytes(&bytes);
        let p = td.path().join(dotted);
        for sub in ["cur", "new", "tmp"] {
            fs::create_dir_all(p.join(sub)).unwrap();
        }
        let folders = discover(td.path(), true).unwrap();
        assert_eq!(folders.len(), 2);
        let sub = folders.iter().find(|f| f.name != "INBOX").unwrap();
        assert!(
            sub.name.contains('\u{FFFD}'),
            "lossy substitution: {:?}",
            sub.name
        );
        assert!(sub.name.starts_with("Foo"));
    }

    #[test]
    fn vanished_depth_sort_deepest_first() {
        let mut names = vec![
            "Archive".to_owned(),
            "Archive.2025.January".to_owned(),
            "Archive.2025".to_owned(),
        ];
        vanished_depth_sort(&mut names);
        assert_eq!(
            names,
            vec![
                "Archive.2025.January".to_owned(),
                "Archive.2025".to_owned(),
                "Archive".to_owned(),
            ]
        );
    }

    // Dovecot stores non-ASCII and '&' in folder names as modified UTF-7.
    #[test]
    fn discover_decodes_modified_utf7_names() {
        let td = tempfile::tempdir().unwrap();
        make_maildir(
            td.path(),
            &[".&AOk-t&AOk-", ".R&-D", ".Plain", ".&ZeVnLIqe-.&U,BTFw-"],
        );
        let folders = discover(td.path(), true).unwrap();
        let by_name = |name: &str| folders.iter().find(|f| f.name == name).unwrap();

        let ete = by_name("&AOk-t&AOk-");
        assert_eq!(ete.decoded_name, "été");
        assert_eq!(ete.leaf, "été");
        assert_eq!(by_name("R&-D").leaf, "R&D");
        assert_eq!(by_name("Plain").leaf, "Plain");

        let nested = by_name("&ZeVnLIqe-.&U,BTFw-");
        assert_eq!(nested.decoded_name, "日本語.台北");
        assert_eq!(nested.leaf, "台北");
        assert_eq!(nested.parent_path.as_deref(), Some("&ZeVnLIqe-"));
        let parent = by_name("&ZeVnLIqe-");
        assert!(parent.ephemeral);
        assert_eq!(parent.leaf, "日本語");
    }

    // Names that are not valid modified UTF-7 are kept byte for byte.
    #[test]
    fn decode_keeps_literal_ampersands_and_utf8() {
        for name in ["R&D", "AT&T", "Tom & Jerry-2", "Été", "Sales&", "A&-B&C"] {
            assert_eq!(decode_name(name), name);
        }
        assert_eq!(decode_name("R&D.&AOk-t&AOk-"), "R&D.été");
    }

    #[test]
    fn filters_match_decoded_names() {
        let mut folders = folders_for(&["INBOX", "Plain"]);
        folders.push(ResolvedFolder {
            name: "&AOk-t&AOk-".to_owned(),
            decoded_name: "été".to_owned(),
            leaf: "été".to_owned(),
            parent_path: None,
            path: PathBuf::new(),
            role: None,
            ephemeral: false,
        });
        let filters = FolderFilters {
            include: Vec::new(),
            exclude: Vec::new(),
            explicit: vec!["été".to_owned()],
        };
        let kept = apply_filters(folders, &filters);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name, "&AOk-t&AOk-");
    }
}
