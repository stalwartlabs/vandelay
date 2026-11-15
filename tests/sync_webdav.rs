/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod common;
mod integration;
mod seeder;

use std::collections::HashSet;

use integration::stalwart::shared as shared_stalwart;
use rusqlite::Connection;

fn base_url() -> &'static str {
    shared_stalwart().base_url()
}
use vandelay::archive::sources::SourceChange;
use vandelay::export::Prune;
use vandelay::import::dav::{DavImport, DavKindArg};
use vandelay::net::auth::Auth;
use vandelay::types::ObjectType;

fn webdav_cfg(root_url: String, domain: &str, localpart: &str) -> DavImport {
    DavImport {
        kind: DavKindArg::Webdav,
        url: root_url,
        auth: Auth::Basic {
            user: format!("{localpart}@{domain}"),
            password: seeder::USER_PASSWORD.to_owned(),
        },
        allow_cleartext: false,
        dav_connections: 4,
        multiget_batch: 50,
        source_change: SourceChange::Reject,
    }
}

fn file_blob_hashes(conn: &Connection) -> HashSet<Vec<u8>> {
    let mut stmt = conn
        .prepare(
            "SELECT blobs.hash FROM file_nodes
             JOIN blobs ON blobs.id = file_nodes.blob_id
             WHERE file_nodes.node_type = 'file'",
        )
        .unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0)).unwrap();
    let mut out = HashSet::new();
    for r in rows {
        out.insert(r.unwrap());
    }
    out
}

#[test]
#[ignore = "requires Docker"]
fn webdav_import_test1_walks_directory_tree() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test1").expect("test1");
    let archive = common::tmp_archive("webdav");

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
    assert!(seeded.file_nodes > 0, "test1 layout seeds file nodes");
    assert!(
        seeded.mailboxes_created > 0,
        "test1 has a seeded mailbox tree"
    );
    assert!(seeded.emails > 0, "test1 has seeded emails");
    assert!(seeded.address_books > 0, "test1 has seeded address books");
    assert!(seeded.contacts > 0, "test1 has seeded contacts");
    assert!(seeded.calendars > 0, "test1 has seeded calendars");
    assert!(seeded.events > 0, "test1 has seeded events");
    assert!(seeded.identity, "test1 has a custom identity");
    assert_eq!(
        seeded.sieve_active,
        Some(true),
        "test1 has an active sieve script"
    );

    let root = format!("{}/dav/file/", fx.base_url);
    let summary = webdav_cfg(root, &fx.domain, &acc.localpart)
        .run(common::live_options(&archive))
        .into_result()
        .expect("import");
    assert!(!summary.any_failed(), "import had failures: {summary:?}");

    let conn = Connection::open(&archive).unwrap();
    let nodes = common::count(&conn, "file_nodes") as usize;
    assert!(
        nodes >= seeded.file_nodes,
        "imported file_nodes ({nodes}) covers seeded layout ({})",
        seeded.file_nodes,
    );

    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn webdav_round_trip_via_jmap_export_converges() {
    let fx = seeder::provision(base_url()).expect("provision");
    let src = fx.account("test1").expect("test1");
    let tgt = fx.account("test4").expect("test4");
    let archive = common::tmp_archive("webdav-rt");

    let root = format!("{}/dav/file/", fx.base_url);
    let imp = webdav_cfg(root, &fx.domain, &src.localpart)
        .run(common::live_options(&archive))
        .into_result()
        .expect("dav import");
    assert!(!imp.any_failed(), "dav import had failures: {imp:?}");

    let conn = Connection::open(&archive).unwrap();
    let local_files: i64 = conn
        .query_row(
            "SELECT count(*) FROM file_nodes WHERE node_type = 'file'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(local_files > 0, "DAV import landed files in the archive");
    drop(conn);

    let e1 = common::export(
        common::connect(
            &fx.base_url,
            common::basic(&tgt.email, &tgt.password),
            &tgt.account_id,
        ),
        None,
        Prune::Keep,
    )
    .run(common::live_options(&archive))
    .into_result()
    .expect("export 1");
    assert!(!e1.any_failed(), "export 1 had failures: {e1:?}");
    let created_files_1: u64 = e1
        .per_type
        .iter()
        .filter(|(t, _)| *t == ObjectType::FileNode)
        .map(|(_, c)| c.created)
        .sum();
    assert!(
        created_files_1 > 0,
        "first export created FileNode rows on the target: {e1:?}"
    );

    let e2 = common::export(
        common::connect(
            &fx.base_url,
            common::basic(&tgt.email, &tgt.password),
            &tgt.account_id,
        ),
        None,
        Prune::Keep,
    )
    .run(common::live_options(&archive))
    .into_result()
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
fn webdav_cross_protocol_parity_with_jmap_matches_blob_hashes() {
    let fx = seeder::provision(base_url()).expect("provision");
    let src = fx.account("test1").expect("test1");

    let archive_jmap = common::tmp_archive("webdav-parity-jmap");
    let s_jmap = common::jmap_import(
        common::connect(
            &fx.base_url,
            common::basic(&src.email, &src.password),
            &src.account_id,
        ),
        None,
    )
    .run(common::live_options(&archive_jmap))
    .into_result()
    .expect("jmap import");
    assert!(!s_jmap.any_failed(), "jmap import failures: {s_jmap:?}");

    let archive_dav = common::tmp_archive("webdav-parity-dav");
    let root = format!("{}/dav/file/", fx.base_url);
    let s_dav = webdav_cfg(root, &fx.domain, &src.localpart)
        .run(common::live_options(&archive_dav))
        .into_result()
        .expect("dav import");
    assert!(!s_dav.any_failed(), "dav import failures: {s_dav:?}");

    let conn_jmap = Connection::open(&archive_jmap).unwrap();
    let hashes_jmap = file_blob_hashes(&conn_jmap);
    drop(conn_jmap);

    let conn_dav = Connection::open(&archive_dav).unwrap();
    let hashes_dav = file_blob_hashes(&conn_dav);
    drop(conn_dav);

    assert!(
        !hashes_jmap.is_empty(),
        "jmap import should yield at least one file blob"
    );
    assert!(
        !hashes_dav.is_empty(),
        "dav import should yield at least one file blob"
    );
    let only_in_jmap: HashSet<_> = hashes_jmap.difference(&hashes_dav).collect();
    let only_in_dav: HashSet<_> = hashes_dav.difference(&hashes_jmap).collect();
    assert!(
        only_in_jmap.is_empty() && only_in_dav.is_empty(),
        "BLAKE3 hash sets must match across protocols (both store bytes verbatim): \
         only-in-jmap={} only-in-dav={}",
        only_in_jmap.len(),
        only_in_dav.len(),
    );

    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn webdav_import_is_idempotent_on_second_run() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test1").expect("test1");
    assert!(!acc.admin_role);
    let archive = common::tmp_archive("webdav-idem");

    let root = format!("{}/dav/file/", fx.base_url);
    webdav_cfg(root.clone(), &fx.domain, &acc.localpart)
        .run(common::live_options(&archive))
        .into_result()
        .expect("import 1");
    let conn1 = Connection::open(&archive).unwrap();
    let n1 = common::count(&conn1, "file_nodes");
    let b1 = common::count(&conn1, "blobs");
    drop(conn1);

    webdav_cfg(root, &fx.domain, &acc.localpart)
        .run(common::live_options(&archive))
        .into_result()
        .expect("import 2");
    let conn2 = Connection::open(&archive).unwrap();
    let n2 = common::count(&conn2, "file_nodes");
    let b2 = common::count(&conn2, "blobs");
    assert_eq!(n1, n2, "second import does not duplicate file_nodes");
    assert_eq!(
        b1, b2,
        "second import does not duplicate blobs (BLAKE3 dedup)"
    );

    seeder::teardown(base_url()).expect("teardown");
}
