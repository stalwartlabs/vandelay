/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use mail_parser::mailbox::maildir::Flag;
use rusqlite::params;

use crate::archive::rows::EmailRecord;
use crate::archive::{LocalId, SourceId, Store};
use crate::console::Grouped;
use crate::import::imap::DeletedMessages;
use crate::logging::Logger;
use crate::sync::TypeCounts;
use crate::sync::dates::{MissingDate, Rfc3339};
use crate::sync::emailmeta::EmailMeta;

use super::keywords::{MaildirFilename, Translation};

const PROGRESS_TICK: u64 = 1000;
const EMPTY_KEYWORDS: &str = "[]";

#[derive(Debug, Clone)]
pub(super) struct DiskEntry {
    pub unique_id: String,
    pub filename: String,
    pub path: PathBuf,
    pub flags: Vec<Flag>,
    pub mtime_unix: u64,
}

#[derive(Debug, Clone, Default)]
pub(super) struct DiskListing {
    pub entries: Vec<DiskEntry>,
    pub io_failures: u64,
}

#[derive(Debug, Clone, Default)]
pub(super) struct FolderDiff {
    pub new: Vec<DiskEntry>,
    pub present: Vec<(DiskEntry, LocalId)>,
    pub vanished: Vec<(String, LocalId)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PresentOutcome {
    Unchanged,
    KeywordsUpdated,
    DeleteRequested,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum InsertError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("db: {0}")]
    Db(#[from] rusqlite::Error),
}

#[derive(Default)]
pub(super) struct StoredKeywords(HashMap<LocalId, String>);

#[derive(Clone, Copy)]
pub(super) struct FolderWriter<'a> {
    pub store: Store<'a>,
    pub source: SourceId,
    pub folder: &'a str,
    pub mailbox: LocalId,
    pub deleted: DeletedMessages,
}

impl DiskListing {
    pub fn read(folder_path: &Path) -> io::Result<DiskListing> {
        let mut out = DiskListing::default();
        let mut seen: HashSet<String> = HashSet::new();
        for sub in ["cur", "new"] {
            let dir = folder_path.join(sub);
            if !dir.is_dir() {
                continue;
            }
            for entry in fs::read_dir(&dir)? {
                let Ok(entry) = entry else {
                    out.io_failures += 1;
                    continue;
                };
                let Ok(file_type) = entry.file_type() else {
                    out.io_failures += 1;
                    continue;
                };
                if !file_type.is_file() {
                    continue;
                }
                let path = entry.path();
                let Some(raw_name) = path.file_name() else {
                    continue;
                };
                let filename = raw_name.to_string_lossy().into_owned();
                if filename.starts_with('.') {
                    continue;
                }
                let name = MaildirFilename(&filename);
                let unique_id = name.unique_id();
                if unique_id.is_empty() || !seen.insert(unique_id.to_owned()) {
                    continue;
                }
                let mtime_unix = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs());
                out.entries.push(DiskEntry {
                    unique_id: unique_id.to_owned(),
                    flags: name.flags(),
                    filename,
                    path,
                    mtime_unix,
                });
            }
        }
        Ok(out)
    }
}

impl DiskEntry {
    pub fn received_at(&self, date_header: Option<&str>) -> String {
        if let Some(secs) = MaildirFilename(&self.filename).delivery_time() {
            return Rfc3339::from_unix(secs);
        }
        if let Some(date) = date_header {
            return date.to_owned();
        }
        if self.mtime_unix > 0 {
            return Rfc3339::from_unix(self.mtime_unix);
        }
        MissingDate::Now.resolve()
    }
}

impl FolderDiff {
    pub fn compute(disk: Vec<DiskEntry>, local: &HashMap<String, LocalId>) -> FolderDiff {
        let on_disk: HashSet<&str> = disk.iter().map(|e| e.unique_id.as_str()).collect();
        let mut vanished: Vec<(String, LocalId)> = local
            .iter()
            .filter(|(uid, _)| !on_disk.contains(uid.as_str()))
            .map(|(uid, &id)| (uid.clone(), id))
            .collect();
        vanished.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let mut diff = FolderDiff {
            vanished,
            ..FolderDiff::default()
        };
        for entry in disk {
            match local.get(&entry.unique_id) {
                None => diff.new.push(entry),
                Some(&id) => diff.present.push((entry, id)),
            }
        }
        diff
    }
}

impl StoredKeywords {
    pub fn load(
        store: Store<'_>,
        source: SourceId,
        folder: &str,
    ) -> rusqlite::Result<StoredKeywords> {
        let mut stmt = store.connection().prepare_cached(
            "SELECT e.id, e.keywords FROM emails e
             JOIN sync_id_maildir m ON m.local_id = e.id AND m.type_name = 'email'
             WHERE m.source_id = ?1 AND m.folder = ?2",
        )?;
        let rows = stmt.query_map(params![source, folder], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        rows.collect::<rusqlite::Result<_>>().map(StoredKeywords)
    }

    pub fn get(&self, id: LocalId) -> &str {
        self.0.get(&id).map_or(EMPTY_KEYWORDS, String::as_str)
    }
}

impl FolderWriter<'_> {
    pub fn insert_new(&self, entry: &DiskEntry) -> Result<Option<LocalId>, InsertError> {
        let bytes = fs::read(&entry.path)?;
        let translation = Translation::from_flags(&entry.flags, self.deleted);
        if translation.has_trashed_flag && self.deleted == DeletedMessages::Skip {
            return Ok(None);
        }
        let EmailMeta {
            index,
            date: date_header,
        } = EmailMeta::from_blob(&bytes);
        let received_at = entry.received_at(date_header.as_deref());
        let blob = self.store.blobs().intern(&bytes)?;
        let message_match = index.to_json();
        let local = self.store.emails().insert(&EmailRecord {
            blob,
            received_at: &received_at,
            mailboxes: &[self.mailbox],
            keywords: &translation.keywords.to_json(),
            message_match: Some(&message_match),
        })?;
        self.store
            .maildir_ids(self.source)
            .insert_item(self.folder, &entry.unique_id, local)?;
        Ok(Some(local))
    }

    pub fn refresh_flags(
        &self,
        local: LocalId,
        entry: &DiskEntry,
        stored: &str,
    ) -> rusqlite::Result<PresentOutcome> {
        let translation = Translation::from_flags(&entry.flags, self.deleted);
        if translation.has_trashed_flag && self.deleted == DeletedMessages::Skip {
            return Ok(PresentOutcome::DeleteRequested);
        }
        let expected = translation.keywords.to_json();
        if stored == expected {
            return Ok(PresentOutcome::Unchanged);
        }
        self.store.emails().update_keywords(local, &expected)?;
        Ok(PresentOutcome::KeywordsUpdated)
    }

    pub fn delete(&self, unique_id: &str, local: LocalId) -> rusqlite::Result<()> {
        self.store.emails().delete(local)?;
        self.store
            .maildir_ids(self.source)
            .delete_item(self.folder, unique_id)
    }

    pub fn apply(
        &self,
        diff: &FolderDiff,
        stored: &StoredKeywords,
        counts: &mut TypeCounts,
        logger: Logger,
    ) {
        let folder = self.folder;
        let total_new = diff.new.len();
        let mut inserted: u64 = 0;
        for entry in &diff.new {
            match self.insert_new(entry) {
                Ok(Some(_)) => {
                    counts.created += 1;
                    counts.fetched += 1;
                    inserted += 1;
                    if inserted.is_multiple_of(PROGRESS_TICK) {
                        logger.progress(format_args!(
                            "Folder \"{folder}\": {} / {} inserted",
                            Grouped(inserted),
                            Grouped(total_new as u64)
                        ));
                    }
                }
                Ok(None) => counts.skipped += 1,
                Err(e) => {
                    logger.warn(format_args!(
                        "cannot import \"{folder}/{}\": {e}",
                        entry.filename
                    ));
                    counts.failed += 1;
                }
            }
        }
        for (entry, local) in &diff.present {
            match self.refresh_flags(*local, entry, stored.get(*local)) {
                Ok(PresentOutcome::Unchanged) => counts.skipped += 1,
                Ok(PresentOutcome::KeywordsUpdated) => counts.fetched += 1,
                Ok(PresentOutcome::DeleteRequested) => {
                    match self.delete(&entry.unique_id, *local) {
                        Ok(()) => counts.deleted += 1,
                        Err(e) => {
                            logger.warn(format_args!(
                                "cannot delete trashed message \"{folder}/{}\": {e}",
                                entry.filename
                            ));
                            counts.failed += 1;
                        }
                    }
                }
                Err(e) => {
                    logger.warn(format_args!(
                        "cannot update the flags of \"{folder}/{}\": {e}",
                        entry.filename
                    ));
                    counts.failed += 1;
                }
            }
        }
        for (unique_id, local) in &diff.vanished {
            match self.delete(unique_id, *local) {
                Ok(()) => counts.deleted += 1,
                Err(e) => {
                    logger.warn(format_args!(
                        "cannot delete vanished message \"{folder}/{unique_id}\": {e}"
                    ));
                    counts.failed += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;

    use super::*;
    use crate::archive::Archive;
    use crate::archive::rows::{MailboxRecord, Subscribed};
    use crate::archive::sources::{SourceKey, SourceKind};

    struct Fixture {
        archive: Archive,
        source: SourceId,
        mailbox: LocalId,
    }

    impl Fixture {
        fn new() -> Fixture {
            let archive = Archive::open_in_memory().unwrap();
            let store = archive.store();
            let source = store
                .sources()
                .upsert(
                    &SourceKey {
                        kind: SourceKind::Maildir,
                        url: "file:///tmp/Maildir",
                        account_id: "/tmp/Maildir",
                    },
                    Some("Maildir"),
                    "",
                )
                .unwrap();
            let mailbox = store
                .mailboxes()
                .insert(&MailboxRecord {
                    name: "INBOX",
                    parent: None,
                    role: Some("inbox"),
                    sort_order: 0,
                    subscribed: Subscribed::Yes,
                })
                .unwrap();
            store
                .maildir_ids(source)
                .insert_folder("INBOX", mailbox)
                .unwrap();
            Fixture {
                archive,
                source,
                mailbox,
            }
        }

        fn writer(&self, deleted: DeletedMessages) -> FolderWriter<'_> {
            FolderWriter {
                store: self.archive.store(),
                source: self.source,
                folder: "INBOX",
                mailbox: self.mailbox,
                deleted,
            }
        }

        fn count(&self, table: &str) -> i64 {
            self.archive
                .connection()
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap()
        }

        fn keywords(&self, local: LocalId) -> String {
            self.archive
                .connection()
                .query_row("SELECT keywords FROM emails WHERE id = ?1", [local], |r| {
                    r.get(0)
                })
                .unwrap()
        }

        fn stored(&self) -> StoredKeywords {
            StoredKeywords::load(self.archive.store(), self.source, "INBOX").unwrap()
        }
    }

    fn write_maildir_message(
        folder_path: &Path,
        sub: &str,
        filename: &str,
        body: &[u8],
    ) -> PathBuf {
        let dir = folder_path.join(sub);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(filename);
        fs::write(&p, body).unwrap();
        p
    }

    fn ensure_folder_skel(folder_path: &Path) {
        for sub in ["cur", "new", "tmp"] {
            fs::create_dir_all(folder_path.join(sub)).unwrap();
        }
    }

    fn only_entry(path: &Path) -> DiskEntry {
        DiskListing::read(path).unwrap().entries.remove(0)
    }

    fn received_at(filename: &str, mtime_unix: u64, date_header: Option<&str>) -> String {
        DiskEntry {
            unique_id: String::new(),
            filename: filename.to_owned(),
            path: PathBuf::new(),
            flags: Vec::new(),
            mtime_unix,
        }
        .received_at(date_header)
    }

    #[test]
    fn list_folder_returns_cur_and_new_skipping_tmp() {
        let td = tempfile::tempdir().unwrap();
        ensure_folder_skel(td.path());
        write_maildir_message(td.path(), "cur", "1.M0.host:2,S", b"a");
        write_maildir_message(td.path(), "new", "2.M0.host", b"b");
        write_maildir_message(td.path(), "tmp", "3.M0.host", b"c");
        let listing = DiskListing::read(td.path()).unwrap();
        assert_eq!(listing.entries.len(), 2);
        assert!(listing.entries.iter().any(|e| e.unique_id == "1.M0.host"));
        assert!(listing.entries.iter().any(|e| e.unique_id == "2.M0.host"));
    }

    #[test]
    fn list_folder_skips_dotfiles() {
        let td = tempfile::tempdir().unwrap();
        ensure_folder_skel(td.path());
        write_maildir_message(td.path(), "cur", ".uidvalidity", b"meta");
        write_maildir_message(td.path(), "cur", ".dovecot-uidlist", b"meta");
        write_maildir_message(td.path(), "cur", "1.M0.host:2,S", b"a");
        let listing = DiskListing::read(td.path()).unwrap();
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].unique_id, "1.M0.host");
    }

    #[test]
    fn list_folder_parses_dovecot_extension_filename() {
        let td = tempfile::tempdir().unwrap();
        ensure_folder_skel(td.path());
        write_maildir_message(
            td.path(),
            "cur",
            "1739471123.M001P01234V0000I0000abcd.host,S=42,W=44:2,RS",
            b"a",
        );
        let listing = DiskListing::read(td.path()).unwrap();
        assert_eq!(listing.entries.len(), 1);
        let e = &listing.entries[0];
        assert_eq!(
            e.unique_id,
            "1739471123.M001P01234V0000I0000abcd.host,S=42,W=44"
        );
        assert!(e.flags.contains(&Flag::Replied));
        assert!(e.flags.contains(&Flag::Seen));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn list_folder_renders_non_utf8_filename_lossy() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let td = tempfile::tempdir().unwrap();
        ensure_folder_skel(td.path());
        let mut bytes = b"1.M0.host".to_vec();
        bytes.extend_from_slice(&[0xff, 0xfe]);
        bytes.extend_from_slice(b":2,S");
        let bad_name = OsStr::from_bytes(&bytes);
        let p = td.path().join("cur").join(bad_name);
        fs::write(&p, b"body").unwrap();
        let listing = DiskListing::read(td.path()).unwrap();
        assert_eq!(listing.entries.len(), 1);
        let e = &listing.entries[0];
        assert!(
            e.unique_id.contains('\u{FFFD}'),
            "lossy substitution: {:?}",
            e.unique_id
        );
        assert!(e.unique_id.starts_with("1.M0.host"));
        assert!(e.flags.contains(&Flag::Seen));
    }

    #[test]
    fn diff_partitions_new_present_vanished() {
        let entries = vec![
            DiskEntry {
                unique_id: "a".into(),
                filename: "a".into(),
                path: PathBuf::new(),
                flags: vec![],
                mtime_unix: 0,
            },
            DiskEntry {
                unique_id: "b".into(),
                filename: "b".into(),
                path: PathBuf::new(),
                flags: vec![],
                mtime_unix: 0,
            },
        ];
        let mut local = HashMap::new();
        local.insert("b".to_owned(), LocalId::from_raw(10));
        local.insert("c".to_owned(), LocalId::from_raw(11));
        let d = FolderDiff::compute(entries, &local);
        assert_eq!(d.new.len(), 1);
        assert_eq!(d.new[0].unique_id, "a");
        assert_eq!(d.present.len(), 1);
        assert_eq!(d.present[0].0.unique_id, "b");
        assert_eq!(d.present[0].1, LocalId::from_raw(10));
        assert_eq!(d.vanished, vec![("c".to_owned(), LocalId::from_raw(11))]);
    }

    #[test]
    fn insert_new_writes_blob_email_and_sync_row() {
        let fx = Fixture::new();
        let td = tempfile::tempdir().unwrap();
        ensure_folder_skel(td.path());
        let body = b"From: a@x\r\nSubject: hi\r\nMessage-ID: <m1@h>\r\n\r\nhi";
        write_maildir_message(td.path(), "cur", "uid1.M0.host:2,S", body);
        let entry = only_entry(td.path());
        let inserted = fx
            .writer(DeletedMessages::Skip)
            .insert_new(&entry)
            .unwrap()
            .unwrap();
        assert_eq!(fx.count("emails"), 1);
        let local = fx
            .archive
            .store()
            .maildir_ids(fx.source)
            .item_id("INBOX", "uid1.M0.host")
            .unwrap();
        assert_eq!(local, Some(inserted));
        assert!(fx.keywords(inserted).contains("$seen"));
    }

    #[test]
    fn insert_new_skips_trashed_without_include_deleted() {
        let fx = Fixture::new();
        let td = tempfile::tempdir().unwrap();
        ensure_folder_skel(td.path());
        write_maildir_message(td.path(), "cur", "uid.host:2,T", b"x");
        let entry = only_entry(td.path());
        let inserted = fx.writer(DeletedMessages::Skip).insert_new(&entry).unwrap();
        assert!(inserted.is_none());
        assert_eq!(fx.count("emails"), 0);
    }

    #[test]
    fn refresh_flags_updates_only_when_changed() {
        let fx = Fixture::new();
        let writer = fx.writer(DeletedMessages::Skip);
        let td = tempfile::tempdir().unwrap();
        ensure_folder_skel(td.path());
        write_maildir_message(td.path(), "cur", "u.host:2,", b"hi");
        let entry = only_entry(td.path());
        let local = writer.insert_new(&entry).unwrap().unwrap();
        let stored = fx.stored();
        assert_eq!(
            writer
                .refresh_flags(local, &entry, stored.get(local))
                .unwrap(),
            PresentOutcome::Unchanged
        );
        fs::rename(&entry.path, entry.path.with_file_name("u.host:2,S")).unwrap();
        let entry2 = only_entry(td.path());
        assert_eq!(
            writer
                .refresh_flags(local, &entry2, stored.get(local))
                .unwrap(),
            PresentOutcome::KeywordsUpdated
        );
        assert!(fx.keywords(local).contains("$seen"));
    }

    #[test]
    fn refresh_flags_returns_delete_when_trashed_added_and_include_deleted_off() {
        let fx = Fixture::new();
        let writer = fx.writer(DeletedMessages::Skip);
        let td = tempfile::tempdir().unwrap();
        ensure_folder_skel(td.path());
        write_maildir_message(td.path(), "cur", "u.host:2,", b"hi");
        let entry = only_entry(td.path());
        let local = writer.insert_new(&entry).unwrap().unwrap();
        fs::rename(&entry.path, entry.path.with_file_name("u.host:2,T")).unwrap();
        let entry2 = only_entry(td.path());
        assert_eq!(
            writer.refresh_flags(local, &entry2, "[]").unwrap(),
            PresentOutcome::DeleteRequested
        );
    }

    #[test]
    fn refresh_flags_keeps_trashed_with_include_deleted_on() {
        let fx = Fixture::new();
        let writer = fx.writer(DeletedMessages::Keep);
        let td = tempfile::tempdir().unwrap();
        ensure_folder_skel(td.path());
        write_maildir_message(td.path(), "cur", "u.host:2,T", b"hi");
        let entry = only_entry(td.path());
        let local = writer.insert_new(&entry).unwrap().unwrap();
        let stored = fx.stored();
        assert_eq!(
            writer
                .refresh_flags(local, &entry, stored.get(local))
                .unwrap(),
            PresentOutcome::Unchanged
        );
    }

    #[test]
    fn filename_delivery_time_wins_over_mtime() {
        let got = received_at(
            "1763065233.M12345P678.host,S=1234:2,S",
            99,
            Some("2025-05-12T10:00:00+02:00"),
        );
        assert_eq!(got, "2025-11-13T20:20:33Z");
    }

    #[test]
    fn date_header_used_when_the_filename_carries_no_timestamp() {
        let got = received_at(
            "M12345P678.host:2,S",
            1763065233,
            Some("2025-05-12T10:00:00+02:00"),
        );
        assert_eq!(got, "2025-05-12T10:00:00+02:00");
    }

    #[test]
    fn mtime_is_the_last_resort_before_now() {
        let got = received_at("M12345P678.host:2,S", 1763065233, None);
        assert_eq!(got, "2025-11-13T20:20:33Z");
    }

    #[test]
    fn implausible_filename_timestamps_are_rejected() {
        let delivery = |name: &str| MaildirFilename(name).delivery_time();
        assert_eq!(delivery("0.M1P1.host"), None);
        assert_eq!(delivery("M1P1.host"), None);
        assert_eq!(delivery("99999999999999.M1P1.host"), None);
        let far_future = (OffsetDateTime::now_utc().unix_timestamp() as u64) + 400_000;
        assert_eq!(delivery(&format!("{far_future}.M1P1.host")), None);
        assert_eq!(delivery("1763065233.M12345P678.host"), Some(1763065233));
    }
}
