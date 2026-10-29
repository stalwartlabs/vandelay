/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use tempfile::TempDir;
use vandelay::logging::Logger;
use vandelay::sync::CommonConfig;
use vandelay::sync::import_maildir::{MaildirImportConfig, run};

fn tmp_archive(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "vandelay-mock-maildir-{tag}-{}-{}.sqlite",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

fn common(archive: &Path) -> CommonConfig {
    CommonConfig {
        archive: archive.to_path_buf(),
        threads: 1,
        dry_run: false,
        max_retries: 1,
        allow_invalid_certs: false,
        logger: Logger::from_flags(true, 0),
    }
}

fn base_cfg(root: &Path) -> MaildirImportConfig {
    MaildirImportConfig {
        maildir: root.to_path_buf(),
        include: Vec::new(),
        exclude: Vec::new(),
        folder: Vec::new(),
        automap: true,
        include_deleted: false,
        allow_source_change: false,
    }
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
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

fn write_message(folder: &Path, sub: &str, filename: &str, body: &[u8]) -> PathBuf {
    let dir = folder.join(sub);
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join(filename);
    fs::write(&p, body).unwrap();
    p
}

fn rfc5322(id: &str, subject: &str, body: &str) -> Vec<u8> {
    format!(
        "From: sender@example.com\r\nTo: rcpt@example.com\r\n\
         Subject: {subject}\r\nMessage-ID: <{id}@example.com>\r\n\
         Date: Mon, 12 May 2025 10:00:00 +0000\r\n\r\n{body}\r\n"
    )
    .into_bytes()
}

fn folder_mailbox_id(conn: &Connection, canonical: &str) -> Option<i64> {
    conn.query_row(
        "SELECT local_id FROM sync_id_maildir
         WHERE type_name = 'mailbox' AND folder = ?1 AND unique_id = ''",
        rusqlite::params![canonical],
        |r| r.get(0),
    )
    .ok()
}

fn emails_for_folder(conn: &Connection, canonical: &str) -> Vec<(String, String)> {
    let mailbox_id = folder_mailbox_id(conn, canonical).expect("mailbox row");
    let mut stmt = conn
        .prepare(
            "SELECT b.hash, e.keywords FROM emails e
             JOIN blobs b ON b.id = e.blob_id
             WHERE EXISTS (
                 SELECT 1 FROM json_each(e.mailbox_ids) WHERE value = ?1
             )
             ORDER BY e.id",
        )
        .unwrap();
    let rows = stmt
        .query_map(rusqlite::params![mailbox_id], |r| {
            let hash: Vec<u8> = r.get(0)?;
            let kw: String = r.get(1)?;
            Ok((hex(&hash), kw))
        })
        .unwrap();
    rows.filter_map(|r| r.ok()).collect()
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn dovecot_fixture(root: &Path) -> HashMap<&'static str, Vec<&'static str>> {
    ensure_maildir(root);

    write_message(
        root,
        "cur",
        "1739471123.M001P00123V0000I0000aaaa.host01,S=128,W=131:2,S",
        &rfc5322("dovecot-inbox-1", "Welcome", "Welcome to Dovecot"),
    );
    write_message(
        root,
        "cur",
        "1739471124.M002P00123V0000I0000aaab.host01,S=140,W=143:2,RS",
        &rfc5322("dovecot-inbox-2", "Re: Welcome", "Reply body"),
    );
    write_message(
        root,
        "new",
        "1739471200.M003P00123V0000I0000aaac.host01,S=64,W=66",
        &rfc5322("dovecot-inbox-3", "Unread mail", "Body of the unread"),
    );

    let sent = ensure_subfolder(root, ".Sent");
    write_message(
        &sent,
        "cur",
        "1739472000.M001P00123V0000I0000bbb1.host01,S=80,W=82:2,S",
        &rfc5322("dovecot-sent-1", "Sent to friend", "Hi friend"),
    );

    let drafts = ensure_subfolder(root, ".Drafts");
    write_message(
        &drafts,
        "cur",
        "1739472100.M002P00123V0000I0000bbb2.host01,S=50,W=52:2,DS",
        &rfc5322("dovecot-draft-1", "Half-written", "Almost there"),
    );

    let trash = ensure_subfolder(root, ".Trash");
    write_message(
        &trash,
        "cur",
        "1739472200.M003P00123V0000I0000bbb3.host01,S=30,W=32:2,ST",
        &rfc5322("dovecot-trash-1", "Junk", "trashed"),
    );

    ensure_subfolder(root, ".Archive");
    let arch25 = ensure_subfolder(root, ".Archive.2025");
    write_message(
        &arch25,
        "cur",
        "1739473000.M001P00123V0000I0000cccc.host01,S=200,W=204:2,S",
        &rfc5322("dovecot-arch25-1", "Last year", "Old mail"),
    );
    write_message(
        &arch25,
        "cur",
        "1739473001.M002P00123V0000I0000cccd.host01,S=200,W=204:2,SF",
        &rfc5322("dovecot-arch25-2", "Important", "Flagged"),
    );

    fs::write(root.join("dovecot.index.log"), b"index").unwrap();
    fs::write(root.join("dovecot-uidlist"), b"uidlist").unwrap();
    fs::create_dir_all(root.join(".dovecot.imap")).unwrap();

    fs::write(root.join("cur/.uidvalidity"), b"sidecar").unwrap();

    let mut expected: HashMap<&'static str, Vec<&'static str>> = HashMap::new();
    expected.insert("INBOX", vec![]);
    expected.insert("Sent", vec![]);
    expected.insert("Drafts", vec![]);
    expected.insert("Trash", vec![]);
    expected.insert("Archive", vec![]);
    expected.insert("Archive.2025", vec![]);
    expected
}

#[test]
fn dovecot_layout_imports_every_folder_with_correct_roles() {
    let td = TempDir::new().unwrap();
    let expected = dovecot_fixture(td.path());
    let archive = tmp_archive("dovecot");
    let summary = run(common(&archive), base_cfg(td.path())).expect("import");
    assert!(!summary.any_failed(), "import failed: {summary:?}");

    let conn = Connection::open(&archive).unwrap();
    for name in expected.keys() {
        assert!(
            folder_mailbox_id(&conn, name).is_some(),
            "missing mailbox row for {name:?}"
        );
    }

    let inbox = emails_for_folder(&conn, "INBOX");
    assert_eq!(inbox.len(), 3, "3 INBOX messages expected");
    let seen_count = inbox.iter().filter(|(_, kw)| kw.contains("$seen")).count();
    assert_eq!(seen_count, 2, "two cur/ msgs are $seen");

    let sent = emails_for_folder(&conn, "Sent");
    assert_eq!(sent.len(), 1);

    let trash = emails_for_folder(&conn, "Trash");
    assert!(trash.is_empty(), "Trashed message skipped by default");

    let arch_2025 = emails_for_folder(&conn, "Archive.2025");
    assert_eq!(arch_2025.len(), 2);
    let flagged = arch_2025.iter().any(|(_, kw)| kw.contains("$flagged"));
    assert!(flagged, "one Archive.2025 message has $flagged");

    let archive_parent_id = folder_mailbox_id(&conn, "Archive").unwrap();
    let arch_2025_parent: Option<i64> = conn
        .query_row(
            "SELECT parent_id FROM mailboxes WHERE name = '2025'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(arch_2025_parent, Some(archive_parent_id));

    let _ = fs::remove_file(&archive);
}

#[test]
fn dovecot_include_deleted_keeps_trash_message() {
    let td = TempDir::new().unwrap();
    dovecot_fixture(td.path());
    let archive = tmp_archive("dovecot-include-deleted");
    let mut cfg = base_cfg(td.path());
    cfg.include_deleted = true;
    let summary = run(common(&archive), cfg).expect("import");
    assert!(!summary.any_failed());
    let conn = Connection::open(&archive).unwrap();
    let trash = emails_for_folder(&conn, "Trash");
    assert_eq!(trash.len(), 1);
    assert!(trash[0].1.contains("$deleted"));
    let _ = fs::remove_file(&archive);
}

fn courier_fixture(root: &Path) {
    ensure_maildir(root);
    write_message(
        root,
        "cur",
        "1234567890.12345_0.mail.example:2,S",
        &rfc5322("courier-1", "Hi", "Hello from Courier"),
    );
    write_message(
        root,
        "new",
        "1234567891.12345_1.mail.example",
        &rfc5322("courier-2", "Unread", "New body"),
    );

    let sent = ensure_subfolder(root, ".Sent");
    write_message(
        &sent,
        "cur",
        "1234567900.12350_0.mail.example:2,S",
        &rfc5322("courier-sent-1", "Sent reply", "Bye"),
    );

    let lists = ensure_subfolder(root, ".Lists.maildir-dev");
    write_message(
        &lists,
        "cur",
        "1234568000.12400_0.mail.example:2,RS",
        &rfc5322("courier-list-1", "[Maildir-dev] thread", "list body"),
    );

    fs::write(root.join("courierimapsubscribed"), b"INBOX.Sent\n").unwrap();
}

#[test]
fn courier_layout_with_orphan_parent_creates_ephemeral_lists_row() {
    let td = TempDir::new().unwrap();
    courier_fixture(td.path());
    let archive = tmp_archive("courier");
    let summary = run(common(&archive), base_cfg(td.path())).expect("import");
    assert!(!summary.any_failed(), "{summary:?}");

    let conn = Connection::open(&archive).unwrap();
    assert!(folder_mailbox_id(&conn, "INBOX").is_some());
    assert!(folder_mailbox_id(&conn, "Sent").is_some());
    let lists_id = folder_mailbox_id(&conn, "Lists").expect("ephemeral Lists row created");
    let leaf_id = folder_mailbox_id(&conn, "Lists.maildir-dev").expect("Lists.maildir-dev row");
    let leaf_parent: Option<i64> = conn
        .query_row(
            "SELECT parent_id FROM mailboxes WHERE id = ?1",
            rusqlite::params![leaf_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leaf_parent, Some(lists_id));

    let role: Option<String> = conn
        .query_row(
            "SELECT role FROM mailboxes WHERE id = ?1",
            rusqlite::params![lists_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(role, None);

    assert_eq!(emails_for_folder(&conn, "INBOX").len(), 2);
    assert_eq!(emails_for_folder(&conn, "Sent").len(), 1);
    assert_eq!(emails_for_folder(&conn, "Lists.maildir-dev").len(), 1);
    let _ = fs::remove_file(&archive);
}

fn qmail_fixture(root: &Path) {
    ensure_maildir(root);

    write_message(
        root,
        "new",
        "1700000000.12345.box01",
        &rfc5322("qmail-1", "qmail one", "body 1"),
    );
    write_message(
        root,
        "new",
        "1700000001.12346.box01",
        &rfc5322("qmail-2", "qmail two", "body 2"),
    );

    write_message(
        root,
        "cur",
        "1700000002.12347.box01:2,S",
        &rfc5322("qmail-3", "Seen", "body 3"),
    );
}

#[test]
fn qmail_root_only_maildir_imports_inbox_only() {
    let td = TempDir::new().unwrap();
    qmail_fixture(td.path());
    let archive = tmp_archive("qmail");
    let summary = run(common(&archive), base_cfg(td.path())).expect("import");
    assert!(!summary.any_failed());
    let conn = Connection::open(&archive).unwrap();
    let mboxes = count(&conn, "mailboxes");
    assert_eq!(mboxes, 1, "INBOX only");
    let inbox = emails_for_folder(&conn, "INBOX");
    assert_eq!(inbox.len(), 3);
    let seen = inbox.iter().filter(|(_, kw)| kw.contains("$seen")).count();
    assert_eq!(seen, 1, "only the cur/ message has $seen");
    let _ = fs::remove_file(&archive);
}

fn cyrus_export_fixture(root: &Path) {
    ensure_maildir(root);
    write_message(
        root,
        "cur",
        "1701000000.X1.cyrus-export:2,S",
        &rfc5322("cyrus-1", "Cyrus mail 1", "exported"),
    );
    write_message(
        root,
        "cur",
        "1701000001.X2.cyrus-export:2,",
        &rfc5322("cyrus-2", "Cyrus mail 2", "exported"),
    );

    let sent = ensure_subfolder(root, ".Sent");
    write_message(
        &sent,
        "cur",
        "1701000100.X3.cyrus-export:2,S",
        &rfc5322("cyrus-sent-1", "Sent from Cyrus", "exported sent"),
    );

    let arch = ensure_subfolder(root, ".Archive");
    write_message(
        &arch,
        "cur",
        "1701000200.X4.cyrus-export:2,S",
        &rfc5322("cyrus-arch-1", "Archived", "old"),
    );
}

#[test]
fn cyrus_style_export_imports_cleanly() {
    let td = TempDir::new().unwrap();
    cyrus_export_fixture(td.path());
    let archive = tmp_archive("cyrus");
    let summary = run(common(&archive), base_cfg(td.path())).expect("import");
    assert!(!summary.any_failed());
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(emails_for_folder(&conn, "INBOX").len(), 2);
    assert_eq!(emails_for_folder(&conn, "Sent").len(), 1);
    assert_eq!(emails_for_folder(&conn, "Archive").len(), 1);
    let sent_role: Option<String> = conn
        .query_row("SELECT role FROM mailboxes WHERE name = 'Sent'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(sent_role.as_deref(), Some("sent"));
    let _ = fs::remove_file(&archive);
}

#[test]
fn include_filter_keeps_only_matching_folders() {
    let td = TempDir::new().unwrap();
    dovecot_fixture(td.path());
    let archive = tmp_archive("filter-include");
    let mut cfg = base_cfg(td.path());
    cfg.include = vec![regex::Regex::new(r"^(INBOX|Sent)$").unwrap()];
    let summary = run(common(&archive), cfg).expect("import");
    assert!(!summary.any_failed());
    let conn = Connection::open(&archive).unwrap();
    assert!(folder_mailbox_id(&conn, "INBOX").is_some());
    assert!(folder_mailbox_id(&conn, "Sent").is_some());
    assert!(folder_mailbox_id(&conn, "Trash").is_none());
    assert!(folder_mailbox_id(&conn, "Archive.2025").is_none());
    let _ = fs::remove_file(&archive);
}

#[test]
fn exclude_filter_drops_matching_folders() {
    let td = TempDir::new().unwrap();
    dovecot_fixture(td.path());
    let archive = tmp_archive("filter-exclude");
    let mut cfg = base_cfg(td.path());
    cfg.exclude = vec![regex::Regex::new(r"^Trash$").unwrap()];
    let summary = run(common(&archive), cfg).expect("import");
    assert!(!summary.any_failed());
    let conn = Connection::open(&archive).unwrap();
    assert!(folder_mailbox_id(&conn, "Trash").is_none());
    assert!(folder_mailbox_id(&conn, "Sent").is_some());
    let _ = fs::remove_file(&archive);
}

#[test]
fn explicit_folder_overrides_include_exclude() {
    let td = TempDir::new().unwrap();
    dovecot_fixture(td.path());
    let archive = tmp_archive("filter-folder");
    let mut cfg = base_cfg(td.path());
    cfg.folder = vec!["Sent".to_owned()];
    let summary = run(common(&archive), cfg).expect("import");
    assert!(!summary.any_failed());
    let conn = Connection::open(&archive).unwrap();
    let mbox_count = count(&conn, "mailboxes");
    assert_eq!(mbox_count, 1);
    assert!(folder_mailbox_id(&conn, "Sent").is_some());
    let _ = fs::remove_file(&archive);
}

#[test]
fn noautomap_drops_roles_except_inbox() {
    let td = TempDir::new().unwrap();
    dovecot_fixture(td.path());
    let archive = tmp_archive("noautomap");
    let mut cfg = base_cfg(td.path());
    cfg.automap = false;
    let summary = run(common(&archive), cfg).expect("import");
    assert!(!summary.any_failed());
    let conn = Connection::open(&archive).unwrap();
    let sent_role: Option<String> = conn
        .query_row("SELECT role FROM mailboxes WHERE name = 'Sent'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(sent_role.is_none(), "automap off -> Sent has no role");
    let inbox_role: Option<String> = conn
        .query_row("SELECT role FROM mailboxes WHERE name = 'INBOX'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(inbox_role.as_deref(), Some("inbox"));
    let _ = fs::remove_file(&archive);
}

#[test]
fn dry_run_reports_diff_without_writing_emails() {
    let td = TempDir::new().unwrap();
    dovecot_fixture(td.path());
    let archive = tmp_archive("dry-run");
    let mut common_cfg = common(&archive);
    common_cfg.dry_run = true;
    let summary = run(common_cfg, base_cfg(td.path())).expect("dryrun");
    let (mailbox_label, mailbox_counts) = &summary.per_type[0];
    let (email_label, email_counts) = &summary.per_type[1];
    assert_eq!(*mailbox_label, "mailbox");
    assert_eq!(*email_label, "email");
    assert!(mailbox_counts.created >= 6);
    assert!(email_counts.created >= 6);

    let conn = Connection::open(&archive).unwrap();
    assert_eq!(count(&conn, "emails"), 0, "dry-run wrote no emails");
    assert_eq!(count(&conn, "mailboxes"), 0, "dry-run wrote no mailboxes");
    let _ = fs::remove_file(&archive);
}

#[test]
fn rejects_path_without_cur_subdir() {
    let td = TempDir::new().unwrap();
    fs::create_dir_all(td.path().join("new")).unwrap();
    let archive = tmp_archive("not-a-maildir");
    let err = run(common(&archive), base_cfg(td.path())).expect_err("should refuse");
    assert!(matches!(err, vandelay::error::Error::Usage(_)));
    let _ = fs::remove_file(&archive);
}

#[test]
fn rejects_dovecot_layout_fs_tree() {
    let td = TempDir::new().unwrap();
    ensure_maildir(td.path());
    for s in ["cur", "new", "tmp"] {
        fs::create_dir_all(td.path().join("Sent").join(s)).unwrap();
    }
    let archive = tmp_archive("layout-fs");
    let err = run(common(&archive), base_cfg(td.path())).expect_err("should refuse");
    match err {
        vandelay::error::Error::Usage(msg) => {
            assert!(msg.contains("Maildir++"), "msg was: {msg}");
        }
        other => panic!("expected Usage error, got {other:?}"),
    }
    let _ = fs::remove_file(&archive);
}

#[test]
fn rejects_nonexistent_path() {
    let archive = tmp_archive("nonexistent");
    let mut cfg = base_cfg(Path::new("/definitely/not/a/real/maildir"));
    cfg.maildir = PathBuf::from("/definitely/not/a/real/maildir");
    let err = run(common(&archive), cfg).expect_err("should refuse");
    assert!(matches!(err, vandelay::error::Error::Usage(_)));
    let _ = fs::remove_file(&archive);
}

#[test]
fn source_change_protection_refuses_second_path() {
    let td_a = TempDir::new().unwrap();
    dovecot_fixture(td_a.path());
    let td_b = TempDir::new().unwrap();
    qmail_fixture(td_b.path());

    let archive = tmp_archive("source-change");
    run(common(&archive), base_cfg(td_a.path())).expect("first import");
    let err = run(common(&archive), base_cfg(td_b.path())).expect_err("second import refused");
    assert!(matches!(err, vandelay::error::Error::SourceChange(_)));
    let _ = fs::remove_file(&archive);
}

#[test]
fn allow_source_change_unlocks_second_path() {
    let td_a = TempDir::new().unwrap();
    dovecot_fixture(td_a.path());
    let td_b = TempDir::new().unwrap();
    qmail_fixture(td_b.path());

    let archive = tmp_archive("allow-source-change");
    run(common(&archive), base_cfg(td_a.path())).expect("first import");
    let mut second = base_cfg(td_b.path());
    second.allow_source_change = true;
    run(common(&archive), second).expect("second import permitted");
    let _ = fs::remove_file(&archive);
}

#[test]
fn folder_and_include_are_mutually_exclusive() {
    let td = TempDir::new().unwrap();
    ensure_maildir(td.path());
    let archive = tmp_archive("mutex-folder-include");
    let mut cfg = base_cfg(td.path());
    cfg.folder = vec!["INBOX".to_owned()];
    cfg.include = vec![regex::Regex::new("^Sent$").unwrap()];
    let err = run(common(&archive), cfg).expect_err("mutex");
    match err {
        vandelay::error::Error::Usage(msg) => {
            assert!(
                msg.contains("--folder") && msg.contains("--include"),
                "msg was: {msg}"
            );
        }
        other => panic!("expected Usage, got {other:?}"),
    }
    let _ = fs::remove_file(&archive);
}

#[test]
fn folder_and_exclude_are_mutually_exclusive() {
    let td = TempDir::new().unwrap();
    ensure_maildir(td.path());
    let archive = tmp_archive("mutex-folder-exclude");
    let mut cfg = base_cfg(td.path());
    cfg.folder = vec!["INBOX".to_owned()];
    cfg.exclude = vec![regex::Regex::new("^Trash$").unwrap()];
    let err = run(common(&archive), cfg).expect_err("mutex");
    assert!(matches!(err, vandelay::error::Error::Usage(_)));
    let _ = fs::remove_file(&archive);
}

#[test]
fn three_level_folder_delete_is_leaf_first() {
    let td = TempDir::new().unwrap();
    ensure_maildir(td.path());
    ensure_subfolder(td.path(), ".A");
    ensure_subfolder(td.path(), ".A.B");
    let leaf = ensure_subfolder(td.path(), ".A.B.C");
    write_message(
        &leaf,
        "cur",
        "1.M0.host:2,S",
        &rfc5322("3lvl-1", "x", "body"),
    );
    let archive = tmp_archive("three-level-delete");
    run(common(&archive), base_cfg(td.path())).expect("first import");

    fs::remove_dir_all(td.path().join(".A.B.C")).unwrap();
    fs::remove_dir_all(td.path().join(".A.B")).unwrap();
    let summary = run(common(&archive), base_cfg(td.path())).expect("second import");
    assert!(!summary.any_failed(), "{summary:?}");
    let (_, mbox) = &summary.per_type[0];
    let (_, email) = &summary.per_type[1];
    assert_eq!(mbox.deleted, 2, "B and B.C deleted leaf-first");
    assert_eq!(email.deleted, 1);

    let conn = Connection::open(&archive).unwrap();
    assert!(folder_mailbox_id(&conn, "A").is_some());
    assert!(folder_mailbox_id(&conn, "A.B").is_none());
    assert!(folder_mailbox_id(&conn, "A.B.C").is_none());
    let _ = fs::remove_file(&archive);
}

#[test]
fn same_unique_id_in_two_folders_produces_two_rows_one_blob() {
    let td = TempDir::new().unwrap();
    ensure_maildir(td.path());
    let sent = ensure_subfolder(td.path(), ".Sent");
    let body = rfc5322("dup-1", "shared", "shared body");
    write_message(td.path(), "cur", "1.M0.host:2,S", &body);
    write_message(&sent, "cur", "1.M0.host:2,S", &body);
    let archive = tmp_archive("cross-folder-dup");
    let summary = run(common(&archive), base_cfg(td.path())).expect("import");
    assert!(!summary.any_failed(), "{summary:?}");
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(count(&conn, "blobs"), 1, "BLAKE3 dedup");
    assert_eq!(count(&conn, "emails"), 2, "one row per folder");
    let inbox = emails_for_folder(&conn, "INBOX");
    let sent_rows = emails_for_folder(&conn, "Sent");
    assert_eq!(inbox.len(), 1);
    assert_eq!(sent_rows.len(), 1);

    assert_eq!(inbox[0].0, sent_rows[0].0);
    let _ = fs::remove_file(&archive);
}

#[cfg(unix)]
#[test]
fn unreadable_file_is_counted_and_warned_not_aborted() {
    use std::os::unix::fs::PermissionsExt;
    let td = TempDir::new().unwrap();
    ensure_maildir(td.path());
    write_message(
        td.path(),
        "cur",
        "1.M0.host:2,S",
        &rfc5322("ok-1", "ok", "ok"),
    );
    let bad = write_message(
        td.path(),
        "cur",
        "2.M0.host:2,S",
        &rfc5322("bad-1", "denied", "denied"),
    );
    let mut perms = fs::metadata(&bad).unwrap().permissions();
    perms.set_mode(0o000);
    fs::set_permissions(&bad, perms).unwrap();

    let archive = tmp_archive("unreadable");
    let summary = run(common(&archive), base_cfg(td.path())).expect("not aborted");

    let mut perms = fs::metadata(&bad).unwrap().permissions();
    perms.set_mode(0o600);
    fs::set_permissions(&bad, perms).unwrap();

    let (_, email) = &summary.per_type[1];
    assert!(summary.any_failed(), "exit-5 worthy");
    assert_eq!(email.failed, 1, "the bad file is counted");
    assert_eq!(email.created, 1, "the readable file imports");
    let _ = fs::remove_file(&archive);
}

#[test]
fn pointing_at_a_dot_subfolder_warns_but_imports() {
    let parent = TempDir::new().unwrap();
    ensure_maildir(parent.path());
    let sub = ensure_subfolder(parent.path(), ".Sent");
    write_message(&sub, "cur", "1.M0.host:2,S", &rfc5322("sub-1", "x", "x"));

    let archive = tmp_archive("dot-subfolder-as-root");
    let summary = run(common(&archive), base_cfg(&sub)).expect("import proceeds");
    assert!(!summary.any_failed(), "{summary:?}");
    let conn = Connection::open(&archive).unwrap();

    assert!(folder_mailbox_id(&conn, "INBOX").is_some());
    assert_eq!(emails_for_folder(&conn, "INBOX").len(), 1);
    let _ = fs::remove_file(&archive);
}

#[test]
fn malformed_message_yields_zero_message_match_but_imports() {
    let td = TempDir::new().unwrap();
    ensure_maildir(td.path());
    write_message(td.path(), "cur", "1.M0.host:2,S", b"");
    write_message(
        td.path(),
        "cur",
        "2.M0.host:2,S",
        b"X-Garbage: yes\r\n\r\nno headers",
    );
    let archive = tmp_archive("malformed");
    let summary = run(common(&archive), base_cfg(td.path())).expect("import");
    assert!(!summary.any_failed(), "{summary:?}");
    let conn = Connection::open(&archive).unwrap();
    let inbox = emails_for_folder(&conn, "INBOX");
    assert_eq!(inbox.len(), 2);
    let _ = fs::remove_file(&archive);
}

#[test]
fn received_at_comes_from_the_filename_not_a_clobbered_mtime() {
    let td = TempDir::new().unwrap();
    let root = td.path();
    ensure_maildir(root);
    let with_stamp = write_message(
        root,
        "cur",
        "1763065233.M1P1.host,S=120:2,S",
        &rfc5322("stamped", "stamped", "body"),
    );
    let without_stamp = write_message(
        root,
        "cur",
        "M2P2.host:2,S",
        &rfc5322("unstamped", "unstamped", "body"),
    );
    let restored =
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000);
    for p in [&with_stamp, &without_stamp] {
        let f = fs::File::options().write(true).open(p).unwrap();
        f.set_modified(restored).unwrap();
    }

    let archive = tmp_archive("received_at");
    let summary = run(common(&archive), base_cfg(root)).expect("import");
    assert!(!summary.any_failed());

    let conn = Connection::open(&archive).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT e.received_at FROM emails e
             JOIN blobs b ON b.id = e.blob_id
             WHERE CAST(b.data AS TEXT) LIKE ?1",
        )
        .unwrap();
    let stamped: String = stmt
        .query_row(rusqlite::params!["%<stamped@example.com>%"], |r| r.get(0))
        .unwrap();
    assert_eq!(
        stamped, "2025-11-13T20:20:33Z",
        "the maildir filename timestamp wins over a restored mtime"
    );
    let unstamped: String = stmt
        .query_row(rusqlite::params!["%<unstamped@example.com>%"], |r| r.get(0))
        .unwrap();
    assert_eq!(
        unstamped, "2025-05-12T10:00:00Z",
        "without a filename timestamp the message Date header is used"
    );
    drop(stmt);
    let _ = fs::remove_file(&archive);
}
