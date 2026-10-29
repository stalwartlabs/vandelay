/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod integration;
mod seeder;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use integration::stalwart::shared as shared_stalwart;
use rusqlite::Connection;
use vandelay::jmap::account::AccountSelector;
use vandelay::jmap::http::Auth;
use vandelay::logging::Logger;

fn base_url() -> &'static str {
    shared_stalwart().base_url()
}
use vandelay::sync::import_dav::{DavAuth, DavImportConfig, DavKindArg, SourceChange};
use vandelay::sync::{self, CommonConfig, ConnectConfig, ExportConfig, ImportConfig};

fn tmp_archive(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "vandelay-{tag}-{}-{}.sqlite",
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
        threads: 4,
        dry_run: false,
        max_retries: 5,
        allow_invalid_certs: true,
        logger: Logger::from_flags(false, 0),
    }
}

fn carddav_cfg(base_url: &str, domain: &str, localpart: &str) -> DavImportConfig {
    DavImportConfig {
        kind: DavKindArg::Carddav,
        url: base_url.to_owned(),
        auth: DavAuth::Basic {
            user: format!("{localpart}@{domain}"),
            password: seeder::USER_PASSWORD.to_owned(),
        },
        allow_cleartext: false,
        dav_connections: 4,
        multiget_batch: 50,
        source_change: SourceChange::Reject,
    }
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn jmap_basic(localpart: &str, domain: &str) -> Auth {
    Auth::Basic {
        user: format!("{localpart}@{domain}"),
        password: seeder::USER_PASSWORD.to_owned(),
    }
}

fn export_cfg(base_url: &str, domain: &str, localpart: &str, account_id: &str) -> ExportConfig {
    ExportConfig {
        connect: ConnectConfig {
            url: base_url.to_owned(),
            auth: jmap_basic(localpart, domain),
            account: AccountSelector::Id(account_id.to_owned()),
        },
        objects: None,
        prune: false,
        yes: true,
    }
}

fn jmap_import_cfg(
    base_url: &str,
    domain: &str,
    localpart: &str,
    account_id: &str,
) -> ImportConfig {
    ImportConfig {
        connect: ConnectConfig {
            url: base_url.to_owned(),
            auth: jmap_basic(localpart, domain),
            account: AccountSelector::Id(account_id.to_owned()),
        },
        objects: None,
        allow_source_change: false,
    }
}

fn card_uids(conn: &Connection) -> HashSet<String> {
    let mut stmt = conn.prepare("SELECT uid FROM contact_cards").unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
    let mut out = HashSet::new();
    for r in rows {
        out.insert(r.unwrap());
    }
    out
}

#[test]
#[ignore = "requires Docker"]
fn carddav_import_test1_yields_address_books_and_cards() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test1").expect("test1");
    let archive = tmp_archive("carddav");

    assert_eq!(fx.domain, seeder::DOMAIN);
    assert!(!fx.domain_id.is_empty(), "domain id resolved");
    assert_eq!(
        fx.admin_login,
        (
            seeder::ADMIN_USER.to_owned(),
            seeder::ADMIN_PASSWORD.to_owned()
        )
    );
    assert!(!acc.admin_role, "test1 is a regular user, not admin");
    let seeded = acc.seeded.as_ref().expect("test1 seed stats");
    assert!(
        seeded.address_books > 0,
        "test1 layout requests an extra address book"
    );
    assert!(seeded.contacts > 0, "test1 layout seeds contacts");
    assert!(
        seeded.mailboxes_created > 0,
        "test1 has a seeded mailbox tree"
    );
    assert!(seeded.emails > 0, "test1 has seeded emails");
    assert!(seeded.calendars > 0, "test1 has seeded calendars");
    assert!(seeded.events > 0, "test1 has seeded events");
    assert!(seeded.file_nodes > 0, "test1 has seeded file nodes");
    assert!(seeded.identity, "test1 has a custom identity");
    assert_eq!(
        seeded.sieve_active,
        Some(true),
        "test1 has an active sieve script"
    );

    let summary = sync::import_dav::run(
        common(&archive),
        carddav_cfg(&fx.base_url, &fx.domain, &acc.localpart),
    )
    .expect("import");
    assert!(!summary.any_failed(), "import had failures: {summary:?}");

    let conn = Connection::open(&archive).unwrap();
    let books = count(&conn, "address_books") as usize;
    let cards = count(&conn, "contact_cards") as usize;
    assert!(
        books >= seeded.address_books,
        "imported address books ({books}) covers seeded layout ({})",
        seeded.address_books,
    );
    assert!(
        cards >= seeded.contacts,
        "imported cards ({cards}) covers seeded layout ({})",
        seeded.contacts,
    );

    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn carddav_import_covers_all_three_sync_in_accounts() {
    let fx = seeder::provision(base_url()).expect("provision");
    for localpart in ["test1", "test2", "test3"] {
        let acc = fx.account(localpart).expect(localpart);
        let archive = tmp_archive(&format!("carddav-all-{localpart}"));
        let summary = sync::import_dav::run(
            common(&archive),
            carddav_cfg(&fx.base_url, &fx.domain, &acc.localpart),
        )
        .unwrap_or_else(|e| panic!("import {localpart} failed: {e:?}"));
        assert!(
            !summary.any_failed(),
            "{localpart} import had failures: {summary:?}"
        );
        let conn = Connection::open(&archive).unwrap();
        let books = count(&conn, "address_books") as usize;
        assert!(
            books >= 1,
            "{localpart} has at least one address book (Stalwart default)"
        );
    }
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn carddav_import_with_tiny_multiget_batch_converges_identically() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test1").expect("test1");

    let archive_big = tmp_archive("carddav-batch-big");
    sync::import_dav::run(
        common(&archive_big),
        carddav_cfg(&fx.base_url, &fx.domain, &acc.localpart),
    )
    .expect("import big batch");
    let conn = Connection::open(&archive_big).unwrap();
    let big = count(&conn, "contact_cards");
    drop(conn);

    let archive_small = tmp_archive("carddav-batch-small");
    let mut cfg = carddav_cfg(&fx.base_url, &fx.domain, &acc.localpart);
    cfg.multiget_batch = 1;
    sync::import_dav::run(common(&archive_small), cfg).expect("import small batch");
    let conn = Connection::open(&archive_small).unwrap();
    let small = count(&conn, "contact_cards");
    assert_eq!(big, small, "batch size must not affect imported count");

    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn carddav_round_trip_via_jmap_export_converges() {
    let fx = seeder::provision(base_url()).expect("provision");
    let src = fx.account("test1").expect("test1");
    let tgt = fx.account("test4").expect("test4");
    let archive = tmp_archive("carddav-rt");

    let imp = sync::import_dav::run(
        common(&archive),
        carddav_cfg(&fx.base_url, &fx.domain, &src.localpart),
    )
    .expect("dav import");
    assert!(!imp.any_failed(), "dav import had failures: {imp:?}");

    let conn = Connection::open(&archive).unwrap();
    let local_cards = count(&conn, "contact_cards");
    assert!(local_cards > 0, "DAV import landed contact cards");
    drop(conn);

    let e1 = sync::export::run(
        common(&archive),
        export_cfg(&fx.base_url, &fx.domain, &tgt.localpart, &tgt.account_id),
    )
    .expect("export 1");
    assert!(!e1.any_failed(), "export 1 had failures: {e1:?}");
    let created_cards_1: u64 = e1
        .per_type
        .iter()
        .filter(|(t, _)| *t == "ContactCard")
        .map(|(_, c)| c.created)
        .sum();
    assert!(
        created_cards_1 > 0,
        "first export created ContactCard rows on the target: {e1:?}"
    );

    let e2 = sync::export::run(
        common(&archive),
        export_cfg(&fx.base_url, &fx.domain, &tgt.localpart, &tgt.account_id),
    )
    .expect("export 2");
    let created_2: u64 = e2.per_type.iter().map(|(_, c)| c.created).sum();
    assert_eq!(
        created_2, 0,
        "second export must be convergent (no new creates): {e2:?}"
    );
    let skipped_2: u64 = e2.per_type.iter().map(|(_, c)| c.skipped).sum();
    assert!(
        skipped_2 > 0,
        "second export matched existing target objects: {e2:?}"
    );

    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn carddav_cross_protocol_parity_with_jmap_matches_uid_set() {
    let fx = seeder::provision(base_url()).expect("provision");
    let src = fx.account("test1").expect("test1");

    let archive_jmap = tmp_archive("carddav-parity-jmap");
    let s_jmap = sync::import_jmap::run(
        common(&archive_jmap),
        jmap_import_cfg(&fx.base_url, &fx.domain, &src.localpart, &src.account_id),
    )
    .expect("jmap import");
    assert!(!s_jmap.any_failed(), "jmap import failures: {s_jmap:?}");

    let archive_dav = tmp_archive("carddav-parity-dav");
    let s_dav = sync::import_dav::run(
        common(&archive_dav),
        carddav_cfg(&fx.base_url, &fx.domain, &src.localpart),
    )
    .expect("dav import");
    assert!(!s_dav.any_failed(), "dav import failures: {s_dav:?}");

    let conn_jmap = Connection::open(&archive_jmap).unwrap();
    let uids_jmap = card_uids(&conn_jmap);
    drop(conn_jmap);

    let conn_dav = Connection::open(&archive_dav).unwrap();
    let uids_dav = card_uids(&conn_dav);
    drop(conn_dav);

    assert!(
        !uids_jmap.is_empty(),
        "jmap import should yield at least one card uid"
    );
    assert!(
        !uids_dav.is_empty(),
        "dav import should yield at least one card uid"
    );
    let only_in_jmap: HashSet<_> = uids_jmap.difference(&uids_dav).collect();
    let only_in_dav: HashSet<_> = uids_dav.difference(&uids_jmap).collect();
    assert!(
        only_in_jmap.is_empty() && only_in_dav.is_empty(),
        "uid sets must match across protocols: only-in-jmap={only_in_jmap:?} only-in-dav={only_in_dav:?}"
    );

    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn carddav_import_is_idempotent_on_second_run() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test1").expect("test1");
    assert!(!acc.admin_role);
    let archive = tmp_archive("carddav-idem");

    sync::import_dav::run(
        common(&archive),
        carddav_cfg(&fx.base_url, &fx.domain, &acc.localpart),
    )
    .expect("import 1");
    let conn1 = Connection::open(&archive).unwrap();
    let n1 = count(&conn1, "contact_cards");
    drop(conn1);

    sync::import_dav::run(
        common(&archive),
        carddav_cfg(&fx.base_url, &fx.domain, &acc.localpart),
    )
    .expect("import 2");
    let conn2 = Connection::open(&archive).unwrap();
    let n2 = count(&conn2, "contact_cards");
    assert_eq!(n1, n2, "second import does not duplicate cards");

    seeder::teardown(base_url()).expect("teardown");
}
