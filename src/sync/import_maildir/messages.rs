/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use mail_parser::mailbox::maildir::Flag;
use rusqlite::{Connection, params};
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::db;
use crate::sync::TypeCounts;
use crate::sync::emailmeta::email_meta_from_blob;
use crate::sync::keys::index_to_json;

use super::keywords::{Translation, flags_from_filename, translate_flags, unique_id_from_filename};

#[derive(Debug, Clone)]
pub struct DiskEntry {
    pub unique_id: String,
    pub filename: String,
    pub path: PathBuf,
    pub flags: Vec<Flag>,
    pub mtime_unix: u64,
}

#[derive(Debug, Clone, Default)]
pub struct DiskListing {
    pub entries: Vec<DiskEntry>,
    pub io_failures: u64,
}

pub fn list_folder(folder_path: &Path) -> std::io::Result<DiskListing> {
    let mut out = DiskListing::default();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for sub in ["cur", "new"] {
        let dir = folder_path.join(sub);
        if !dir.is_dir() {
            continue;
        }
        let entries = std::fs::read_dir(&dir)?;
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    out.io_failures += 1;
                    continue;
                }
            };
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => {
                    out.io_failures += 1;
                    continue;
                }
            };
            if !file_type.is_file() {
                continue;
            }
            let Some(raw_name) = path.file_name() else {
                continue;
            };
            let filename: String = raw_name.to_string_lossy().into_owned();
            if filename.starts_with('.') {
                continue;
            }
            let unique_id = unique_id_from_filename(&filename).to_owned();
            if unique_id.is_empty() {
                continue;
            }
            if seen.contains_key(&unique_id) {
                continue;
            }
            let flags = flags_from_filename(&filename);
            let mtime_unix = match entry.metadata().and_then(|m| m.modified()).and_then(|t| {
                t.duration_since(UNIX_EPOCH).map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
                })
            }) {
                Ok(d) => d.as_secs(),
                Err(_) => 0,
            };
            seen.insert(unique_id.clone(), out.entries.len());
            out.entries.push(DiskEntry {
                unique_id,
                filename,
                path,
                flags,
                mtime_unix,
            });
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Default)]
pub struct FolderDiff {
    pub new: Vec<DiskEntry>,
    pub present: Vec<(DiskEntry, i64)>,
    pub vanished: Vec<(String, i64)>,
}

pub fn diff(disk: Vec<DiskEntry>, local: &HashMap<String, i64>) -> FolderDiff {
    let mut diff = FolderDiff::default();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for entry in disk {
        match local.get(&entry.unique_id) {
            None => {
                seen.insert(entry.unique_id.clone());
                diff.new.push(entry);
            }
            Some(&local_id) => {
                seen.insert(entry.unique_id.clone());
                diff.present.push((entry, local_id));
            }
        }
    }
    for (uid, local_id) in local {
        if !seen.contains(uid) {
            diff.vanished.push((uid.clone(), *local_id));
        }
    }
    diff.vanished.sort_by(|a, b| a.0.cmp(&b.0));
    diff
}

#[derive(Clone, Copy)]
pub struct InsertContext<'a> {
    pub source_id: i64,
    pub folder: &'a str,
    pub mailbox_local: i64,
    pub include_deleted: bool,
}

pub fn insert_new(
    tx: &rusqlite::Transaction<'_>,
    ctx: InsertContext<'_>,
    entry: &DiskEntry,
) -> Result<Option<i64>, InsertError> {
    let bytes = std::fs::read(&entry.path).map_err(InsertError::Io)?;
    let translation = translate_flags(&entry.flags, ctx.include_deleted);
    if translation.has_trashed_flag && !ctx.include_deleted {
        return Ok(None);
    }
    let (index, date_header) = email_meta_from_blob(&bytes);
    let received_at = pick_received_at(&entry.filename, entry.mtime_unix, date_header.as_deref());
    let blob_id = db::blobs::intern_blob(tx, &bytes)?;
    let message_match = index_to_json(&index);
    let mailbox_ids = Value::Array(vec![Value::from(ctx.mailbox_local)]);
    let keywords = keywords_json(&translation);
    tx.execute(
        "INSERT INTO emails (blob_id, received_at, mailbox_ids, keywords, message_match)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            blob_id,
            received_at,
            mailbox_ids.to_string(),
            keywords,
            message_match
        ],
    )?;
    let local_id = tx.last_insert_rowid();
    db::maildir_ids::insert_email(tx, ctx.source_id, ctx.folder, &entry.unique_id, local_id)?;
    Ok(Some(local_id))
}

pub fn delivery_time_from_filename(filename: &str) -> Option<u64> {
    let digits: String = filename.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || digits.len() > 12 {
        return None;
    }
    let secs: u64 = digits.parse().ok()?;
    if secs == 0 {
        return None;
    }
    let now = OffsetDateTime::now_utc().unix_timestamp().max(0) as u64;
    if secs > now.saturating_add(86_400) {
        return None;
    }
    Some(secs)
}

pub fn pick_received_at(filename: &str, mtime_unix: u64, date_header: Option<&str>) -> String {
    if let Some(secs) = delivery_time_from_filename(filename) {
        return format_unix_rfc3339(secs);
    }
    if let Some(d) = date_header {
        return d.to_owned();
    }
    if mtime_unix > 0 {
        return format_unix_rfc3339(mtime_unix);
    }
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentOutcome {
    Unchanged,

    KeywordsUpdated,

    DeleteRequested,
}

pub fn refresh_flags(
    tx: &rusqlite::Transaction<'_>,
    local_id: i64,
    entry: &DiskEntry,
    stored_keywords: &str,
    include_deleted: bool,
) -> Result<PresentOutcome, rusqlite::Error> {
    let translation = translate_flags(&entry.flags, include_deleted);
    if translation.has_trashed_flag && !include_deleted {
        return Ok(PresentOutcome::DeleteRequested);
    }
    let expected = keywords_json(&translation);
    if stored_keywords == expected {
        return Ok(PresentOutcome::Unchanged);
    }
    tx.execute(
        "UPDATE emails SET keywords = ?1 WHERE id = ?2",
        params![expected, local_id],
    )?;
    Ok(PresentOutcome::KeywordsUpdated)
}

pub fn load_present_keywords(
    conn: &Connection,
    source_id: i64,
    folder: &str,
) -> Result<HashMap<i64, String>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT e.id, e.keywords FROM emails e
         JOIN sync_id_maildir m ON m.local_id = e.id AND m.type_name = 'email'
         WHERE m.source_id = ?1 AND m.folder = ?2",
    )?;
    let rows = stmt.query_map(params![source_id, folder], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = HashMap::new();
    for r in rows {
        let (id, kw) = r?;
        out.insert(id, kw);
    }
    Ok(out)
}

pub fn delete_vanished(
    tx: &rusqlite::Transaction<'_>,
    source_id: i64,
    folder: &str,
    unique_id: &str,
    local_id: i64,
) -> Result<(), rusqlite::Error> {
    tx.execute("DELETE FROM emails WHERE id = ?1", params![local_id])?;
    db::maildir_ids::delete_email(tx, source_id, folder, unique_id)?;
    Ok(())
}

const PROGRESS_TICK: u64 = 1000;

pub fn apply_folder(
    conn: &mut Connection,
    ctx: InsertContext<'_>,
    diff: FolderDiff,
    counts: &mut TypeCounts,
    logger: crate::logging::Logger,
) -> Result<(), crate::error::Error> {
    let stored_keywords =
        load_present_keywords(conn, ctx.source_id, ctx.folder).unwrap_or_default();
    let tx = conn.transaction()?;
    let total_new = diff.new.len() as u64;
    let mut inserted: u64 = 0;
    for entry in &diff.new {
        match insert_new(&tx, ctx, entry) {
            Ok(Some(_)) => {
                counts.created += 1;
                counts.fetched += 1;
                inserted += 1;
                if inserted.is_multiple_of(PROGRESS_TICK)
                    && logger.enabled(crate::logging::LEVEL_PROGRESS)
                {
                    eprintln!(
                        "folder {folder:?}: inserted {inserted}/{total_new}",
                        folder = ctx.folder
                    );
                }
            }
            Ok(None) => {
                counts.skipped += 1;
            }
            Err(e) => {
                logger.warn(&format!(
                    "maildir {folder:?}/{name}: {e}",
                    folder = ctx.folder,
                    name = entry.filename
                ));
                counts.failed += 1;
            }
        }
    }
    for (entry, local_id) in &diff.present {
        let stored = stored_keywords
            .get(local_id)
            .map(String::as_str)
            .unwrap_or("[]");
        match refresh_flags(&tx, *local_id, entry, stored, ctx.include_deleted) {
            Ok(PresentOutcome::Unchanged) => counts.skipped += 1,
            Ok(PresentOutcome::KeywordsUpdated) => counts.fetched += 1,
            Ok(PresentOutcome::DeleteRequested) => {
                match delete_vanished(&tx, ctx.source_id, ctx.folder, &entry.unique_id, *local_id) {
                    Ok(()) => counts.deleted += 1,
                    Err(e) => {
                        logger.warn(&format!(
                            "maildir {folder:?}/{name}: T-flag delete failed: {e}",
                            folder = ctx.folder,
                            name = entry.filename
                        ));
                        counts.failed += 1;
                    }
                }
            }
            Err(e) => {
                logger.warn(&format!(
                    "maildir {folder:?}/{name}: flag update failed: {e}",
                    folder = ctx.folder,
                    name = entry.filename
                ));
                counts.failed += 1;
            }
        }
    }
    for (unique_id, local_id) in &diff.vanished {
        if let Err(e) = delete_vanished(&tx, ctx.source_id, ctx.folder, unique_id, *local_id) {
            logger.warn(&format!(
                "maildir {folder:?}/{unique_id}: delete failed: {e}",
                folder = ctx.folder
            ));
            counts.failed += 1;
        } else {
            counts.deleted += 1;
        }
    }
    tx.commit()?;
    Ok(())
}

fn keywords_json(translation: &Translation) -> String {
    Value::Array(
        translation
            .keywords
            .iter()
            .map(|k| Value::String(k.clone()))
            .collect(),
    )
    .to_string()
}

fn format_unix_rfc3339(secs: u64) -> String {
    let when = UNIX_EPOCH + Duration::from_secs(secs);
    OffsetDateTime::from(when)
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

#[derive(Debug, thiserror::Error)]
pub enum InsertError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("db: {0}")]
    Db(#[from] rusqlite::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init;
    use crate::db::sources::{SourceKey, upsert_source};
    use std::fs;

    fn fresh_archive() -> (Connection, i64) {
        let c = Connection::open_in_memory().unwrap();
        init::apply_schema(&c).unwrap();
        let sid = upsert_source(
            &c,
            &SourceKey {
                kind: "maildir".to_owned(),
                session_url: "file:///tmp/Maildir".to_owned(),
                account_id: "/tmp/Maildir".to_owned(),
            },
            Some("Maildir"),
            "",
        )
        .unwrap();
        (c, sid)
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

    #[test]
    fn list_folder_returns_cur_and_new_skipping_tmp() {
        let td = tempfile::tempdir().unwrap();
        ensure_folder_skel(td.path());
        write_maildir_message(td.path(), "cur", "1.M0.host:2,S", b"a");
        write_maildir_message(td.path(), "new", "2.M0.host", b"b");
        write_maildir_message(td.path(), "tmp", "3.M0.host", b"c");
        let listing = list_folder(td.path()).unwrap();
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
        let listing = list_folder(td.path()).unwrap();
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
        let listing = list_folder(td.path()).unwrap();
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
        let listing = list_folder(td.path()).unwrap();
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
        local.insert("b".to_owned(), 10);
        local.insert("c".to_owned(), 11);
        let d = diff(entries, &local);
        assert_eq!(d.new.len(), 1);
        assert_eq!(d.new[0].unique_id, "a");
        assert_eq!(d.present.len(), 1);
        assert_eq!(d.present[0].0.unique_id, "b");
        assert_eq!(d.present[0].1, 10);
        assert_eq!(d.vanished, vec![("c".to_owned(), 11)]);
    }

    #[test]
    fn insert_new_writes_blob_email_and_sync_row() {
        let (mut c, sid) = fresh_archive();
        c.execute(
            "INSERT INTO mailboxes (name, parent_id, role, sort_order, is_subscribed)
             VALUES ('INBOX', NULL, 'inbox', 0, 1)",
            [],
        )
        .unwrap();
        let mailbox_local: i64 = c.last_insert_rowid();
        db::maildir_ids::insert_mailbox(&c, sid, "INBOX", mailbox_local).unwrap();

        let td = tempfile::tempdir().unwrap();
        ensure_folder_skel(td.path());
        let body = b"From: a@x\r\nSubject: hi\r\nMessage-ID: <m1@h>\r\n\r\nhi";
        write_maildir_message(td.path(), "cur", "uid1.M0.host:2,S", body);
        let listing = list_folder(td.path()).unwrap();
        let entry = listing.entries.into_iter().next().unwrap();

        let tx = c.transaction().unwrap();
        let inserted = insert_new(
            &tx,
            InsertContext {
                source_id: sid,
                folder: "INBOX",
                mailbox_local,
                include_deleted: false,
            },
            &entry,
        )
        .unwrap();
        tx.commit().unwrap();
        assert!(inserted.is_some());

        let email_count: i64 = c
            .query_row("SELECT count(*) FROM emails", [], |r| r.get(0))
            .unwrap();
        assert_eq!(email_count, 1);
        let local = db::maildir_ids::local_for_email(&c, sid, "INBOX", "uid1.M0.host")
            .unwrap()
            .unwrap();
        assert_eq!(local, inserted.unwrap());
        let kws: String = c
            .query_row(
                "SELECT keywords FROM emails WHERE id = ?1",
                params![local],
                |r| r.get(0),
            )
            .unwrap();
        assert!(kws.contains("$seen"));
    }

    #[test]
    fn insert_new_skips_trashed_without_include_deleted() {
        let (mut c, sid) = fresh_archive();
        c.execute(
            "INSERT INTO mailboxes (name, parent_id, role, sort_order, is_subscribed)
             VALUES ('INBOX', NULL, 'inbox', 0, 1)",
            [],
        )
        .unwrap();
        let mailbox_local: i64 = c.last_insert_rowid();
        db::maildir_ids::insert_mailbox(&c, sid, "INBOX", mailbox_local).unwrap();

        let td = tempfile::tempdir().unwrap();
        ensure_folder_skel(td.path());
        write_maildir_message(td.path(), "cur", "uid.host:2,T", b"x");
        let listing = list_folder(td.path()).unwrap();
        let entry = listing.entries.into_iter().next().unwrap();
        let tx = c.transaction().unwrap();
        let inserted = insert_new(
            &tx,
            InsertContext {
                source_id: sid,
                folder: "INBOX",
                mailbox_local,
                include_deleted: false,
            },
            &entry,
        )
        .unwrap();
        tx.commit().unwrap();
        assert!(inserted.is_none());
        let email_count: i64 = c
            .query_row("SELECT count(*) FROM emails", [], |r| r.get(0))
            .unwrap();
        assert_eq!(email_count, 0);
    }

    #[test]
    fn refresh_flags_updates_only_when_changed() {
        let (mut c, sid) = fresh_archive();
        c.execute(
            "INSERT INTO mailboxes (name, parent_id, role, sort_order, is_subscribed)
             VALUES ('INBOX', NULL, 'inbox', 0, 1)",
            [],
        )
        .unwrap();
        let mailbox_local = c.last_insert_rowid();
        db::maildir_ids::insert_mailbox(&c, sid, "INBOX", mailbox_local).unwrap();
        let td = tempfile::tempdir().unwrap();
        ensure_folder_skel(td.path());
        write_maildir_message(td.path(), "cur", "u.host:2,", b"hi");
        let entry = list_folder(td.path()).unwrap().entries.remove(0);
        let tx = c.transaction().unwrap();
        let local = insert_new(
            &tx,
            InsertContext {
                source_id: sid,
                folder: "INBOX",
                mailbox_local,
                include_deleted: false,
            },
            &entry,
        )
        .unwrap()
        .unwrap();
        tx.commit().unwrap();

        let stored = load_present_keywords(&c, sid, "INBOX").unwrap();
        let stored_for_local = stored
            .get(&local)
            .cloned()
            .unwrap_or_else(|| "[]".to_owned());

        let tx = c.transaction().unwrap();
        assert_eq!(
            refresh_flags(&tx, local, &entry, &stored_for_local, false).unwrap(),
            PresentOutcome::Unchanged
        );
        tx.commit().unwrap();

        fs::rename(&entry.path, entry.path.with_file_name("u.host:2,S")).unwrap();
        let entry2 = list_folder(td.path()).unwrap().entries.remove(0);
        let tx = c.transaction().unwrap();
        assert_eq!(
            refresh_flags(&tx, local, &entry2, &stored_for_local, false).unwrap(),
            PresentOutcome::KeywordsUpdated
        );
        tx.commit().unwrap();
        let kws: String = c
            .query_row(
                "SELECT keywords FROM emails WHERE id = ?1",
                params![local],
                |r| r.get(0),
            )
            .unwrap();
        assert!(kws.contains("$seen"));
    }

    #[test]
    fn refresh_flags_returns_delete_when_trashed_added_and_include_deleted_off() {
        let (mut c, sid) = fresh_archive();
        c.execute(
            "INSERT INTO mailboxes (name, parent_id, role, sort_order, is_subscribed)
             VALUES ('INBOX', NULL, 'inbox', 0, 1)",
            [],
        )
        .unwrap();
        let mailbox_local = c.last_insert_rowid();
        db::maildir_ids::insert_mailbox(&c, sid, "INBOX", mailbox_local).unwrap();
        let td = tempfile::tempdir().unwrap();
        ensure_folder_skel(td.path());
        write_maildir_message(td.path(), "cur", "u.host:2,", b"hi");
        let entry = list_folder(td.path()).unwrap().entries.remove(0);
        let tx = c.transaction().unwrap();
        let local = insert_new(
            &tx,
            InsertContext {
                source_id: sid,
                folder: "INBOX",
                mailbox_local,
                include_deleted: false,
            },
            &entry,
        )
        .unwrap()
        .unwrap();
        tx.commit().unwrap();

        fs::rename(&entry.path, entry.path.with_file_name("u.host:2,T")).unwrap();
        let entry2 = list_folder(td.path()).unwrap().entries.remove(0);
        let tx = c.transaction().unwrap();
        assert_eq!(
            refresh_flags(&tx, local, &entry2, "[]", false).unwrap(),
            PresentOutcome::DeleteRequested
        );
        tx.commit().unwrap();
    }

    #[test]
    fn refresh_flags_keeps_trashed_with_include_deleted_on() {
        let (mut c, sid) = fresh_archive();
        c.execute(
            "INSERT INTO mailboxes (name, parent_id, role, sort_order, is_subscribed)
             VALUES ('INBOX', NULL, 'inbox', 0, 1)",
            [],
        )
        .unwrap();
        let mailbox_local = c.last_insert_rowid();
        db::maildir_ids::insert_mailbox(&c, sid, "INBOX", mailbox_local).unwrap();
        let td = tempfile::tempdir().unwrap();
        ensure_folder_skel(td.path());
        write_maildir_message(td.path(), "cur", "u.host:2,T", b"hi");
        let entry = list_folder(td.path()).unwrap().entries.remove(0);
        let tx = c.transaction().unwrap();
        let local = insert_new(
            &tx,
            InsertContext {
                source_id: sid,
                folder: "INBOX",
                mailbox_local,
                include_deleted: true,
            },
            &entry,
        )
        .unwrap()
        .unwrap();
        tx.commit().unwrap();

        let stored = load_present_keywords(&c, sid, "INBOX").unwrap();
        let stored_for_local = stored.get(&local).cloned().unwrap();
        let tx = c.transaction().unwrap();
        assert_eq!(
            refresh_flags(&tx, local, &entry, &stored_for_local, true).unwrap(),
            PresentOutcome::Unchanged
        );
        tx.commit().unwrap();
    }

    #[test]
    fn filename_delivery_time_wins_over_mtime() {
        let got = pick_received_at(
            "1763065233.M12345P678.host,S=1234:2,S",
            99,
            Some("2025-05-12T10:00:00+02:00"),
        );
        assert_eq!(got, "2025-11-13T20:20:33Z");
    }

    #[test]
    fn date_header_used_when_the_filename_carries_no_timestamp() {
        let got = pick_received_at(
            "M12345P678.host:2,S",
            1763065233,
            Some("2025-05-12T10:00:00+02:00"),
        );
        assert_eq!(got, "2025-05-12T10:00:00+02:00");
    }

    #[test]
    fn mtime_is_the_last_resort_before_now() {
        let got = pick_received_at("M12345P678.host:2,S", 1763065233, None);
        assert_eq!(got, "2025-11-13T20:20:33Z");
    }

    #[test]
    fn implausible_filename_timestamps_are_rejected() {
        assert_eq!(delivery_time_from_filename("0.M1P1.host"), None);
        assert_eq!(delivery_time_from_filename("M1P1.host"), None);
        assert_eq!(
            delivery_time_from_filename("99999999999999.M1P1.host"),
            None
        );
        let far_future = (OffsetDateTime::now_utc().unix_timestamp() as u64) + 400_000;
        assert_eq!(
            delivery_time_from_filename(&format!("{far_future}.M1P1.host")),
            None
        );
        assert_eq!(
            delivery_time_from_filename("1763065233.M12345P678.host"),
            Some(1763065233)
        );
    }
}
