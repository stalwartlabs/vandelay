/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::borrow::Cow;
use std::collections::HashSet;
use std::fs;
use std::iter;
use std::path::{Path, PathBuf};

use regex::Regex;

use crate::imap::automap::Automap;
use crate::imap::name::NameEncoding;
use crate::sync::tree::FolderPath;

const INBOX: &str = "INBOX";
pub(super) const DELIMITER: char = '.';

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Presence {
    OnDisk,
    Ephemeral,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResolvedFolder {
    pub name: String,
    pub decoded_name: String,
    pub leaf: String,
    pub parent_path: Option<String>,
    pub path: PathBuf,
    pub role: Option<&'static str>,
    pub presence: Presence,
}

pub(super) struct FolderFilters {
    pub include: Vec<Regex>,
    pub exclude: Vec<Regex>,
    pub explicit: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum DiscoverError {
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

#[derive(Debug, Clone, Copy)]
pub(super) struct MaildirName<'a>(pub &'a str);

impl<'a> MaildirName<'a> {
    fn folder_path(self) -> FolderPath<'a> {
        FolderPath::new(self.0, DELIMITER)
    }

    pub fn parent(self) -> Option<&'a str> {
        self.folder_path().parent()
    }

    pub fn leaf(self) -> &'a str {
        self.folder_path().leaf()
    }

    pub fn depth(self) -> usize {
        self.folder_path().depth()
    }

    pub fn ancestors_and_self(self) -> impl Iterator<Item = &'a str> {
        let name = self.0;
        name.char_indices()
            .filter(|&(_, c)| c == DELIMITER)
            .filter_map(move |(end, _)| name.get(..end))
            .chain(iter::once(name))
    }

    fn decode_segment(segment: &str) -> Cow<'_, str> {
        if !segment.contains('&') {
            return Cow::Borrowed(segment);
        }
        NameEncoding::ModifiedUtf7
            .decode(segment)
            .map_or(Cow::Borrowed(segment), Cow::Owned)
    }

    pub fn decoded(self) -> String {
        let mut decoded = String::with_capacity(self.0.len());
        for (index, segment) in self.0.split(DELIMITER).enumerate() {
            if index > 0 {
                decoded.push(DELIMITER);
            }
            decoded.push_str(&MaildirName::decode_segment(segment));
        }
        decoded
    }

    pub fn decoded_leaf(self) -> String {
        MaildirName::decode_segment(self.leaf()).into_owned()
    }
}

impl ResolvedFolder {
    fn ephemeral(name: &str, root: &Path) -> ResolvedFolder {
        let maildir = MaildirName(name);
        ResolvedFolder {
            decoded_name: maildir.decoded(),
            leaf: maildir.decoded_leaf(),
            parent_path: maildir.parent().map(str::to_owned),
            name: name.to_owned(),
            path: root.to_path_buf(),
            role: None,
            presence: Presence::Ephemeral,
        }
    }

    pub fn is_ephemeral(&self) -> bool {
        self.presence == Presence::Ephemeral
    }

    pub fn discover(root: &Path, automap: Automap) -> Result<Vec<ResolvedFolder>, DiscoverError> {
        if !root.exists() {
            return Err(DiscoverError::NotFound(root.to_path_buf()));
        }
        let meta = fs::metadata(root).map_err(|e| DiscoverError::Io(root.to_path_buf(), e))?;
        if !meta.is_dir() {
            return Err(DiscoverError::NotADirectory(root.to_path_buf()));
        }
        if !root.join("cur").is_dir() {
            return Err(DiscoverError::NotAMaildir(root.to_path_buf()));
        }
        let mut folders = vec![ResolvedFolder {
            name: INBOX.to_owned(),
            decoded_name: INBOX.to_owned(),
            leaf: INBOX.to_owned(),
            parent_path: None,
            path: root.to_path_buf(),
            role: Some("inbox"),
            presence: Presence::OnDisk,
        }];

        let mut subfolders: Vec<(String, PathBuf)> = Vec::new();
        let entries = fs::read_dir(root).map_err(|e| DiscoverError::Io(root.to_path_buf(), e))?;
        for entry in entries {
            let entry = entry.map_err(|e| DiscoverError::Io(root.to_path_buf(), e))?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(raw_name) = path.file_name() else {
                continue;
            };
            let name = raw_name.to_string_lossy();
            if matches!(name.as_ref(), "cur" | "new" | "tmp") {
                continue;
            }
            let has_cur = path.join("cur").is_dir();
            if let Some(stripped) = name.strip_prefix(DELIMITER) {
                if has_cur && !stripped.is_empty() {
                    subfolders.push((stripped.to_owned(), path));
                }
            } else if has_cur {
                return Err(DiscoverError::NotMaildirPlus(name.into_owned()));
            }
        }

        subfolders.sort_by(|a, b| a.0.cmp(&b.0));

        let mut known: HashSet<String> = HashSet::with_capacity(subfolders.len() + 1);
        known.insert(INBOX.to_owned());
        folders.reserve(subfolders.len());
        for (canonical, path) in subfolders {
            let maildir = MaildirName(&canonical);
            if let Some(parent) = maildir.parent() {
                for ancestor in MaildirName(parent).ancestors_and_self() {
                    if !known.contains(ancestor) {
                        known.insert(ancestor.to_owned());
                        folders.push(ResolvedFolder::ephemeral(ancestor, root));
                    }
                }
            }
            let decoded_name = maildir.decoded();
            let role = match automap {
                Automap::Enabled => automap.role(&decoded_name, &[], ""),
                Automap::Disabled => None,
            };
            folders.push(ResolvedFolder {
                leaf: maildir.decoded_leaf(),
                parent_path: maildir.parent().map(str::to_owned),
                name: canonical.clone(),
                decoded_name,
                path,
                role,
                presence: Presence::OnDisk,
            });
            known.insert(canonical);
        }
        Ok(folders)
    }

    pub fn restore_ephemeral_parents(folders: &mut Vec<ResolvedFolder>, root: &Path) {
        let mut known: HashSet<&str> = folders.iter().map(|f| f.name.as_str()).collect();
        let mut missing: Vec<ResolvedFolder> = Vec::new();
        for parent in folders.iter().filter_map(|f| f.parent_path.as_deref()) {
            for ancestor in MaildirName(parent).ancestors_and_self() {
                if known.insert(ancestor) {
                    missing.push(ResolvedFolder::ephemeral(ancestor, root));
                }
            }
        }
        if missing.is_empty() {
            return;
        }
        folders.extend(missing);
        folders.sort_by_key(|f| MaildirName(&f.name).depth());
    }
}

impl FolderFilters {
    pub fn apply(&self, folders: Vec<ResolvedFolder>) -> Vec<ResolvedFolder> {
        folders
            .into_iter()
            .filter(|f| self.matches(&f.decoded_name))
            .collect()
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
        let folders = ResolvedFolder::discover(td.path(), Automap::Enabled).unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].name, "INBOX");
        assert_eq!(folders[0].role, Some("inbox"));
        assert!(folders[0].parent_path.is_none());
    }

    #[test]
    fn discover_dovecot_style_subfolders() {
        let td = tempfile::tempdir().unwrap();
        make_maildir(td.path(), &[".Sent", ".Drafts", ".Trash", ".Junk"]);
        let folders = ResolvedFolder::discover(td.path(), Automap::Enabled).unwrap();
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
        let folders = ResolvedFolder::discover(td.path(), Automap::Enabled).unwrap();
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
        let folders = ResolvedFolder::discover(td.path(), Automap::Enabled).unwrap();
        let lists = folders.iter().find(|f| f.name == "Lists").unwrap();
        assert!(lists.is_ephemeral());
        assert!(lists.role.is_none());
        assert!(lists.parent_path.is_none());
        let leaf = folders
            .iter()
            .find(|f| f.name == "Lists.maildir-dev")
            .unwrap();
        assert!(!leaf.is_ephemeral());
        assert_eq!(leaf.parent_path.as_deref(), Some("Lists"));
        assert_eq!(leaf.leaf, "maildir-dev");
    }

    #[test]
    fn discover_skips_non_dot_subfolders_silently_only_if_no_cur() {
        let td = tempfile::tempdir().unwrap();
        make_maildir(td.path(), &[".Sent"]);

        fs::create_dir(td.path().join(".dovecot.imap")).unwrap();
        fs::write(td.path().join("dovecot.index.log"), "").unwrap();
        let folders = ResolvedFolder::discover(td.path(), Automap::Enabled).unwrap();
        assert_eq!(folders.len(), 2);
        assert!(folders.iter().any(|f| f.name == "INBOX"));
        assert!(folders.iter().any(|f| f.name == "Sent"));
    }

    #[test]
    fn discover_rejects_dovecot_layout_fs_tree() {
        let td = tempfile::tempdir().unwrap();
        make_maildir(td.path(), &["Sent"]);
        let err = ResolvedFolder::discover(td.path(), Automap::Enabled).unwrap_err();
        assert!(matches!(err, DiscoverError::NotMaildirPlus(_)));
    }

    #[test]
    fn discover_rejects_path_without_cur_subdir() {
        let td = tempfile::tempdir().unwrap();
        fs::create_dir_all(td.path().join("new")).unwrap();
        let err = ResolvedFolder::discover(td.path(), Automap::Enabled).unwrap_err();
        assert!(matches!(err, DiscoverError::NotAMaildir(_)));
    }

    #[test]
    fn discover_rejects_nonexistent_path() {
        let err = ResolvedFolder::discover(
            Path::new("/definitely/not/a/maildir/at/all"),
            Automap::Enabled,
        )
        .unwrap_err();
        assert!(matches!(err, DiscoverError::NotFound(_)));
    }

    #[test]
    fn discover_returns_parents_before_children() {
        let td = tempfile::tempdir().unwrap();
        make_maildir(
            td.path(),
            &[".Archive.2025", ".Archive", ".Lists.maildir-dev"],
        );
        let folders = ResolvedFolder::discover(td.path(), Automap::Enabled).unwrap();
        let index = |name: &str| folders.iter().position(|f| f.name == name).unwrap();
        assert!(index("Archive") < index("Archive.2025"));
        assert!(index("Lists") < index("Lists.maildir-dev"));
        assert_eq!(index("INBOX"), 0);
    }

    #[test]
    fn ancestor_chain_walks_dot_segments() {
        let chain = |name| MaildirName(name).ancestors_and_self().collect::<Vec<_>>();
        assert_eq!(chain("A"), ["A"]);
        assert_eq!(chain("A.B.C"), ["A", "A.B", "A.B.C"]);
    }

    #[test]
    fn leaf_and_split_handle_root_and_nested() {
        assert_eq!(MaildirName("INBOX").leaf(), "INBOX");
        assert_eq!(MaildirName("Archive.2025").leaf(), "2025");
        assert_eq!(MaildirName("INBOX").parent(), None);
        assert_eq!(MaildirName("Archive.2025").parent(), Some("Archive"));
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
                presence: Presence::OnDisk,
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
        let kept = filters.apply(folders);
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
        let kept = filters.apply(folders);
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
        let kept = filters.apply(folders);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name, "Sent");
    }

    #[test]
    fn discover_with_automap_off_clears_roles_except_inbox() {
        let td = tempfile::tempdir().unwrap();
        make_maildir(td.path(), &[".Sent", ".Drafts"]);
        let folders = ResolvedFolder::discover(td.path(), Automap::Disabled).unwrap();
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
        let folders = ResolvedFolder::discover(td.path(), Automap::Enabled).unwrap();
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
        FolderPath::sort_children_first_by(&mut names, DELIMITER, String::as_str);
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
        let folders = ResolvedFolder::discover(td.path(), Automap::Enabled).unwrap();
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
        assert!(parent.is_ephemeral());
        assert_eq!(parent.leaf, "日本語");
    }

    // Names that are not valid modified UTF-7 are kept byte for byte.
    #[test]
    fn decode_keeps_literal_ampersands_and_utf8() {
        for name in ["R&D", "AT&T", "Tom & Jerry-2", "Été", "Sales&", "A&-B&C"] {
            assert_eq!(MaildirName(name).decoded(), name);
        }
        assert_eq!(MaildirName("R&D.&AOk-t&AOk-").decoded(), "R&D.été");
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
            presence: Presence::OnDisk,
        });
        let filters = FolderFilters {
            include: Vec::new(),
            exclude: Vec::new(),
            explicit: vec!["été".to_owned()],
        };
        let kept = filters.apply(folders);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name, "&AOk-t&AOk-");
    }
}
