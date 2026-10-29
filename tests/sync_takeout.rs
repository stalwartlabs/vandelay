/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use tempfile::TempDir;

use vandelay::logging::Logger;
use vandelay::sync::CommonConfig;
use vandelay::sync::import_takeout::{TakeoutImportConfig, run};

const RESOURCES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/resources");

fn common_for(archive: &Path) -> CommonConfig {
    CommonConfig {
        archive: archive.to_path_buf(),
        threads: 1,
        dry_run: false,
        max_retries: 0,
        allow_invalid_certs: false,
        logger: Logger::from_flags(true, 0),
    }
}

fn config_for(root: PathBuf) -> TakeoutImportConfig {
    TakeoutImportConfig {
        takeout_root: root,
        allow_source_change: false,
        automap: true,
    }
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn mailbox_names(conn: &Connection) -> HashSet<String> {
    let mut stmt = conn.prepare("SELECT name FROM mailboxes").unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

struct SyntheticTakeout {
    root: PathBuf,
}

impl SyntheticTakeout {
    fn build(root: &Path) -> Self {
        Self::write_mail(root);
        Self::write_calendar(root);
        Self::write_contacts(root);
        Self::write_noise_files(root);
        Self {
            root: root.to_path_buf(),
        }
    }

    fn path(&self) -> &Path {
        &self.root
    }

    fn write_mail(root: &Path) {
        let mail_dir = root.join("Takeout/Mail");
        fs::create_dir_all(&mail_dir).unwrap();

        let mut all_mail = String::new();
        let messages = [
            (1, "Inbox,Opened", "Plain inbox read"),
            (
                2,
                "Inbox,Important,Opened,WorkProject",
                "Tagged with custom label",
            ),
            (3, "Inbox,Opened,Starred", "Starred mail"),
            (4, "Archived,Sent,Opened", "Sent and archived"),
            (
                5,
                "Archived,Sent,Opened,WorkProject/2026-Q1",
                "Nested custom label",
            ),
            (
                6,
                "Trash,Category Social,Unread",
                "Trash + Category dropped, unread",
            ),
            (
                7,
                "Inbox,Opened,Category Promotions",
                "Inbox + Category Promotions dropped",
            ),
        ];
        for (n, labels, subject) in messages {
            all_mail.push_str(&takeout_message(n, labels, subject));
        }
        fs::write(
            mail_dir.join("All Mail Including Spam and Trash.mbox"),
            all_mail,
        )
        .unwrap();

        let mut per_label = String::new();
        for n in 8..=9 {
            per_label.push_str(&takeout_message(
                n,
                "Inbox,Opened,Github",
                "Per-label export",
            ));
        }
        fs::write(mail_dir.join("Github.mbox"), per_label).unwrap();
    }

    fn write_calendar(root: &Path) {
        let cal_dir = root.join("Takeout/Calendar");
        fs::create_dir_all(&cal_dir).unwrap();
        fs::copy(
            Path::new(RESOURCES).join("icals/000.ics"),
            cal_dir.join("Weekly.ics"),
        )
        .unwrap();
        fs::copy(
            Path::new(RESOURCES).join("icals/002.ics"),
            cal_dir.join("no_calname.ics"),
        )
        .unwrap();
        fs::write(
            cal_dir.join("meet_settings.json"),
            br#"{"Meeting data":[{"Meeting code":"abc-defg-hij"}]}"#,
        )
        .unwrap();
    }

    fn write_contacts(root: &Path) {
        let all_dir = root.join("Takeout/Contacts/All Contacts");
        let my_dir = root.join("Takeout/Contacts/My Contacts");
        let starred_dir = root.join("Takeout/Contacts/Starred in Android");
        fs::create_dir_all(&all_dir).unwrap();
        fs::create_dir_all(&my_dir).unwrap();
        fs::create_dir_all(&starred_dir).unwrap();

        let all = synthetic_vcards_with_and_without_uid();
        fs::write(all_dir.join("All Contacts.vcf"), &all).unwrap();
        fs::write(my_dir.join("My Contacts.vcf"), &all).unwrap();
        fs::write(starred_dir.join("Starred in Android.vcf"), "").unwrap();
        fs::write(
            all_dir.join("Alice Example.jpg"),
            [0xff_u8, 0xd8, 0xff, 0xe0],
        )
        .unwrap();
        fs::write(
            my_dir.join("Alice Example.jpg"),
            [0xff_u8, 0xd8, 0xff, 0xe0],
        )
        .unwrap();
    }

    fn write_noise_files(root: &Path) {
        fs::write(root.join("Takeout/archive_browser.html"), b"<html/>").unwrap();
        fs::write(root.join("Takeout/user-generated-memory.json"), b"{}").unwrap();
    }
}

fn takeout_message(n: u64, labels: &str, subject: &str) -> String {
    format!(
        "From {n}@xxx Thu Nov 13 20:20:33 +0000 2025\n\
         X-GM-THRID: {n}\n\
         X-Gmail-Labels: {labels}\n\
         Message-ID: <synthetic-{n}@example.com>\n\
         From: \"Anon\" <anon@example.com>\n\
         To: \"Anon\" <anon@example.com>\n\
         Subject: {subject}\n\
         Date: Thu, 13 Nov 2025 20:20:33 +0000\n\
         Content-Type: text/plain; charset=us-ascii\n\
         \n\
         Synthetic body for message {n}.\n\
         \n"
    )
}

fn synthetic_vcards_with_and_without_uid() -> String {
    let mut buf = String::new();
    buf.push_str(
        "BEGIN:VCARD\r\n\
         VERSION:3.0\r\n\
         UID:synthetic-uid-1\r\n\
         FN:Alice Example\r\n\
         N:Example;Alice;;;\r\n\
         EMAIL;TYPE=INTERNET:alice@example.com\r\n\
         TEL;TYPE=CELL:+1-555-0100\r\n\
         END:VCARD\r\n",
    );
    buf.push_str(
        "BEGIN:VCARD\r\n\
         VERSION:3.0\r\n\
         UID:synthetic-uid-2\r\n\
         FN:Bob Example\r\n\
         N:Example;Bob;;;\r\n\
         EMAIL;TYPE=INTERNET:bob@example.com\r\n\
         END:VCARD\r\n",
    );
    buf.push_str(
        "BEGIN:VCARD\r\n\
         VERSION:3.0\r\n\
         FN:Carol Example\r\n\
         N:Example;Carol;;;\r\n\
         EMAIL;TYPE=INTERNET:carol@example.com\r\n\
         END:VCARD\r\n",
    );
    buf.push_str(
        "BEGIN:VCARD\r\n\
         VERSION:3.0\r\n\
         item1.EMAIL;TYPE=INTERNET:dave@example.com\r\n\
         item1.X-ABLabel:\r\n\
         END:VCARD\r\n",
    );
    buf
}

#[test]
fn empty_directory_fails_with_usage() {
    let td = TempDir::new().unwrap();
    let archive = td.path().join("a.sqlite");
    let err = run(common_for(&archive), config_for(td.path().to_path_buf())).unwrap_err();
    assert!(format!("{err}").contains("no .mbox"));
    assert!(matches!(err, vandelay::error::Error::Usage(_)));
    assert_eq!(err.exit_code(), 1);
}

#[test]
fn nonexistent_path_fails_with_usage() {
    let td = TempDir::new().unwrap();
    let archive = td.path().join("a.sqlite");
    let missing = td.path().join("does-not-exist");
    let err = run(common_for(&archive), config_for(missing)).unwrap_err();
    assert!(matches!(err, vandelay::error::Error::Usage(_)));
    assert_eq!(err.exit_code(), 1);
}

#[test]
fn synthetic_takeout_imports_expected_row_counts() {
    let td = TempDir::new().unwrap();
    SyntheticTakeout::build(td.path());
    let archive = td.path().join("a.sqlite");
    let summary = run(common_for(&archive), config_for(td.path().to_path_buf())).unwrap();

    let conn = Connection::open(&archive).unwrap();
    assert_eq!(count(&conn, "emails"), 9);
    assert_eq!(count(&conn, "calendars"), 2);
    assert!(count(&conn, "calendar_events") >= 2);
    assert_eq!(count(&conn, "address_books"), 1);
    assert_eq!(
        count(&conn, "contact_cards"),
        4,
        "2 UID-bearing cards dedup across the two .vcf files (=2); \
         2 UID-less cards dedup by content across the two files (=2); total 4"
    );

    let by = |t: &str| {
        summary
            .per_type
            .iter()
            .find(|(n, _)| *n == t)
            .unwrap()
            .1
            .clone()
    };
    assert!(by("mailbox").created >= 6);
    assert_eq!(by("email").created, 9);
    assert_eq!(by("email").failed, 0);
    assert_eq!(by("calendarevent").failed, 0);
    assert_eq!(by("contactcard").failed, 0);
}

#[test]
fn label_mapping_produces_expected_mailbox_tree() {
    let td = TempDir::new().unwrap();
    SyntheticTakeout::build(td.path());
    let archive = td.path().join("a.sqlite");
    run(common_for(&archive), config_for(td.path().to_path_buf())).unwrap();

    let conn = Connection::open(&archive).unwrap();
    let names = mailbox_names(&conn);
    for required in [
        "Inbox",
        "Sent",
        "Archive",
        "Trash",
        "WorkProject",
        "2026-Q1",
        "Github",
    ] {
        assert!(
            names.contains(required),
            "missing mailbox {required:?}: have {names:?}"
        );
    }
    assert!(
        names.iter().all(|n| !n.starts_with("Category")),
        "Category tokens must be dropped: {names:?}"
    );
    assert!(
        !names.contains("Starred"),
        "Starred is a keyword, not a mailbox: {names:?}"
    );
    assert!(
        !names.contains("Important"),
        "Important is a keyword, not a mailbox: {names:?}"
    );

    let nested_parent_id: i64 = conn
        .query_row(
            "SELECT parent_id FROM mailboxes WHERE name = '2026-Q1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let parent_name: String = conn
        .query_row(
            "SELECT name FROM mailboxes WHERE id = ?1",
            [nested_parent_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(parent_name, "WorkProject");
}

#[test]
fn system_roles_assigned_when_automap_on() {
    let td = TempDir::new().unwrap();
    SyntheticTakeout::build(td.path());
    let archive = td.path().join("a.sqlite");
    run(common_for(&archive), config_for(td.path().to_path_buf())).unwrap();

    let conn = Connection::open(&archive).unwrap();
    let expected = [
        ("Inbox", "inbox"),
        ("Sent", "sent"),
        ("Trash", "trash"),
        ("Archive", "archive"),
    ];
    for (name, role) in expected {
        let got: String = conn
            .query_row("SELECT role FROM mailboxes WHERE name = ?1", [name], |r| {
                r.get(0)
            })
            .unwrap_or_else(|_| panic!("mailbox {name} missing"));
        assert_eq!(got, role, "{name} should have role {role}");
    }
    let custom_role: Option<String> = conn
        .query_row(
            "SELECT role FROM mailboxes WHERE name = 'WorkProject'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(custom_role.is_none());
}

#[test]
fn keyword_translation_matches_label_table() {
    let td = TempDir::new().unwrap();
    SyntheticTakeout::build(td.path());
    let archive = td.path().join("a.sqlite");
    run(common_for(&archive), config_for(td.path().to_path_buf())).unwrap();

    let conn = Connection::open(&archive).unwrap();
    let starred: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM emails WHERE keywords LIKE '%$flagged%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(starred, 1);
    let important: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM emails WHERE keywords LIKE '%$important%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(important, 1);
    let seen: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM emails WHERE keywords LIKE '%$seen%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(seen, 8);
    let unseen: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM emails WHERE keywords NOT LIKE '%$seen%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(unseen, 1);
}

#[test]
fn calendar_xwr_calname_honored_else_falls_back_to_imported() {
    let td = TempDir::new().unwrap();
    SyntheticTakeout::build(td.path());
    let archive = td.path().join("a.sqlite");
    run(common_for(&archive), config_for(td.path().to_path_buf())).unwrap();

    let conn = Connection::open(&archive).unwrap();
    let names: HashSet<String> = {
        let mut s = conn.prepare("SELECT name FROM calendars").unwrap();
        s.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    assert!(
        names.contains("weekly"),
        "weekly from icals/000.ics: {names:?}"
    );
    assert!(
        names.contains("Imported"),
        "Imported fallback for icals/002.ics: {names:?}"
    );
}

#[test]
fn contacts_dedup_by_uid_across_two_vcf_files() {
    let td = TempDir::new().unwrap();
    SyntheticTakeout::build(td.path());
    let archive = td.path().join("a.sqlite");
    run(common_for(&archive), config_for(td.path().to_path_buf())).unwrap();

    let conn = Connection::open(&archive).unwrap();
    assert_eq!(count(&conn, "address_books"), 1);
    let with_uid: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM contact_cards WHERE uid IN ('synthetic-uid-1', 'synthetic-uid-2')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(with_uid, 2);
    let synthetic_uids: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM contact_cards WHERE uid LIKE 'vandelay-syn-%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        synthetic_uids, 2,
        "the 2 UID-less cards appear in both files but are stored once each"
    );
}

#[test]
fn rerun_against_unchanged_source_is_idempotent() {
    let td = TempDir::new().unwrap();
    SyntheticTakeout::build(td.path());
    let archive = td.path().join("a.sqlite");
    run(common_for(&archive), config_for(td.path().to_path_buf())).unwrap();

    let s2 = run(common_for(&archive), config_for(td.path().to_path_buf())).unwrap();
    let email_counts = &s2.per_type.iter().find(|(n, _)| *n == "email").unwrap().1;
    assert_eq!(email_counts.created, 0);
    assert_eq!(email_counts.failed, 0);
    assert_eq!(email_counts.skipped, 9);

    let event_counts = &s2
        .per_type
        .iter()
        .find(|(n, _)| *n == "calendarevent")
        .unwrap()
        .1;
    assert_eq!(event_counts.created, 0);
    assert_eq!(event_counts.failed, 0);
    assert!(event_counts.skipped > 0);

    let card_counts = &s2
        .per_type
        .iter()
        .find(|(n, _)| *n == "contactcard")
        .unwrap()
        .1;
    assert_eq!(card_counts.created, 0);
    assert_eq!(card_counts.failed, 0);
    assert!(card_counts.skipped > 0);
}

#[test]
fn message_without_xgmail_labels_lands_in_filename_mailbox() {
    let td = TempDir::new().unwrap();
    let mail_dir = td.path().join("Takeout/Mail");
    fs::create_dir_all(&mail_dir).unwrap();
    fs::write(
        mail_dir.join("ImportedFromThunderbird.mbox"),
        b"From a@b Thu Nov 13 20:20:33 +0000 2025\nFrom: a@b\nSubject: nolabels\n\nbody\n",
    )
    .unwrap();
    let archive = td.path().join("a.sqlite");
    run(common_for(&archive), config_for(td.path().to_path_buf())).unwrap();
    let conn = Connection::open(&archive).unwrap();
    assert!(mailbox_names(&conn).contains("ImportedFromThunderbird"));
}

#[test]
fn nonmatching_siblings_are_ignored_silently() {
    let td = TempDir::new().unwrap();
    SyntheticTakeout::build(td.path());
    let archive = td.path().join("a.sqlite");
    let summary = run(common_for(&archive), config_for(td.path().to_path_buf())).unwrap();
    assert_eq!(
        summary
            .per_type
            .iter()
            .find(|(n, _)| *n == "email")
            .unwrap()
            .1
            .failed,
        0,
        "the .json / .html / .jpg sidecars must not cause failures"
    );
}

#[test]
fn source_change_protection_refuses_different_path() {
    let td = TempDir::new().unwrap();
    let a = td.path().join("a");
    let b = td.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    SyntheticTakeout::build(&a);
    SyntheticTakeout::build(&b);

    let archive = td.path().join("a.sqlite");
    run(common_for(&archive), config_for(a.clone())).unwrap();
    let err = run(common_for(&archive), config_for(b.clone())).unwrap_err();
    assert!(matches!(err, vandelay::error::Error::SourceChange(_)));

    let mut cfg2 = config_for(b);
    cfg2.allow_source_change = true;
    run(common_for(&archive), cfg2).expect("--allow-source-change permits");
}

#[test]
fn re_extraction_to_same_canonical_path_converges() {
    let td = TempDir::new().unwrap();
    SyntheticTakeout::build(td.path());
    let archive = td.path().join("a.sqlite");
    run(common_for(&archive), config_for(td.path().to_path_buf())).unwrap();
    let initial: i64 = {
        let c = Connection::open(&archive).unwrap();
        count(&c, "emails")
    };

    fs::remove_dir_all(td.path().join("Takeout")).unwrap();
    SyntheticTakeout::build(td.path());

    run(common_for(&archive), config_for(td.path().to_path_buf())).unwrap();
    let c = Connection::open(&archive).unwrap();
    assert_eq!(count(&c, "emails"), initial);
}

#[test]
fn noautomap_disables_role_assignment() {
    let td = TempDir::new().unwrap();
    SyntheticTakeout::build(td.path());
    let archive = td.path().join("a.sqlite");
    let mut cfg = config_for(td.path().to_path_buf());
    cfg.automap = false;
    run(common_for(&archive), cfg).unwrap();
    let conn = Connection::open(&archive).unwrap();
    let with_role: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM mailboxes WHERE role IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        with_role, 0,
        "no mailbox should have a role under --noautomap"
    );
}

#[test]
fn synthetic_takeout_layout_mirrors_real_takeout_directory_shape() {
    let td = TempDir::new().unwrap();
    let synth = SyntheticTakeout::build(td.path());
    let root = synth.path();
    assert!(
        root.join("Takeout/Mail/All Mail Including Spam and Trash.mbox")
            .exists()
    );
    assert!(root.join("Takeout/Mail/Github.mbox").exists());
    assert!(root.join("Takeout/Calendar/Weekly.ics").exists());
    assert!(root.join("Takeout/Calendar/no_calname.ics").exists());
    assert!(root.join("Takeout/Calendar/meet_settings.json").exists());
    assert!(
        root.join("Takeout/Contacts/All Contacts/All Contacts.vcf")
            .exists()
    );
    assert!(
        root.join("Takeout/Contacts/All Contacts/Alice Example.jpg")
            .exists()
    );
    assert!(
        root.join("Takeout/Contacts/My Contacts/My Contacts.vcf")
            .exists()
    );
    assert!(
        root.join("Takeout/Contacts/Starred in Android/Starred in Android.vcf")
            .exists()
    );
    assert!(root.join("Takeout/archive_browser.html").exists());
    assert!(root.join("Takeout/user-generated-memory.json").exists());
}

#[test]
fn dry_run_creates_no_object_rows_in_archive() {
    let td = TempDir::new().unwrap();
    SyntheticTakeout::build(td.path());
    let archive = td.path().join("a.sqlite");
    let mut common = common_for(&archive);
    common.dry_run = true;
    let summary = run(common, config_for(td.path().to_path_buf())).unwrap();

    let conn = Connection::open(&archive).unwrap();
    assert_eq!(count(&conn, "emails"), 0);
    assert_eq!(count(&conn, "mailboxes"), 0);
    assert_eq!(count(&conn, "calendars"), 0);
    assert_eq!(count(&conn, "calendar_events"), 0);
    assert_eq!(count(&conn, "address_books"), 0);
    assert_eq!(count(&conn, "contact_cards"), 0);
    assert_eq!(count(&conn, "blobs"), 0);
    for (_, counts) in &summary.per_type {
        assert_eq!(counts.created, 0);
        assert_eq!(counts.fetched, 0);
        assert_eq!(counts.failed, 0);
    }
}

#[test]
fn byte_identical_message_in_two_mbox_files_dedupes_to_one_row() {
    let td = TempDir::new().unwrap();
    let mail_dir = td.path().join("Takeout/Mail");
    fs::create_dir_all(&mail_dir).unwrap();
    let body = takeout_message(1, "Inbox,Opened,Github", "shared subject");
    fs::write(mail_dir.join("All.mbox"), &body).unwrap();
    fs::write(mail_dir.join("Github.mbox"), &body).unwrap();

    let archive = td.path().join("a.sqlite");
    let summary = run(common_for(&archive), config_for(td.path().to_path_buf())).unwrap();

    let conn = Connection::open(&archive).unwrap();
    assert_eq!(
        count(&conn, "emails"),
        1,
        "the same bytes across two .mbox files must dedupe to one emails row"
    );
    assert_eq!(count(&conn, "blobs"), 1, "blob layer dedupes too");
    let email = &summary
        .per_type
        .iter()
        .find(|(n, _)| *n == "email")
        .unwrap()
        .1;
    assert_eq!(email.created, 1);
    assert!(
        email.skipped + email.fetched >= 1,
        "the second occurrence must go through the present-row path"
    );
}

#[test]
fn two_ics_files_without_calname_merge_into_single_imported_calendar() {
    let td = TempDir::new().unwrap();
    let cal_dir = td.path().join("Takeout/Calendar");
    fs::create_dir_all(&cal_dir).unwrap();
    fs::copy(
        Path::new(RESOURCES).join("icals/002.ics"),
        cal_dir.join("first.ics"),
    )
    .unwrap();
    fs::copy(
        Path::new(RESOURCES).join("icals/002.ics"),
        cal_dir.join("second.ics"),
    )
    .unwrap();
    let archive = td.path().join("a.sqlite");
    run(common_for(&archive), config_for(td.path().to_path_buf())).unwrap();

    let conn = Connection::open(&archive).unwrap();
    let imported_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM calendars WHERE name = 'Imported'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        imported_count, 1,
        "two .ics files without X-WR-CALNAME must share one Imported calendar"
    );
}

#[test]
fn empty_mbox_file_imports_with_no_failures() {
    let td = TempDir::new().unwrap();
    let mail_dir = td.path().join("Takeout/Mail");
    fs::create_dir_all(&mail_dir).unwrap();
    fs::write(mail_dir.join("Empty.mbox"), b"").unwrap();
    let archive = td.path().join("a.sqlite");
    let summary = run(common_for(&archive), config_for(td.path().to_path_buf())).unwrap();
    let email = &summary
        .per_type
        .iter()
        .find(|(n, _)| *n == "email")
        .unwrap()
        .1;
    assert_eq!(email.created, 0);
    assert_eq!(email.failed, 0);
    let conn = Connection::open(&archive).unwrap();
    assert_eq!(count(&conn, "emails"), 0);
    assert_eq!(count(&conn, "mailboxes"), 0);
}

#[test]
fn chat_label_creates_chat_mailbox_with_no_role() {
    let td = TempDir::new().unwrap();
    let mail_dir = td.path().join("Takeout/Mail");
    fs::create_dir_all(&mail_dir).unwrap();
    fs::write(
        mail_dir.join("All.mbox"),
        takeout_message(1, "Chat,Opened", "hello"),
    )
    .unwrap();
    let archive = td.path().join("a.sqlite");
    run(common_for(&archive), config_for(td.path().to_path_buf())).unwrap();
    let conn = Connection::open(&archive).unwrap();
    assert!(mailbox_names(&conn).contains("Chat"));
    let role: Option<String> = conn
        .query_row("SELECT role FROM mailboxes WHERE name = 'Chat'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(role.is_none(), "Chat is a system label with no JMAP role");
}

#[test]
fn noautomap_leaves_nested_mailboxes_role_null() {
    let td = TempDir::new().unwrap();
    let mail_dir = td.path().join("Takeout/Mail");
    fs::create_dir_all(&mail_dir).unwrap();
    fs::write(
        mail_dir.join("All.mbox"),
        takeout_message(1, "Inbox,Opened,Project/2026-Q1", "nested"),
    )
    .unwrap();
    let archive = td.path().join("a.sqlite");
    let mut cfg = config_for(td.path().to_path_buf());
    cfg.automap = false;
    run(common_for(&archive), cfg).unwrap();
    let conn = Connection::open(&archive).unwrap();
    let with_role: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM mailboxes WHERE role IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(with_role, 0);
    let names = mailbox_names(&conn);
    assert!(names.contains("Inbox"));
    assert!(names.contains("Project"));
    assert!(names.contains("2026-Q1"));
}
