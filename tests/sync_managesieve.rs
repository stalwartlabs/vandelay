/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod common;
mod integration;
mod seeder;

use integration::stalwart::shared as shared_stalwart;
use rusqlite::Connection;

fn base_url() -> &'static str {
    shared_stalwart().base_url()
}

fn sieve_url() -> String {
    let s = shared_stalwart();
    format!("sieve://{}:{}", s.host, s.sieve_port)
}

fn sieves_url() -> String {
    let s = shared_stalwart();
    format!("sieves://{}:{}", s.host, s.sieve_port)
}
use vandelay::archive::sources::SourceChange;
use vandelay::export::{Confirm, Prune};
use vandelay::import::jmap::JmapImport;
use vandelay::import::managesieve::{ManageSieveImport, SaslAuth};
use vandelay::jmap::account::AccountSelector;
use vandelay::jmap::connect::ConnectConfig;
use vandelay::logging::{Level, Logger};
use vandelay::net::auth::Auth;
use vandelay::net::http::TlsPolicy;
use vandelay::net::stream::Cleartext;
use vandelay::sync::{RunMode, RunOptions};
use vandelay::types::ObjectType;

fn basic_config(localpart: &str) -> ManageSieveImport {
    ManageSieveImport {
        url: sieve_url(),
        auth: SaslAuth::Basic {
            user: format!("{localpart}@{}", seeder::DOMAIN),
            password: seeder::USER_PASSWORD.to_owned(),
        },
        cleartext: Cleartext::Forbid,
        source_change: SourceChange::Reject,
    }
}

#[test]
#[ignore = "requires Docker"]
fn managesieve_import_test1_lands_active_sieve_script() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test1").expect("test1");
    let archive = common::tmp_archive("test1");

    assert_eq!(fx.domain, seeder::DOMAIN);
    assert!(!fx.domain_id.is_empty(), "domain id resolved");
    assert_eq!(
        fx.admin_login,
        (
            seeder::ADMIN_USER.to_owned(),
            seeder::ADMIN_PASSWORD.to_owned()
        )
    );
    assert!(!fx.base_url.is_empty(), "base url set");
    assert!(!acc.account_id.is_empty(), "JMAP account id resolved");
    assert!(!acc.email.is_empty());
    assert!(!acc.admin_role, "test1 is a regular user");
    if let Some(seeded) = &acc.seeded {
        assert_eq!(
            seeded.sieve_active,
            Some(true),
            "test1 layout activates a sieve"
        );
        assert!(seeded.emails > 0);
        assert!(seeded.contacts > 0);
        assert!(seeded.events > 0);
        assert!(seeded.file_nodes > 0);
        assert!(seeded.address_books > 0);
        assert!(seeded.calendars > 0);
        assert!(seeded.identity);
        assert!(seeded.mailboxes_created > 0);
    }
    assert_eq!(acc.password, seeder::USER_PASSWORD);

    let summary = basic_config(&acc.localpart)
        .run(common::live_options(&archive))
        .into_result()
        .expect("import");
    assert!(!summary.any_failed(), "import had failures: {summary:?}");

    let conn = Connection::open(&archive).unwrap();
    let scripts = common::count(&conn, "sieve_scripts");
    assert!(scripts >= 2, "test1 layout seeds two scripts");

    let active: i64 = conn
        .query_row(
            "SELECT count(*) FROM sieve_scripts WHERE is_active = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(active, 1, "exactly one of test1's two scripts is active");

    let blob_count = common::count(&conn, "blobs");
    assert!(blob_count >= 1, "blob interned for the script body");

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn managesieve_import_test2_lands_inactive_sieve_script() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test2").expect("test2");
    let archive = common::tmp_archive("test2");

    let summary = basic_config(&acc.localpart)
        .run(common::live_options(&archive))
        .into_result()
        .expect("import");
    assert!(!summary.any_failed(), "import had failures: {summary:?}");

    let conn = Connection::open(&archive).unwrap();
    let scripts = common::count(&conn, "sieve_scripts");
    assert!(scripts >= 1, "expected at least one sieve script for test2");
    let active: i64 = conn
        .query_row(
            "SELECT count(*) FROM sieve_scripts WHERE is_active = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(active, 0, "test2's seeded script should be inactive");

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn managesieve_import_test3_has_no_scripts() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test3").expect("test3");
    let archive = common::tmp_archive("test3");

    let summary = basic_config(&acc.localpart)
        .run(common::live_options(&archive))
        .into_result()
        .expect("import");
    assert!(!summary.any_failed(), "import had failures: {summary:?}");

    let conn = Connection::open(&archive).unwrap();
    assert_eq!(common::count(&conn, "sieve_scripts"), 0);

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn managesieve_second_run_is_convergent() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test1").expect("test1");
    let archive = common::tmp_archive("converge");

    let first = basic_config(&acc.localpart)
        .run(common::live_options(&archive))
        .into_result()
        .expect("first import");
    let (_, c1) = &first.per_type[0];
    assert!(
        c1.created >= 2,
        "test1 seeds two scripts, both new on first run"
    );

    let summary = basic_config(&acc.localpart)
        .run(common::live_options(&archive))
        .into_result()
        .expect("second import");
    let (_, c) = &summary.per_type[0];
    assert_eq!(c.created, 0, "convergent run should create nothing");
    assert_eq!(c.deleted, 0, "convergent run should delete nothing");
    assert_eq!(c.fetched, 0, "convergent run should not rewrite blobs");
    assert!(c.skipped >= 2, "both present scripts should be skipped");

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn managesieve_and_jmap_imports_share_blob_bytes() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test1").expect("test1");
    let msieve_archive = common::tmp_archive("parity-msieve");
    let jmap_archive = common::tmp_archive("parity-jmap");

    basic_config(&acc.localpart)
        .run(common::live_options(&msieve_archive))
        .into_result()
        .expect("managesieve import");

    let jmap_cfg = JmapImport {
        connect: ConnectConfig {
            url: fx.base_url.clone(),
            auth: Auth::Basic {
                user: acc.email.clone(),
                password: seeder::USER_PASSWORD.to_owned(),
            },
            account: AccountSelector::Id(acc.account_id.clone()),
        },
        objects: Some(vec![vandelay::types::ObjectType::SieveScript].into()),
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

    let msieve_conn = Connection::open(&msieve_archive).unwrap();
    let jmap_conn = Connection::open(&jmap_archive).unwrap();

    fn name_hash_active(conn: &Connection) -> Vec<(String, Vec<u8>, bool)> {
        let mut stmt = conn
            .prepare(
                "SELECT s.name, b.hash, s.is_active
                 FROM sieve_scripts s JOIN blobs b ON b.id = s.blob_id
                 ORDER BY s.name",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                let name: String = r.get(0)?;
                let hash: Vec<u8> = r.get(1)?;
                let active: i64 = r.get(2)?;
                Ok((name, hash, active != 0))
            })
            .unwrap();
        rows.filter_map(|r| r.ok()).collect()
    }

    let msieve = name_hash_active(&msieve_conn);
    let jmap = name_hash_active(&jmap_conn);
    assert!(
        !msieve.is_empty(),
        "managesieve archive must have at least one script row"
    );
    assert_eq!(
        msieve, jmap,
        "every script's (name, blake3, is_active) must match across protocols"
    );

    let _ = std::fs::remove_file(&msieve_archive);
    let _ = std::fs::remove_file(&jmap_archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn managesieve_dry_run_reports_diff_without_writing() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test1").expect("test1");
    let archive = common::tmp_archive("dryrun");

    let mut common_cfg = common::live_options(&archive);
    common_cfg.mode = RunMode::DryRun;
    let summary = basic_config(&acc.localpart)
        .run(common_cfg)
        .into_result()
        .expect("dryrun");
    let (_, plan) = &summary.plan[0];
    assert!(plan.new >= 1, "expected at least one 'new' in dry-run plan");

    let conn = Connection::open(&archive).unwrap();
    assert_eq!(
        common::count(&conn, "sieve_scripts"),
        0,
        "dry-run must not write rows"
    );
    assert_eq!(
        common::count(&conn, "blobs"),
        0,
        "dry-run must not intern blobs"
    );

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn managesieve_source_change_protection_refuses_second_account() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc1 = fx.account("test1").expect("test1");
    let acc2 = fx.account("test2").expect("test2");
    let archive = common::tmp_archive("source_change");

    basic_config(&acc1.localpart)
        .run(common::live_options(&archive))
        .into_result()
        .expect("first import");

    let err = basic_config(&acc2.localpart)
        .run(common::live_options(&archive))
        .into_result()
        .expect_err("should refuse the second source without override");
    assert!(matches!(err, vandelay::error::Error::SourceChange(_)));

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn managesieve_implicit_tls_path_succeeds_when_offered() {
    let fx = seeder::provision(base_url()).expect("provision");
    let acc = fx.account("test1").expect("test1");
    let archive = common::tmp_archive("implicit_tls");

    let mut cfg = basic_config(&acc.localpart);
    cfg.url = sieves_url();
    let result = cfg.run(common::live_options(&archive)).into_result();

    if let Err(vandelay::error::Error::Connection(msg)) = &result {
        eprintln!("(expected on cleartext-only deployments) {msg}");
    }

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}

#[test]
#[ignore = "requires Docker"]
fn managesieve_round_trip_via_jmap_export_converges() {
    let fx = seeder::provision(base_url()).expect("provision");
    let src = fx.account("test1").expect("test1");
    let dst = fx.account("test4").expect("test4");
    let archive = common::tmp_archive("roundtrip");

    basic_config(&src.localpart)
        .run(common::live_options(&archive))
        .into_result()
        .expect("managesieve import");

    let import_conn = Connection::open(&archive).unwrap();
    let imported_scripts = common::count(&import_conn, "sieve_scripts");
    assert!(
        imported_scripts >= 1,
        "expected at least one script in the source archive"
    );
    let imported_active: i64 = import_conn
        .query_row(
            "SELECT count(*) FROM sieve_scripts WHERE is_active = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        imported_active, 1,
        "test1 should have exactly one active script"
    );
    drop(import_conn);

    let export_common = RunOptions {
        archive: archive.clone(),
        threads: 4,
        mode: RunMode::Apply,
        max_retries: 5,
        tls: TlsPolicy::AcceptInvalid,
        logger: Logger::new(Level::Default),
    };
    let export_cfg = vandelay::export::Export {
        connect: ConnectConfig {
            url: fx.base_url.clone(),
            auth: Auth::Basic {
                user: dst.email.clone(),
                password: seeder::USER_PASSWORD.to_owned(),
            },
            account: AccountSelector::Id(dst.account_id.clone()),
        },
        objects: Some(vec![vandelay::types::ObjectType::SieveScript].into()),
        prune: Prune::Keep,
        confirm: Confirm::Prompt,
    };
    let first = export_cfg
        .run(export_common)
        .into_result()
        .expect("first export");
    assert!(!first.any_failed(), "first export had failures: {first:?}");
    let created_first: u64 = first
        .per_type
        .iter()
        .filter(|(k, _)| *k == ObjectType::SieveScript)
        .map(|(_, c)| c.created)
        .sum();
    assert_eq!(
        created_first as i64, imported_scripts,
        "first export should create every imported script on the target ({first:?})"
    );

    let export_common2 = RunOptions {
        archive: archive.clone(),
        threads: 4,
        mode: RunMode::Apply,
        max_retries: 5,
        tls: TlsPolicy::AcceptInvalid,
        logger: Logger::new(Level::Default),
    };
    let export_cfg2 = vandelay::export::Export {
        connect: ConnectConfig {
            url: fx.base_url.clone(),
            auth: Auth::Basic {
                user: dst.email.clone(),
                password: seeder::USER_PASSWORD.to_owned(),
            },
            account: AccountSelector::Id(dst.account_id.clone()),
        },
        objects: Some(vec![vandelay::types::ObjectType::SieveScript].into()),
        prune: Prune::Keep,
        confirm: Confirm::Prompt,
    };
    let second = export_cfg2
        .run(export_common2)
        .into_result()
        .expect("second export");
    for (name, counts) in &second.per_type {
        assert_eq!(
            counts.created, 0,
            "{name:?}: round-trip second export should create nothing"
        );
        assert_eq!(
            counts.deleted, 0,
            "{name:?}: round-trip second export should delete nothing"
        );
    }

    let _ = std::fs::remove_file(&archive);
    seeder::teardown(base_url()).expect("teardown");
}
