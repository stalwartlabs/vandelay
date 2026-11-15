/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

#![allow(dead_code, clippy::needless_borrows_for_generic_args)]

mod common;
mod integration;
mod seeder;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use integration::stalwart::shared as shared_stalwart;
use rusqlite::Connection;

fn base_url() -> &'static str {
    shared_stalwart().base_url()
}
use vandelay::archive::sources::SourceChange;
use vandelay::imap::automap::Automap;
use vandelay::import::imap::DeletedMessages;
use vandelay::import::jmap::JmapImport;
use vandelay::import::maildir::MaildirImport;
use vandelay::jmap::account::AccountSelector;
use vandelay::jmap::connect::ConnectConfig;
use vandelay::logging::{Level, Logger};
use vandelay::net::auth::Auth;
use vandelay::net::http::TlsPolicy;
use vandelay::sync::{RunMode, RunOptions};

fn base_cfg(root: &Path) -> MaildirImport {
    MaildirImport {
        maildir: root.to_path_buf(),
        include: Vec::new(),
        exclude: Vec::new(),
        folder: Vec::new(),
        automap: Automap::Enabled,
        deleted: DeletedMessages::Skip,
        source_change: SourceChange::Reject,
    }
}

fn ensure_maildir(root: &Path) {
    for sub in ["cur", "new", "tmp"] {
        fs::create_dir_all(root.join(sub)).unwrap();
    }
}

fn ensure_subfolder(root: &Path, dotted: &str) -> PathBuf {
    let sub = root.join(dotted);
    for s in ["cur", "new", "tmp"] {
        fs::create_dir_all(sub.join(s)).unwrap();
    }
    sub
}

fn write(folder: &Path, sub: &str, filename: &str, body: &[u8]) -> PathBuf {
    let dir = folder.join(sub);
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join(filename);
    fs::write(&p, body).unwrap();
    p
}

fn msg(id: &str, subject: &str, body: &str) -> Vec<u8> {
    format!(
        "From: a@example.com\r\nTo: b@example.com\r\n\
         Subject: {subject}\r\nMessage-ID: <{id}@example.com>\r\n\
         Date: Mon, 12 May 2025 10:00:00 +0000\r\n\r\n{body}\r\n"
    )
    .into_bytes()
}

fn build_fixture(root: &Path) {
    ensure_maildir(root);
    write(
        root,
        "cur",
        "100.M001.host:2,S",
        &msg("conv-i1", "i1", "body 1"),
    );
    write(
        root,
        "cur",
        "101.M002.host:2,",
        &msg("conv-i2", "i2", "body 2"),
    );
    write(
        root,
        "new",
        "102.M003.host",
        &msg("conv-i3", "i3", "body 3"),
    );
    let sent = ensure_subfolder(root, ".Sent");
    write(
        &sent,
        "cur",
        "200.M001.host:2,S",
        &msg("conv-s1", "sent-1", "sent body"),
    );
    ensure_subfolder(root, ".Archive");
    let arch25 = ensure_subfolder(root, ".Archive.2025");
    write(
        &arch25,
        "cur",
        "300.M001.host:2,S",
        &msg("conv-a1", "arch-1", "arch body 1"),
    );
    write(
        &arch25,
        "cur",
        "301.M002.host:2,SF",
        &msg("conv-a2", "arch-2", "arch body 2"),
    );
}

fn snapshot_hashes_by_folder(conn: &Connection) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut stmt = conn
        .prepare(
            "SELECT m.folder, b.hash, e.keywords
             FROM emails e
             JOIN blobs b ON b.id = e.blob_id
             JOIN sync_id_maildir m ON m.local_id = e.id AND m.type_name = 'email'
             ORDER BY m.folder, m.unique_id",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            let folder: String = r.get(0)?;
            let hash: Vec<u8> = r.get(1)?;
            let kw: String = r.get(2)?;
            Ok((folder, hex(&hash), kw))
        })
        .unwrap();
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for r in rows {
        let (folder, hash, kw) = r.unwrap();
        out.entry(folder).or_default().insert(hash, kw);
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[test]
fn second_run_with_no_changes_creates_nothing_and_deletes_nothing() {
    let td = tempfile::TempDir::new().unwrap();
    build_fixture(td.path());
    let archive = common::tmp_archive("idempotent");
    let first = base_cfg(td.path())
        .run(common::live_options(&archive))
        .into_result()
        .expect("first");
    assert!(!first.any_failed());
    let (_, mbox_first) = &first.per_type[0];
    let (_, email_first) = &first.per_type[1];
    assert!(mbox_first.created >= 4);
    assert_eq!(email_first.created, 6);

    let second = base_cfg(td.path())
        .run(common::live_options(&archive))
        .into_result()
        .expect("second");
    let (_, mbox_second) = &second.per_type[0];
    let (_, email_second) = &second.per_type[1];
    assert_eq!(mbox_second.created, 0);
    assert_eq!(mbox_second.deleted, 0);
    assert_eq!(email_second.created, 0);
    assert_eq!(email_second.deleted, 0);
    assert_eq!(email_second.fetched, 0, "no body re-read on convergent run");
    assert!(email_second.skipped >= 6);

    let _ = std::fs::remove_file(&archive);
}

#[test]
fn flag_change_between_runs_updates_keywords_only() {
    let td = tempfile::TempDir::new().unwrap();
    ensure_maildir(td.path());
    let path = write(
        td.path(),
        "cur",
        "1.M0.host:2,",
        &msg("flag-1", "subj", "body"),
    );
    let archive = common::tmp_archive("flag-update");
    base_cfg(td.path())
        .run(common::live_options(&archive))
        .into_result()
        .expect("first");
    let conn = Connection::open(&archive).unwrap();
    let email_id: i64 = conn
        .query_row("SELECT id FROM emails LIMIT 1", [], |r| r.get(0))
        .unwrap();
    let blob_id: i64 = conn
        .query_row(
            "SELECT blob_id FROM emails WHERE id = ?1",
            [email_id],
            |r| r.get(0),
        )
        .unwrap();
    let kw: String = conn
        .query_row(
            "SELECT keywords FROM emails WHERE id = ?1",
            [email_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!kw.contains("$seen"));
    drop(conn);

    fs::rename(&path, path.with_file_name("1.M0.host:2,S")).unwrap();

    let summary = base_cfg(td.path())
        .run(common::live_options(&archive))
        .into_result()
        .expect("second");
    let (_, email_counts) = &summary.per_type[1];
    assert_eq!(email_counts.created, 0);
    assert_eq!(email_counts.deleted, 0);
    assert_eq!(email_counts.fetched, 1, "exactly one flag-only update");

    let conn = Connection::open(&archive).unwrap();
    let new_id: i64 = conn
        .query_row("SELECT id FROM emails LIMIT 1", [], |r| r.get(0))
        .unwrap();
    let new_blob_id: i64 = conn
        .query_row("SELECT blob_id FROM emails WHERE id = ?1", [new_id], |r| {
            r.get(0)
        })
        .unwrap();
    let new_kw: String = conn
        .query_row("SELECT keywords FROM emails WHERE id = ?1", [new_id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(new_id, email_id, "row identity preserved");
    assert_eq!(new_blob_id, blob_id, "blob is unchanged");
    assert!(new_kw.contains("$seen"));
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn delete_on_disk_removes_email_row_and_orphan_blob() {
    let td = tempfile::TempDir::new().unwrap();
    ensure_maildir(td.path());
    let p1 = write(
        td.path(),
        "cur",
        "1.M0.host:2,",
        &msg("del-1", "subj-1", "body-1"),
    );
    write(
        td.path(),
        "cur",
        "2.M0.host:2,",
        &msg("del-2", "subj-2", "body-2"),
    );
    let archive = common::tmp_archive("delete");
    base_cfg(td.path())
        .run(common::live_options(&archive))
        .into_result()
        .expect("first");
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(common::count(&conn, "emails"), 2);
    assert_eq!(common::count(&conn, "blobs"), 2);
    drop(conn);

    fs::remove_file(&p1).unwrap();
    let summary = base_cfg(td.path())
        .run(common::live_options(&archive))
        .into_result()
        .expect("second");
    let (_, email_counts) = &summary.per_type[1];
    assert_eq!(email_counts.deleted, 1);
    assert_eq!(email_counts.created, 0);

    let conn = Connection::open(&archive).unwrap();
    assert_eq!(common::count(&conn, "emails"), 1);

    assert_eq!(common::count(&conn, "blobs"), 1);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn add_on_disk_inserts_one_new_email_only() {
    let td = tempfile::TempDir::new().unwrap();
    ensure_maildir(td.path());
    write(td.path(), "cur", "1.M0.host:2,", &msg("a-1", "1", "body"));
    let archive = common::tmp_archive("add");
    base_cfg(td.path())
        .run(common::live_options(&archive))
        .into_result()
        .expect("first");

    write(td.path(), "cur", "2.M0.host:2,", &msg("a-2", "2", "body2"));
    let summary = base_cfg(td.path())
        .run(common::live_options(&archive))
        .into_result()
        .expect("second");
    let (_, email_counts) = &summary.per_type[1];
    assert_eq!(email_counts.created, 1);
    assert_eq!(email_counts.deleted, 0);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn folder_delete_vanishes_emails_leaf_first() {
    let td = tempfile::TempDir::new().unwrap();
    ensure_maildir(td.path());
    ensure_subfolder(td.path(), ".Archive");
    let arch25 = ensure_subfolder(td.path(), ".Archive.2025");
    write(&arch25, "cur", "1.M0.host:2,", &msg("d-1", "del-1", "del"));
    let archive = common::tmp_archive("folder-delete");
    base_cfg(td.path())
        .run(common::live_options(&archive))
        .into_result()
        .expect("first");

    let conn = Connection::open(&archive).unwrap();
    assert!(
        conn.query_row("SELECT 1 FROM mailboxes WHERE name = '2025'", [], |r| r
            .get::<_, i64>(0))
            .is_ok()
    );
    drop(conn);

    fs::remove_dir_all(td.path().join(".Archive.2025")).unwrap();
    let summary = base_cfg(td.path())
        .run(common::live_options(&archive))
        .into_result()
        .expect("second");
    let (_, mbox_counts) = &summary.per_type[0];
    let (_, email_counts) = &summary.per_type[1];
    assert_eq!(mbox_counts.deleted, 1);
    assert_eq!(email_counts.deleted, 1);

    let conn = Connection::open(&archive).unwrap();
    assert!(
        conn.query_row("SELECT 1 FROM mailboxes WHERE name = '2025'", [], |r| r
            .get::<_, i64>(0))
            .is_err()
    );

    assert!(
        conn.query_row("SELECT 1 FROM mailboxes WHERE name = 'Archive'", [], |r| {
            r.get::<_, i64>(0)
        })
        .is_ok()
    );
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn folder_rename_vanishes_old_and_inserts_new() {
    let td = tempfile::TempDir::new().unwrap();
    ensure_maildir(td.path());
    let old = ensure_subfolder(td.path(), ".Old");
    write(
        &old,
        "cur",
        "1.M0.host:2,",
        &msg("r-1", "to-rename", "body"),
    );
    let archive = common::tmp_archive("folder-rename");
    base_cfg(td.path())
        .run(common::live_options(&archive))
        .into_result()
        .expect("first");

    fs::rename(td.path().join(".Old"), td.path().join(".New")).unwrap();
    let summary = base_cfg(td.path())
        .run(common::live_options(&archive))
        .into_result()
        .expect("second");
    let (_, mbox_counts) = &summary.per_type[0];
    let (_, email_counts) = &summary.per_type[1];
    assert_eq!(mbox_counts.deleted, 1);
    assert_eq!(mbox_counts.created, 1);

    assert_eq!(email_counts.deleted, 1);
    assert_eq!(email_counts.created, 1);
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(common::count(&conn, "blobs"), 1, "blob retained via dedup");
    let _ = std::fs::remove_file(&archive);
}

fn cwd_lock() -> std::sync::MutexGuard<'static, ()> {
    static M: std::sync::Mutex<()> = std::sync::Mutex::new(());
    M.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn relative_path_resolves_to_same_canonical_source() {
    let _g = cwd_lock();
    let td = tempfile::TempDir::new().unwrap();
    build_fixture(td.path());
    let archive = common::tmp_archive("canonical");
    base_cfg(td.path())
        .run(common::live_options(&archive))
        .into_result()
        .expect("first import");

    let parent = td.path().parent().unwrap();
    let dir_name = td.path().file_name().unwrap();
    let cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(parent).unwrap();
    let mut second = base_cfg(Path::new(dir_name));
    second.source_change = SourceChange::Reject;
    let result = second.run(common::live_options(&archive)).into_result();
    std::env::set_current_dir(cwd).unwrap();
    assert!(result.is_ok(), "{result:?}");
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn dry_run_then_real_run_produces_same_counts_for_new() {
    let td = tempfile::TempDir::new().unwrap();
    build_fixture(td.path());
    let archive = common::tmp_archive("dry-then-real");

    let mut dry = common::live_options(&archive);
    dry.mode = RunMode::DryRun;
    let dry_summary = base_cfg(td.path()).run(dry).into_result().expect("dry");
    let (_, dry_email) = &dry_summary.plan[1];
    let new_via_dry = dry_email.new;

    let real_summary = base_cfg(td.path())
        .run(common::live_options(&archive))
        .into_result()
        .expect("real run");
    let (_, real_email) = &real_summary.per_type[1];
    assert_eq!(new_via_dry, real_email.created);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn trashed_flag_added_between_runs_deletes_present_row() {
    let td = tempfile::TempDir::new().unwrap();
    ensure_maildir(td.path());
    let path = write(
        td.path(),
        "cur",
        "trash.M0.host:2,",
        &msg("t-1", "subj", "body"),
    );
    let archive = common::tmp_archive("trash-on-present");
    base_cfg(td.path())
        .run(common::live_options(&archive))
        .into_result()
        .expect("first");
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(common::count(&conn, "emails"), 1);
    drop(conn);

    fs::rename(&path, path.with_file_name("trash.M0.host:2,T")).unwrap();
    let summary = base_cfg(td.path())
        .run(common::live_options(&archive))
        .into_result()
        .expect("second");
    let (_, email_counts) = &summary.per_type[1];
    assert_eq!(email_counts.deleted, 1, "T flag drops the present row");
    assert_eq!(email_counts.fetched, 0);
    assert_eq!(email_counts.created, 0);

    let conn = Connection::open(&archive).unwrap();
    assert_eq!(common::count(&conn, "emails"), 0);

    let _ = std::fs::remove_file(&archive);
}

#[cfg(unix)]
#[test]
fn symlinked_subfolder_is_followed_and_appears_as_its_own_folder() {
    let td = tempfile::TempDir::new().unwrap();
    ensure_maildir(td.path());
    let real = ensure_subfolder(td.path(), ".Real");
    write(
        &real,
        "cur",
        "1.M0.host:2,S",
        &msg("sym-1", "shared", "shared body"),
    );
    std::os::unix::fs::symlink(&real, td.path().join(".Shared")).unwrap();
    let archive = common::tmp_archive("symlink");
    let summary = base_cfg(td.path())
        .run(common::live_options(&archive))
        .into_result()
        .expect("import");
    assert!(!summary.any_failed(), "{summary:?}");

    let conn = Connection::open(&archive).unwrap();
    assert!(
        conn.query_row("SELECT 1 FROM mailboxes WHERE name = 'Real'", [], |r| r
            .get::<_, i64>(0))
            .is_ok(),
        "Real folder discovered"
    );
    assert!(
        conn.query_row("SELECT 1 FROM mailboxes WHERE name = 'Shared'", [], |r| r
            .get::<_, i64>(
            0
        ))
        .is_ok(),
        "Shared (symlink) folder discovered"
    );

    assert_eq!(common::count(&conn, "blobs"), 1);
    assert_eq!(common::count(&conn, "emails"), 2);
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn convergence_is_stable_across_three_runs() {
    let td = tempfile::TempDir::new().unwrap();
    build_fixture(td.path());
    let archive = common::tmp_archive("triple-converge");
    base_cfg(td.path())
        .run(common::live_options(&archive))
        .into_result()
        .expect("r1");
    let snap1 = {
        let conn = Connection::open(&archive).unwrap();
        snapshot_hashes_by_folder(&conn)
    };
    base_cfg(td.path())
        .run(common::live_options(&archive))
        .into_result()
        .expect("r2");
    let snap2 = {
        let conn = Connection::open(&archive).unwrap();
        snapshot_hashes_by_folder(&conn)
    };
    base_cfg(td.path())
        .run(common::live_options(&archive))
        .into_result()
        .expect("r3");
    let snap3 = {
        let conn = Connection::open(&archive).unwrap();
        snapshot_hashes_by_folder(&conn)
    };
    assert_eq!(snap1, snap2);
    assert_eq!(snap2, snap3);
    let _ = std::fs::remove_file(&archive);
}

fn lay_seeder_mbox_as_maildir(root: &Path, messages: &[seeder::data::MboxMessage]) {
    ensure_maildir(root);
    for (i, m) in messages.iter().enumerate() {
        let filename = format!("{seq}.M0.parity-host:2,S", seq = 1_700_000_000 + i as u64);
        write(root, "cur", &filename, &m.raw);
    }
}

#[test]
#[ignore = "requires Docker"]
fn maildir_and_jmap_imports_share_blob_bytes() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test1").expect("test1");

    let corpus = seeder::data::load_mbox(50).expect("mbox corpus");
    let td = tempfile::TempDir::new().unwrap();
    lay_seeder_mbox_as_maildir(td.path(), &corpus);

    let md_archive = common::tmp_archive("parity-maildir");
    let jmap_archive = common::tmp_archive("parity-jmap");

    base_cfg(td.path())
        .run(common::live_options(&md_archive))
        .into_result()
        .expect("maildir import");

    let jmap_cfg = JmapImport {
        connect: ConnectConfig {
            url: fx.base_url.clone(),
            auth: Auth::Basic {
                user: acc.email.clone(),
                password: seeder::USER_PASSWORD.to_owned(),
            },
            account: AccountSelector::Id(acc.account_id.clone()),
        },
        objects: Some(
            vec![
                vandelay::types::ObjectType::Mailbox,
                vandelay::types::ObjectType::Email,
            ]
            .into(),
        ),
        source_change: SourceChange::Reject,
    };
    let jmap_common = RunOptions {
        archive: jmap_archive.clone(),
        threads: 4,
        mode: RunMode::Apply,
        max_retries: 5,
        tls: TlsPolicy::AcceptInvalid,
        logger: Logger::new(Level::Default),
    };
    jmap_cfg
        .run(jmap_common)
        .into_result()
        .expect("jmap import");

    let md_hashes = blob_hashes(&Connection::open(&md_archive).unwrap());
    let jmap_hashes = blob_hashes(&Connection::open(&jmap_archive).unwrap());

    assert!(
        !md_hashes.is_empty(),
        "maildir archive must have at least one blob"
    );
    let missing: Vec<&String> = md_hashes
        .iter()
        .filter(|h| !jmap_hashes.contains(*h))
        .collect();
    assert!(
        missing.is_empty(),
        "{} blobs in maildir archive missing from JMAP archive: {:?}",
        missing.len(),
        missing.iter().take(3).collect::<Vec<_>>()
    );

    let _ = std::fs::remove_file(&md_archive);
    let _ = std::fs::remove_file(&jmap_archive);
    seeder::teardown(base_url()).expect("teardown");
}

fn blob_hashes(conn: &Connection) -> std::collections::HashSet<String> {
    let mut stmt = conn.prepare("SELECT hash FROM blobs").unwrap();
    stmt.query_map([], |r| Ok(hex(&r.get::<_, Vec<u8>>(0)?)))
        .unwrap()
        .filter_map(Result::ok)
        .collect()
}

#[test]
#[ignore = "requires Docker"]
fn maildir_message_count_matches_jmap_for_same_corpus() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test1").expect("test1");
    let corpus = seeder::data::load_mbox(30).expect("mbox corpus");
    let td = tempfile::TempDir::new().unwrap();
    lay_seeder_mbox_as_maildir(td.path(), &corpus);

    let md_archive = common::tmp_archive("count-maildir");
    base_cfg(td.path())
        .run(common::live_options(&md_archive))
        .into_result()
        .expect("maildir import");
    let md_conn = Connection::open(&md_archive).unwrap();
    let md_count = common::count(&md_conn, "emails");
    assert_eq!(
        md_count as usize,
        corpus.len(),
        "all corpus messages imported"
    );

    let jmap_archive = common::tmp_archive("count-jmap");
    let jmap_cfg = JmapImport {
        connect: ConnectConfig {
            url: fx.base_url.clone(),
            auth: Auth::Basic {
                user: acc.email.clone(),
                password: seeder::USER_PASSWORD.to_owned(),
            },
            account: AccountSelector::Id(acc.account_id.clone()),
        },
        objects: Some(
            vec![
                vandelay::types::ObjectType::Mailbox,
                vandelay::types::ObjectType::Email,
            ]
            .into(),
        ),
        source_change: SourceChange::Reject,
    };
    let jmap_common = RunOptions {
        archive: jmap_archive.clone(),
        threads: 4,
        mode: RunMode::Apply,
        max_retries: 5,
        tls: TlsPolicy::AcceptInvalid,
        logger: Logger::new(Level::Default),
    };
    jmap_cfg
        .run(jmap_common)
        .into_result()
        .expect("jmap import");

    let _ = std::fs::remove_file(&md_archive);
    let _ = std::fs::remove_file(&jmap_archive);
    seeder::teardown(base_url()).expect("teardown");
}
