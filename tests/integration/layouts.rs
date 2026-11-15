/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashSet;
use std::path::Path;

use rusqlite::Connection;
use serde_json::Value;
use vandelay::archive::sources::SourceChange;
use vandelay::error::Error;
use vandelay::imap::automap::Automap;
use vandelay::imap::session::Compress;
use vandelay::import::imap::{DeletedMessages, FolderScope, ImapImport, MimeKeywords, SaslAuth};
use vandelay::net::stream::Cleartext;
use vandelay::sync::Summary;

use super::imap_client::{ImapSeed, MailboxSeed, SeedTag};
use super::validate::{
    all_contact_uids, all_event_uids, assert_message_round_trip, blob_bytes, blob_id_for_hash,
    calendar_event_count, cleanup, collection_id_by_name, collection_names, common,
    contact_card_count, count, emails_in_mailbox, keywords_for_blob, mailbox_id_by_name,
    mailbox_id_by_path, mailbox_path, open_archive, tmp_archive,
};
use super::{Account, Endpoint};

#[derive(Debug, Clone, Copy)]
pub struct MailboxSpec {
    pub key: &'static str,
    pub name: &'static str,
    pub parent: Option<&'static str>,
}

#[derive(Debug, Clone, Copy)]
pub struct FileSpec {
    pub key: &'static str,
    pub name: &'static str,
    pub parent: Option<&'static str>,
    pub directory: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct SieveScript {
    pub name: &'static str,
    pub body: &'static str,
    pub active: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct Layout {
    pub mailboxes: &'static [MailboxSpec],
    pub files: &'static [FileSpec],
    pub calendars: &'static [&'static str],
    pub address_books: &'static [&'static str],
    pub sieve_scripts: &'static [SieveScript],
    pub email_count: usize,
    pub contact_count: usize,
    pub event_count: usize,
}

macro_rules! mailbox {
    ($k:expr, $n:expr, $p:expr) => {
        MailboxSpec {
            key: $k,
            name: $n,
            parent: $p,
        }
    };
}

macro_rules! node {
    ($k:expr, $n:expr, $p:expr, $d:expr) => {
        FileSpec {
            key: $k,
            name: $n,
            parent: $p,
            directory: $d,
        }
    };
}

pub const ACCOUNT1: &str = "user1";
pub const ACCOUNT2: &str = "user2";
pub const ACCOUNT3: &str = "user3";

pub const PASSWORD: &str = "VandelayUser#2026";

pub static MAILBOXES_DEEP: &[MailboxSpec] = &[
    mailbox!("proj", "Projects", None),
    mailbox!("y2024", "2024", Some("proj")),
    mailbox!("q1", "Q1", Some("y2024")),
    mailbox!("acme", "ClientAcme", Some("q1")),
    mailbox!("acme_in", "Internal", Some("acme")),
    mailbox!("q2", "Q2", Some("y2024")),
    mailbox!("globex", "ClientGlobex", Some("q2")),
    mailbox!("y2025", "2025", Some("proj")),
    mailbox!("alpha", "Alpha", Some("y2025")),
    mailbox!("beta", "Beta", Some("y2025")),
    mailbox!("travel", "Travel", None),
    mailbox!("asia", "Asia", Some("travel")),
    mailbox!("jp", "Japan", Some("asia")),
    mailbox!("tokyo", "Tokyo", Some("jp")),
    mailbox!("eu", "Europe", Some("travel")),
    mailbox!("it", "Italy", Some("eu")),
    mailbox!("rome", "Rome", Some("it")),
    mailbox!("arch", "Archive", None),
    mailbox!("arch23", "2023", Some("arch")),
    mailbox!("arch24", "2024", Some("arch")),
];

pub static MAILBOXES_MEDIUM: &[MailboxSpec] = &[
    mailbox!("work", "Work", None),
    mailbox!("clients", "Clients", Some("work")),
    mailbox!("acme", "AcmeCorp", Some("clients")),
    mailbox!("inv", "Invoices", Some("acme")),
    mailbox!("globex", "Globex", Some("clients")),
    mailbox!("internal", "Internal", Some("work")),
    mailbox!("pers", "Personal", None),
    mailbox!("rcpt", "Receipts", Some("pers")),
];

pub static MAILBOXES_FLAT: &[MailboxSpec] = &[
    mailbox!("lists", "Lists", None),
    mailbox!("news", "Newsletters", Some("lists")),
    mailbox!("misc", "Misc", None),
];

pub static FILES_DEEP: &[FileSpec] = &[
    node!("docs", "Documents", None, true),
    node!("rep", "Reports", Some("docs"), true),
    node!("d24", "2024", Some("rep"), true),
    node!("q1f", "q1-results.bin", Some("d24"), false),
    node!("q2f", "q2-results.bin", Some("d24"), false),
    node!("d25", "2025", Some("rep"), true),
    node!("q1f25", "q1-results.bin", Some("d25"), false),
    node!("spec", "Specs", Some("docs"), true),
    node!("ovr", "overview.bin", Some("spec"), false),
    node!("notes", "notes.bin", Some("docs"), false),
    node!("photos", "Photos", None, true),
    node!("vac", "Vacations", Some("photos"), true),
    node!("vit", "Italy", Some("vac"), true),
    node!("vrome", "rome.bin", Some("vit"), false),
    node!("vjp", "Japan", Some("vac"), true),
    node!("vtokyo", "tokyo.bin", Some("vjp"), false),
    node!("readme", "README.bin", None, false),
];

pub static FILES_MEDIUM: &[FileSpec] = &[
    node!("media", "Media", None, true),
    node!("audio", "Audio", Some("media"), true),
    node!("song", "song.bin", Some("audio"), false),
    node!("video", "Video", Some("media"), true),
    node!("clip", "clip.bin", Some("video"), false),
    node!("proj", "Projects", None, true),
    node!("src", "src", Some("proj"), true),
    node!("main", "main.bin", Some("src"), false),
];

pub static FILES_FLAT: &[FileSpec] = &[
    node!("bak", "Backup", None, true),
    node!("snap", "snapshot.bin", Some("bak"), false),
    node!("scratch", "scratch.bin", None, false),
];

pub static CALENDARS_DEEP: &[&str] = &["Personal", "Work", "Birthdays", "Travel"];
pub static CALENDARS_MEDIUM: &[&str] = &["Personal", "Work"];
pub static CALENDARS_FLAT: &[&str] = &["Personal", "Holidays"];

pub static ADDRESS_BOOKS_DEEP: &[&str] = &["Personal", "Work", "Friends"];
pub static ADDRESS_BOOKS_MEDIUM: &[&str] = &["Personal", "Work"];
pub static ADDRESS_BOOKS_FLAT: &[&str] = &["Personal", "Family"];

pub static SIEVE_DEEP: &[SieveScript] = &[
    SieveScript {
        name: "spam",
        body: "require [\"fileinto\"];\nif header :contains \"X-Spam\" \"YES\" {\n  fileinto \"Junk\";\n  stop;\n}\n",
        active: true,
    },
    SieveScript {
        name: "vacation",
        body: "require [\"vacation\"];\nvacation :days 7 :subject \"Out of office\" \"I am away, will reply later.\";\n",
        active: false,
    },
    SieveScript {
        name: "organize",
        body: "require [\"fileinto\",\"mailbox\"];\nif address :is \"from\" \"newsletter@example.com\" {\n  fileinto :create \"Projects/2025/Beta\";\n}\n",
        active: false,
    },
];

pub static SIEVE_MEDIUM: &[SieveScript] = &[
    SieveScript {
        name: "filter",
        body: "require [\"fileinto\"];\nif address :is \"to\" \"work@example.com\" {\n  fileinto \"Work\";\n}\n",
        active: true,
    },
    SieveScript {
        name: "draft",
        body: "require [\"reject\"];\nif address :is \"from\" \"blocked@example.com\" {\n  reject \"go away\";\n}\n",
        active: false,
    },
];

pub static SIEVE_FLAT: &[SieveScript] = &[];

pub fn layout_for(username: &str) -> Layout {
    match username {
        s if s == ACCOUNT1 => Layout {
            mailboxes: MAILBOXES_DEEP,
            files: FILES_DEEP,
            calendars: CALENDARS_DEEP,
            address_books: ADDRESS_BOOKS_DEEP,
            sieve_scripts: SIEVE_DEEP,
            email_count: 200,
            contact_count: 40,
            event_count: 40,
        },
        s if s == ACCOUNT2 => Layout {
            mailboxes: MAILBOXES_MEDIUM,
            files: FILES_MEDIUM,
            calendars: CALENDARS_MEDIUM,
            address_books: ADDRESS_BOOKS_MEDIUM,
            sieve_scripts: SIEVE_MEDIUM,
            email_count: 80,
            contact_count: 15,
            event_count: 15,
        },
        _ => Layout {
            mailboxes: MAILBOXES_FLAT,
            files: FILES_FLAT,
            calendars: CALENDARS_FLAT,
            address_books: ADDRESS_BOOKS_FLAT,
            sieve_scripts: SIEVE_FLAT,
            email_count: 20,
            contact_count: 10,
            event_count: 10,
        },
    }
}

pub fn accounts() -> [&'static str; 3] {
    [ACCOUNT1, ACCOUNT2, ACCOUNT3]
}

pub fn collection_segment(prefix: &str, idx: usize, name: &str) -> String {
    let mut out = format!("{prefix}-{:02}-", idx);
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_owned()
}

pub fn events_per_calendar(layout: &Layout, idx: usize) -> usize {
    if layout.calendars.is_empty() {
        return 0;
    }
    let base = layout.event_count / layout.calendars.len().max(1);
    let extra = if idx == 0 {
        layout.event_count % layout.calendars.len()
    } else {
        0
    };
    base + extra
}

pub fn events_offset(layout: &Layout, idx: usize) -> usize {
    (0..idx).map(|i| events_per_calendar(layout, i)).sum()
}

pub fn contacts_per_book(layout: &Layout, idx: usize) -> usize {
    if layout.address_books.is_empty() {
        return 0;
    }
    let base = layout.contact_count / layout.address_books.len().max(1);
    let extra = if idx == 0 {
        layout.contact_count % layout.address_books.len()
    } else {
        0
    };
    base + extra
}

pub fn contacts_offset(layout: &Layout, idx: usize) -> usize {
    (0..idx).map(|i| contacts_per_book(layout, i)).sum()
}

#[derive(Debug, Clone)]
pub struct SeededItem {
    pub uid: String,
    pub href: String,
    pub source: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct CollectionPlan {
    pub display_name: String,
    pub base_href: String,
    pub items: Vec<SeededItem>,
}

impl CollectionPlan {
    pub fn new(display_name: String, base_href: String) -> Self {
        Self {
            display_name,
            base_href,
            items: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AccountSeed {
    pub username: String,
    pub calendars: Vec<CollectionPlan>,
    pub address_books: Vec<CollectionPlan>,
}

impl AccountSeed {
    pub fn new(username: String) -> Self {
        Self {
            username,
            calendars: Vec::new(),
            address_books: Vec::new(),
        }
    }

    pub fn total_events(&self) -> usize {
        self.calendars.iter().map(|c| c.items.len()).sum()
    }

    pub fn total_contacts(&self) -> usize {
        self.address_books.iter().map(|c| c.items.len()).sum()
    }

    pub fn event_uids(&self) -> HashSet<String> {
        self.calendars
            .iter()
            .flat_map(|c| c.items.iter().map(|i| i.uid.clone()))
            .collect()
    }

    pub fn contact_uids(&self) -> HashSet<String> {
        self.address_books
            .iter()
            .flat_map(|c| c.items.iter().map(|i| i.uid.clone()))
            .collect()
    }
}

pub fn assert_event_collection_exact(conn: &Connection, seed: &AccountSeed) {
    let names = collection_names(conn, "calendars");
    let expected: HashSet<String> = seed
        .calendars
        .iter()
        .map(|c| c.display_name.clone())
        .collect();
    assert_eq!(
        names, expected,
        "{}: calendar displayname set mismatch",
        seed.username
    );
    for plan in &seed.calendars {
        let cal_id = collection_id_by_name(conn, "calendars", &plan.display_name)
            .unwrap_or_else(|| panic!("calendar {} missing", plan.display_name));
        let got = calendar_event_count(conn, cal_id);
        assert_eq!(
            got as usize,
            plan.items.len(),
            "{}: calendar {} event count {got} != seeded {}",
            seed.username,
            plan.display_name,
            plan.items.len()
        );
    }
}

pub fn assert_contact_collection_exact(conn: &Connection, seed: &AccountSeed) {
    let names = collection_names(conn, "address_books");
    let expected: HashSet<String> = seed
        .address_books
        .iter()
        .map(|c| c.display_name.clone())
        .collect();
    assert_eq!(
        names, expected,
        "{}: address book displayname set mismatch",
        seed.username
    );
    for plan in &seed.address_books {
        let book_id = collection_id_by_name(conn, "address_books", &plan.display_name)
            .unwrap_or_else(|| panic!("address book {} missing", plan.display_name));
        let got = contact_card_count(conn, book_id);
        assert_eq!(
            got as usize,
            plan.items.len(),
            "{}: book {} contact count {got} != seeded {}",
            seed.username,
            plan.display_name,
            plan.items.len()
        );
    }
}

pub fn assert_event_uids_round_trip(conn: &Connection, seed: &AccountSeed) {
    let imported = all_event_uids(conn);
    let expected = seed.event_uids();
    let missing: Vec<_> = expected.difference(&imported).collect();
    assert!(
        missing.is_empty(),
        "{}: event uids missing from archive: {missing:?}",
        seed.username
    );
}

pub fn assert_contact_uids_round_trip(conn: &Connection, seed: &AccountSeed) {
    let imported = all_contact_uids(conn);
    let expected = seed.contact_uids();
    let missing: Vec<_> = expected.difference(&imported).collect();
    assert!(
        missing.is_empty(),
        "{}: contact uids missing from archive: {missing:?}",
        seed.username
    );
}

pub fn assert_event_data_well_formed(conn: &Connection) {
    let mut stmt = conn
        .prepare("SELECT data, data_type FROM calendar_events")
        .expect("prepare");
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .expect("query")
        .filter_map(|r| r.ok())
        .collect();
    assert!(!rows.is_empty(), "no calendar_events rows");
    for (raw, data_type) in rows {
        assert_eq!(
            data_type, "Event",
            "all VEVENT seeds must land as Event, got data_type={data_type}, raw={raw}"
        );
        let v: Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("calendar_events.data invalid JSON: {e}; raw={raw}"));
        let obj = v.as_object().expect("event data is object");
        let kind = obj
            .get("@type")
            .or_else(|| obj.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        assert_eq!(kind, "Event", "JSCalendar @type must be Event, got {kind}");
        assert!(obj.contains_key("uid"), "event JSON missing uid: {obj:?}");
        assert!(
            obj.contains_key("start"),
            "event JSON missing start: {obj:?}"
        );
    }
}

pub fn assert_contact_data_well_formed(conn: &Connection) {
    let mut stmt = conn
        .prepare("SELECT uid, data FROM contact_cards")
        .expect("prepare");
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .expect("query")
        .filter_map(|r| r.ok())
        .collect();
    assert!(!rows.is_empty(), "no contact_cards rows");
    for (uid, raw) in rows {
        assert!(!uid.is_empty(), "contact uid empty");
        let v: Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("contact_cards.data invalid JSON: {e}; raw={raw}"));
        let obj = v.as_object().expect("contact data is object");
        let kind = obj
            .get("@type")
            .or_else(|| obj.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        assert_eq!(kind, "Card", "JSContact @type must be Card, got {kind}");
    }
}

pub struct ImapSuite<'a> {
    pub server: &'static str,
    pub imap: &'a Endpoint,
    pub cleartext: Cleartext,
}

impl ImapSuite<'_> {
    pub fn config(&self, account: &Account) -> ImapImport {
        ImapImport {
            url: format!("imap://{}:{}", self.imap.host, self.imap.port),
            auth: SaslAuth::Basic {
                user: account.username.clone(),
                password: account.password.clone(),
            },
            cleartext: self.cleartext,
            compress: Compress::Off,
            include: Vec::new(),
            exclude: Vec::new(),
            exclude_special: Vec::new(),
            folder: Vec::new(),
            scope: FolderScope::All,
            automap: Automap::Enabled,
            deleted: DeletedMessages::Skip,
            mime_keywords: MimeKeywords::Skip,
            fetch_batch: 64,
            imap_connections: 2,
            source_change: SourceChange::Reject,
        }
    }

    pub fn import(&self, account: &Account, archive: &Path, what: &str) -> Summary {
        let summary = self
            .config(account)
            .run(common(archive))
            .into_result()
            .unwrap_or_else(|e| panic!("{} {what}: {e:?}", self.server));
        assert!(
            !summary.any_failed(),
            "{} {what} had failures for {}: {summary:?}",
            self.server,
            account.username
        );
        summary
    }

    pub fn assert_seeded(&self, conn: &Connection, account: &Account, seed: &MailboxSeed) {
        let user = &account.username;
        let mailbox_count = count(conn, "mailboxes") as usize;
        let email_count = count(conn, "emails") as usize;
        let expected_mailboxes = account.layout.mailboxes.len() + 1;
        assert_eq!(
            mailbox_count, expected_mailboxes,
            "{user}: imported {mailbox_count} mailboxes, expected exactly {expected_mailboxes}"
        );
        assert_eq!(
            email_count, seed.total_appends,
            "{user}: every seeded email must land in archive (appended {}, imported {email_count})",
            seed.total_appends
        );
        assert!(count(conn, "blobs") > 0, "{user}: blobs interned");

        let bad_match: i64 = conn
            .query_row(
                "SELECT count(*) FROM emails WHERE message_match IS NULL OR message_match = ''",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            bad_match, 0,
            "{user}: every imported email must have a non-empty message_match"
        );

        if let Some(probe) = seed.appends.iter().find(|a| a.tag == SeedTag::FlagProbe) {
            let blob_id = blob_id_for_hash(conn, blake3::hash(&probe.raw).as_bytes())
                .expect("flag probe blob");
            assert_eq!(
                blob_bytes(conn, blob_id),
                probe.raw,
                "{user}: flag probe blob bytes mismatch"
            );
            let keywords = keywords_for_blob(conn, blob_id);
            let kw_set: HashSet<_> = keywords.iter().map(String::as_str).collect();
            for keyword in &probe.flags {
                assert!(
                    kw_set.contains(keyword.as_str()),
                    "{user}: flag probe missing {keyword} in keywords {keywords:?}"
                );
            }
        }

        for path in &seed.paths {
            let id = mailbox_id_by_path(conn, path, '/')
                .unwrap_or_else(|| panic!("{user}: mailbox path {path} missing in archive"));
            assert_eq!(mailbox_path(conn, id, '/'), *path);
            let expected = seed.histogram.get(path).copied().unwrap_or(0);
            let got = emails_in_mailbox(conn, id) as usize;
            assert_eq!(
                got, expected,
                "{user}: mailbox {path} email count {got} != appended {expected}"
            );
        }
        for append in &seed.appends {
            assert_message_round_trip(
                conn,
                &append.raw,
                &append.target,
                &format!("{user}/append/{:?}", append.tag),
            );
        }
        let inbox_role: Option<String> = conn
            .query_row("SELECT role FROM mailboxes WHERE name = 'INBOX'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            inbox_role,
            Some("inbox".to_owned()),
            "{user}: INBOX role tagged"
        );
        let inbox_id = mailbox_id_by_name(conn, "INBOX").expect("INBOX");
        let inbox_seeded = seed.histogram.get("INBOX").copied().unwrap_or(0);
        assert_eq!(
            emails_in_mailbox(conn, inbox_id) as usize,
            inbox_seeded,
            "{user}: INBOX email count mismatch"
        );
    }

    pub fn assert_incremental(&self, account: &Account, archive: &Path) {
        let user = &account.username;
        let conn = open_archive(archive);
        let email_count = count(&conn, "emails");
        let mailbox_count = count(&conn, "mailboxes");
        let blob_count = count(&conn, "blobs");
        drop(conn);

        self.import(account, archive, "idempotent imap re-import");
        let conn = open_archive(archive);
        assert_eq!(
            count(&conn, "emails"),
            email_count,
            "{user}: idempotent re-import added emails"
        );
        assert_eq!(
            count(&conn, "mailboxes"),
            mailbox_count,
            "{user}: idempotent re-import added mailboxes"
        );
        assert_eq!(
            count(&conn, "blobs"),
            blob_count,
            "{user}: idempotent re-import added blobs"
        );
        drop(conn);

        let mut client = ImapSeed::open(self.imap, account).expect("imap login");
        client
            .delete_and_expunge_first("INBOX")
            .expect("expunge first INBOX message");
        client.logout().expect("imap logout");
        self.import(account, archive, "re-import after expunge");
        let conn = open_archive(archive);
        assert_eq!(
            count(&conn, "emails"),
            email_count - 1,
            "{user}: expunged message was not pruned"
        );
        drop(conn);

        let mut client = ImapSeed::open(self.imap, account).expect("imap login");
        let (added_raw, added_mid) = client
            .append_probe(account, "INBOX", "post-import")
            .expect("append new INBOX message");
        client.logout().expect("imap logout");
        self.import(account, archive, "re-import after append");
        let conn = open_archive(archive);
        assert_eq!(
            count(&conn, "emails"),
            email_count,
            "{user}: append-after-expunge should restore email count to baseline"
        );
        let added_blob = blob_id_for_hash(&conn, blake3::hash(&added_raw).as_bytes())
            .expect("added message blob present");
        assert_eq!(
            blob_bytes(&conn, added_blob),
            added_raw,
            "{user}: added message blob bytes mismatch"
        );
        let mid_normalised = added_mid
            .trim_matches(|c| c == '<' || c == '>')
            .to_ascii_lowercase();
        let mid_found: i64 = conn
            .query_row(
                "SELECT count(*) FROM emails
                 WHERE EXISTS (SELECT 1 FROM json_each(json_extract(message_match,'$.m')) j
                               WHERE j.value = ?1)",
                [mid_normalised.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            mid_found >= 1,
            "{user}: added Message-ID {added_mid} not found in message_match"
        );
        let inbox_id = mailbox_id_by_name(&conn, "INBOX").expect("INBOX id");
        let added_in_inbox: bool = conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM emails
                                WHERE blob_id = ?1
                                  AND EXISTS (SELECT 1 FROM json_each(mailbox_ids) j
                                              WHERE j.value = ?2))",
                rusqlite::params![added_blob, inbox_id],
                |r| r.get::<_, i64>(0).map(|n| n != 0),
            )
            .unwrap();
        assert!(
            added_in_inbox,
            "{user}: added message blob not linked to INBOX"
        );
    }

    pub fn assert_source_change(&self, primary: &Account, other: &Account) {
        let shared_archive = tmp_archive(&format!("{}-source-change", self.server));
        self.config(primary)
            .run(common(&shared_archive))
            .into_result()
            .expect("seed archive with primary user");
        let err = self
            .config(other)
            .run(common(&shared_archive))
            .into_result()
            .expect_err("expected source-change abort");
        assert!(
            matches!(err, Error::SourceChange(_)),
            "expected SourceChange, got {err:?}"
        );
        cleanup(&shared_archive);
    }

    pub fn assert_non_ascii_round_trip(&self, account: &Account) {
        let mutf7_name = "Envoy&AOk-s";
        let utf8_name = "Envoyés";
        let message = b"From: accents@vandelay.test\r\n\
             To: user1@vandelay.test\r\n\
             Subject: Accents\r\n\
             Message-ID: <accents-1@vandelay.test>\r\n\
             Date: Wed, 01 Jan 2025 12:00:00 +0000\r\n\
             \r\n\
             Accented folder probe.\r\n";
        let mut client = ImapSeed::open(self.imap, account).expect("imap login");
        client
            .create_with_message(mutf7_name, message)
            .expect("create accented mailbox");
        client.logout().expect("imap logout");

        let archive = tmp_archive(&format!("{}-non-ascii", self.server));
        let summary = self
            .config(account)
            .run(common(&archive))
            .into_result()
            .expect("imap import");
        assert!(
            !summary.any_failed(),
            "an accented folder name must not fail the import: {summary:?}"
        );

        let conn = open_archive(&archive);
        let names: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT name FROM mailboxes ORDER BY name")
                .unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        let mailbox_id = mailbox_id_by_name(&conn, utf8_name).unwrap_or_else(|| {
            panic!("no mailbox named {utf8_name}; names must not be mojibake, got {names:?}")
        });
        assert_eq!(
            emails_in_mailbox(&conn, mailbox_id),
            1,
            "the message inside the accented folder must be imported"
        );
        drop(conn);
        cleanup(&archive);
    }
}
