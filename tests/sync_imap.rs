/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod integration;
mod seeder;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use integration::Account;
use integration::dovecot::Dovecot;
use integration::stalwart::shared as shared_stalwart;
use integration::validate::{emails_in_mailbox, mailbox_id_by_path, mailbox_path, open_archive};
use rusqlite::Connection;
use serde_json::json;

fn base_url() -> &'static str {
    shared_stalwart().base_url()
}

fn imaps_url() -> String {
    let s = shared_stalwart();
    format!("imaps://{}:{}", s.host, s.imaps_port)
}

use vandelay::jmap::account::AccountSelector;
use vandelay::jmap::http::Auth;
use vandelay::logging::Logger;
use vandelay::sync::import_imap::{ImapAuth, ImapImportConfig};
use vandelay::sync::{self, CommonConfig, ConnectConfig, ExportConfig, ImportConfig};

fn tmp_archive(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "vandelay-imap-{tag}-{}-{}.sqlite",
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
        max_retries: 3,
        allow_invalid_certs: true,
        logger: Logger::from_flags(false, 0),
    }
}

fn imap_basic_config(localpart: &str) -> ImapImportConfig {
    ImapImportConfig {
        url: imaps_url(),
        auth: ImapAuth::Basic {
            user: format!("{localpart}@{}", seeder::DOMAIN),
            password: seeder::USER_PASSWORD.to_owned(),
        },
        allow_cleartext: false,
        compress: false,
        include: Vec::new(),
        exclude: Vec::new(),
        exclude_special: Vec::new(),
        folder: Vec::new(),
        subscribed_only: false,
        automap: true,
        include_deleted: false,
        fetch_batch: 256,
        imap_connections: 4,
        allow_source_change: false,
    }
}

fn dovecot_config(account: &Account, imap: &integration::Endpoint) -> ImapImportConfig {
    ImapImportConfig {
        url: format!("imap://{}:{}", imap.host, imap.port),
        auth: ImapAuth::Basic {
            user: account.username.clone(),
            password: account.password.clone(),
        },
        allow_cleartext: false,
        compress: false,
        include: Vec::new(),
        exclude: Vec::new(),
        exclude_special: Vec::new(),
        folder: Vec::new(),
        subscribed_only: false,
        automap: true,
        include_deleted: false,
        fetch_batch: 64,
        imap_connections: 2,
        allow_source_change: false,
    }
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn blob_hashes(conn: &Connection) -> HashSet<Vec<u8>> {
    let mut stmt = conn.prepare("SELECT hash FROM blobs").unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0)).unwrap();
    rows.filter_map(|r| r.ok()).collect()
}

#[test]
#[ignore = "requires Docker"]
fn imap_import_test1_lands_mailbox_tree_and_emails() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test1").expect("test1");
    let archive = tmp_archive("test1");
    assert_eq!(fx.domain, seeder::DOMAIN);
    assert!(!fx.domain_id.is_empty());
    assert_eq!(
        fx.admin_login,
        (
            seeder::ADMIN_USER.to_owned(),
            seeder::ADMIN_PASSWORD.to_owned()
        )
    );
    assert!(!acc.admin_role, "test1 is a regular user, not admin");
    if let Some(seeded) = &acc.seeded {
        assert!(seeded.emails > 0, "test1 was seeded with emails");
        assert!(seeded.contacts > 0, "test1 should be seeded with contacts");
        assert!(seeded.events > 0, "test1 should be seeded with events");
        assert!(
            seeded.file_nodes > 0,
            "test1 should be seeded with file nodes"
        );
        assert!(
            seeded.address_books > 0,
            "test1 layout requests an extra address book"
        );
        assert!(
            seeded.calendars > 0,
            "test1 layout requests an extra calendar"
        );
        assert!(seeded.identity, "test1 layout requests a custom identity");
        assert_eq!(
            seeded.sieve_active,
            Some(true),
            "test1 layout activates a sieve script"
        );
    }

    let summary = sync::import_imap::run(common(&archive), imap_basic_config(&acc.localpart))
        .expect("import");
    assert!(!summary.any_failed(), "import had failures: {summary:?}");

    let conn = Connection::open(&archive).unwrap();
    let mailbox_count = count(&conn, "mailboxes") as usize;
    let email_count = count(&conn, "emails") as usize;
    let blob_count = count(&conn, "blobs") as usize;

    let seeded = acc.seeded.as_ref().expect("seed stats");
    assert!(
        mailbox_count >= seeded.mailboxes_created,
        "mailboxes {mailbox_count} should cover the seeded tree ({}) plus Stalwart defaults",
        seeded.mailboxes_created
    );
    assert!(email_count > 0, "at least some emails imported");
    assert!(blob_count > 0, "blobs interned");

    let inbox_role: Option<String> = conn
        .query_row("SELECT role FROM mailboxes WHERE name = 'INBOX'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(inbox_role, Some("inbox".to_owned()));

    let archive_role_exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM mailboxes WHERE role = 'archive')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        archive_role_exists,
        "seeded archive mailbox surfaces with archive role"
    );

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn imap_second_run_is_convergent() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test2").expect("test2");
    let archive = tmp_archive("converge");

    sync::import_imap::run(common(&archive), imap_basic_config(&acc.localpart))
        .expect("first import");

    let summary = sync::import_imap::run(common(&archive), imap_basic_config(&acc.localpart))
        .expect("second import");
    for (name, counts) in &summary.per_type {
        assert_eq!(
            counts.created, 0,
            "{name}: convergent run should create nothing"
        );
        assert_eq!(
            counts.deleted, 0,
            "{name}: convergent run should delete nothing"
        );
    }

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn imap_and_jmap_imports_share_blob_set() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test1").expect("test1");
    let imap_archive = tmp_archive("parity-imap");
    let jmap_archive = tmp_archive("parity-jmap");

    sync::import_imap::run(common(&imap_archive), imap_basic_config(&acc.localpart))
        .expect("imap import");

    let jmap_cfg = ImportConfig {
        connect: ConnectConfig {
            url: fx.base_url.clone(),
            auth: Auth::Basic {
                user: acc.email.clone(),
                password: seeder::USER_PASSWORD.to_owned(),
            },
            account: AccountSelector::Id(acc.account_id.clone()),
        },
        objects: None,
        allow_source_change: false,
    };
    let jmap_common = CommonConfig {
        archive: jmap_archive.clone(),
        threads: 4,
        dry_run: false,
        max_retries: 5,
        allow_invalid_certs: true,
        logger: Logger::from_flags(false, 0),
    };
    sync::import_jmap::run(jmap_common, jmap_cfg).expect("jmap import");

    let imap_conn = Connection::open(&imap_archive).unwrap();
    let jmap_conn = Connection::open(&jmap_archive).unwrap();

    let imap_blobs = blob_hashes(&imap_conn);
    let jmap_blobs = blob_hashes(&jmap_conn);

    let missing: Vec<&Vec<u8>> = imap_blobs.difference(&jmap_blobs).collect();
    assert!(
        missing.is_empty(),
        "IMAP archive has {} blobs the JMAP archive lacks; bytes differ between protocols",
        missing.len()
    );

    let imap_emails = count(&imap_conn, "emails");
    let jmap_emails = count(&jmap_conn, "emails");
    assert!(
        imap_emails >= jmap_emails,
        "IMAP saw fewer emails ({imap_emails}) than JMAP ({jmap_emails})"
    );

    let _ = std::fs::remove_file(&imap_archive);
    let _ = std::fs::remove_file(&jmap_archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn imap_import_test3_lands_mailbox_tree_and_emails() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test3").expect("test3");
    let archive = tmp_archive("test3");

    let summary = sync::import_imap::run(common(&archive), imap_basic_config(&acc.localpart))
        .expect("import");
    assert!(!summary.any_failed(), "import had failures: {summary:?}");

    let conn = Connection::open(&archive).unwrap();
    assert!(count(&conn, "mailboxes") > 0);
    assert!(count(&conn, "blobs") >= 0);

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn imap_multi_chunk_metadata_fetch_converges() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test1").expect("test1");
    let archive = tmp_archive("multi_chunk");

    let mut cfg = imap_basic_config(&acc.localpart);
    cfg.fetch_batch = 5;
    let summary = sync::import_imap::run(common(&archive), cfg).expect("import");
    assert!(!summary.any_failed(), "import had failures: {summary:?}");

    let mut cfg2 = imap_basic_config(&acc.localpart);
    cfg2.fetch_batch = 5;
    let second = sync::import_imap::run(common(&archive), cfg2).expect("second");
    for (name, c) in &second.per_type {
        assert_eq!(c.created, 0, "{name}: convergent run creates nothing");
        assert_eq!(c.deleted, 0, "{name}: convergent run deletes nothing");
    }

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn imap_dry_run_reports_new_diff_without_writing() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test2").expect("test2");
    let archive = tmp_archive("dryrun");

    let mut common_cfg = common(&archive);
    common_cfg.dry_run = true;
    let summary =
        sync::import_imap::run(common_cfg, imap_basic_config(&acc.localpart)).expect("dry-run");
    let mailbox = summary
        .per_type
        .iter()
        .find(|(k, _)| *k == "mailbox")
        .expect("mailbox counts");
    assert!(mailbox.1.created > 0, "dry-run reports new mailboxes");

    let conn = Connection::open(&archive).unwrap();
    assert_eq!(
        count(&conn, "mailboxes"),
        0,
        "dry-run must not persist mailboxes"
    );
    assert_eq!(count(&conn, "emails"), 0, "dry-run must not persist emails");
    drop(conn);

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn imap_imported_archive_exports_via_jmap() {
    let fx = seeder::provision(base_url()).expect("provision");
    let src = fx.account("test1").expect("test1");
    let dst = fx.account("test4").expect("test4");
    let archive = tmp_archive("roundtrip");

    sync::import_imap::run(common(&archive), imap_basic_config(&src.localpart))
        .expect("imap import");

    let export_common = CommonConfig {
        archive: archive.clone(),
        threads: 4,
        dry_run: false,
        max_retries: 5,
        allow_invalid_certs: true,
        logger: Logger::from_flags(false, 0),
    };
    let export_cfg = ExportConfig {
        connect: ConnectConfig {
            url: fx.base_url.clone(),
            auth: Auth::Basic {
                user: dst.email.clone(),
                password: seeder::USER_PASSWORD.to_owned(),
            },
            account: AccountSelector::Id(dst.account_id.clone()),
        },
        objects: None,
        prune: false,
        yes: false,
    };
    let first = sync::export::run(export_common, export_cfg).expect("first export");
    assert!(!first.any_failed(), "first export had failures: {first:?}");

    let export_common2 = CommonConfig {
        archive: archive.clone(),
        threads: 4,
        dry_run: false,
        max_retries: 5,
        allow_invalid_certs: true,
        logger: Logger::from_flags(false, 0),
    };
    let export_cfg2 = ExportConfig {
        connect: ConnectConfig {
            url: fx.base_url.clone(),
            auth: Auth::Basic {
                user: dst.email.clone(),
                password: seeder::USER_PASSWORD.to_owned(),
            },
            account: AccountSelector::Id(dst.account_id.clone()),
        },
        objects: None,
        prune: false,
        yes: false,
    };
    let second = sync::export::run(export_common2, export_cfg2).expect("second export");
    for (name, counts) in &second.per_type {
        assert_eq!(
            counts.created, 0,
            "{name}: round-trip second export should create nothing"
        );
    }

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn dovecot_noselect_parents_keep_the_tree_through_export() {
    let d = Dovecot::start().expect("dovecot start");
    let account = d.accounts.first().expect("account").clone();
    let message = b"From: tree@vandelay.test\r\n\
         To: user1@vandelay.test\r\n\
         Subject: Tree\r\n\
         Message-ID: <tree-1@vandelay.test>\r\n\
         Date: Wed, 01 Jan 2025 12:00:00 +0000\r\n\
         \r\n\
         Structural parent probe.\r\n";
    let leaves = [
        "Projects/Invoices",
        "Clients/Acme/Invoices",
        "Archive",
        "Archive/2024",
        "Archive /2024",
    ];
    let structural = ["Projects", "Clients", "Clients/Acme", "Archive "];
    let listed = d
        .create_leaves_without_parents(&account, &leaves, message)
        .expect("create hierarchy");
    for name in structural {
        let quoted = format!("\"{name}\"");
        let line = listed
            .iter()
            .find(|l| l.ends_with(&quoted) || l.ends_with(&format!(" {name}")))
            .unwrap_or_else(|| panic!("{name:?} missing from LIST: {listed:?}"));
        let lower = line.to_ascii_lowercase();
        assert!(
            lower.contains("\\noselect") || lower.contains("\\nonexistent"),
            "{name:?} should be non-selectable: {line}"
        );
    }

    let archive = tmp_archive("dovecot-noselect");
    for pass in 0..2 {
        let summary = sync::import_imap::run(common(&archive), dovecot_config(&account, &d.imap))
            .expect("imap import");
        assert!(!summary.any_failed(), "pass {pass}: {summary:?}");
    }
    let conn = open_archive(&archive);
    for path in leaves.iter().chain(&structural) {
        let id = mailbox_id_by_path(&conn, path, '/')
            .unwrap_or_else(|| panic!("{path:?} not in the archive tree"));
        assert_eq!(mailbox_path(&conn, id, '/'), *path);
    }
    for path in structural {
        let id = mailbox_id_by_path(&conn, path, '/').expect("structural");
        assert_eq!(emails_in_mailbox(&conn, id), 0, "{path:?}");
    }
    for path in leaves {
        let id = mailbox_id_by_path(&conn, path, '/').expect("leaf");
        assert_eq!(emails_in_mailbox(&conn, id), 1, "{path:?}");
    }
    drop(conn);

    let fx = seeder::provision(base_url()).expect("provision");
    let dst = fx.account(seeder::SYNC_OUT[0]).expect("sync-out target");
    let export = sync::export::run(
        CommonConfig {
            archive: archive.clone(),
            threads: 4,
            dry_run: false,
            max_retries: 5,
            allow_invalid_certs: true,
            logger: Logger::from_flags(false, 0),
        },
        ExportConfig {
            connect: ConnectConfig {
                url: fx.base_url.clone(),
                auth: Auth::Basic {
                    user: dst.email.clone(),
                    password: dst.password.clone(),
                },
                account: AccountSelector::Id(dst.account_id.clone()),
            },
            objects: None,
            prune: false,
            yes: true,
        },
    )
    .expect("export");
    assert!(!export.any_failed(), "export reported failures: {export:?}");

    let jmap =
        seeder::jmap::Jmap::connect(&fx.base_url, &dst.email, &dst.password).expect("session");
    let resp = jmap
        .request(
            &["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            json!([["Mailbox/get", {"accountId": dst.account_id, "ids": null}, "g"]]),
        )
        .expect("Mailbox/get");
    let list = resp["methodResponses"][0][1]["list"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let by_id: HashMap<&str, (&str, Option<&str>)> = list
        .iter()
        .filter_map(|m| {
            Some((
                m["id"].as_str()?,
                (m["name"].as_str()?, m["parentId"].as_str()),
            ))
        })
        .collect();
    let paths: HashSet<String> = by_id
        .keys()
        .map(|id| {
            let mut parts = Vec::new();
            let mut cur = Some(*id);
            while let Some(c) = cur {
                let (name, parent) = by_id[c];
                parts.push(name);
                cur = parent;
            }
            parts.reverse();
            parts.join("/")
        })
        .collect();
    for path in leaves
        .iter()
        .chain(&structural)
        .filter(|p| !p.contains(' '))
    {
        assert!(
            paths.contains(*path),
            "{path:?} missing on target: {paths:?}"
        );
    }
    for orphan in ["Invoices", "Acme", "2024"] {
        assert!(
            !paths.contains(orphan),
            "{orphan:?} leaked to the top level"
        );
    }

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(&fx.base_url).expect("teardown");
}
