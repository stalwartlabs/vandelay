/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod integration;

use integration::Account;
use integration::dovecot::{Dovecot, no_message_id_probe, shared_message_id_probe};
use integration::layouts::ImapSuite;
use integration::validate::{
    blob_bytes, blob_id_for_hash, cleanup, common, count, email_rows_for_blob, open_archive,
    tmp_archive,
};

use rusqlite::Connection;
use vandelay::archive::sources::SourceChange;
use vandelay::imap::client::ImapClient;
use vandelay::import::imap::SaslAuth;
use vandelay::import::managesieve::ManageSieveImport;
use vandelay::logging::{Level, Logger};
use vandelay::net::http::TlsPolicy;
use vandelay::net::sasl::SaslPayload;
use vandelay::net::stream::Cleartext;
use vandelay::net::stream::ConnectMode;
use vandelay::net::stream::Connector;

fn suite(d: &Dovecot) -> ImapSuite<'_> {
    ImapSuite {
        server: "dovecot",
        imap: &d.imap,
        cleartext: Cleartext::Forbid,
    }
}

fn sieve_config(account: &Account, sieve: &integration::Endpoint) -> ManageSieveImport {
    ManageSieveImport {
        url: format!("sieve://{}:{}", sieve.host, sieve.port),
        auth: SaslAuth::Basic {
            user: account.username.clone(),
            password: account.password.clone(),
        },
        cleartext: Cleartext::Allow,
        source_change: SourceChange::Reject,
    }
}

#[test]
#[ignore = "requires Docker"]
fn dovecot_starts_seeds_and_imports() {
    let d = Dovecot::start().expect("dovecot start");
    let seeds = d.seed_all().expect("dovecot seed");
    assert_eq!(
        seeds.len(),
        d.accounts.len(),
        "seed should return stats for every account"
    );
    d.verify_seed(&seeds).expect("dovecot verify");
    let suite = suite(&d);

    for seed in &seeds {
        let account = d
            .accounts
            .iter()
            .find(|a| a.username == seed.username)
            .expect("account");
        assert!(
            !seed.mailbox.paths.is_empty(),
            "{}: expected mailboxes seeded",
            seed.username
        );
        assert!(
            seed.mailbox.total_appends > 0,
            "{}: expected emails appended",
            seed.username
        );

        let archive = tmp_archive(&format!("dovecot-{}", seed.username));
        suite.import(account, &archive, "imap import");

        let conn = open_archive(&archive);
        suite.assert_seeded(&conn, account, &seed.mailbox);

        let nomid_target = seed
            .mailbox
            .extras
            .nomid_target
            .clone()
            .expect("nomid target");
        let nomid_hash = blake3::hash(&no_message_id_probe()).as_bytes().to_vec();
        let nomid_matches: Vec<String> = {
            let mut nomid_stmt = conn
                .prepare(
                    "SELECT message_match FROM emails
                     WHERE blob_id = (SELECT id FROM blobs WHERE hash = ?1)",
                )
                .expect("prepare nomid");
            nomid_stmt
                .query_map([nomid_hash.as_slice()], |r| r.get::<_, String>(0))
                .expect("query nomid")
                .filter_map(|r| r.ok())
                .collect()
        };
        assert!(
            !nomid_matches.is_empty(),
            "{}: no-Message-ID probe missing in archive (target {nomid_target})",
            seed.username
        );
        for m in &nomid_matches {
            let parsed: serde_json::Value = serde_json::from_str(m).expect("message_match json");
            let mids = parsed
                .get("m")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            assert!(
                mids.is_empty(),
                "{}: no-Message-ID probe should have empty Message-ID set, got {m}",
                seed.username
            );
            let fb = parsed.get("f").and_then(|v| v.as_str()).unwrap_or("");
            assert_eq!(
                fb.len(),
                64,
                "{}: no-Message-ID probe must carry a BLAKE3 fallback hex, got {m}",
                seed.username
            );
        }

        if seed.mailbox.extras.mid_dedup_targets.is_some() {
            let body = shared_message_id_probe();
            let hash = blake3::hash(&body);
            let blob_id = blob_id_for_hash(&conn, hash.as_bytes()).expect("shared-mid probe blob");
            let rows_for_blob = email_rows_for_blob(&conn, blob_id);
            assert_eq!(
                rows_for_blob, 2,
                "{}: shared-MID probe should produce 2 email rows for the dedup blob, got {rows_for_blob}",
                seed.username
            );
            let matches: Vec<String> = {
                let mut stmt = conn
                    .prepare("SELECT message_match FROM emails WHERE blob_id = ?1")
                    .expect("prepare matches");
                stmt.query_map([blob_id], |r| r.get::<_, String>(0))
                    .expect("query matches")
                    .filter_map(|r| r.ok())
                    .collect()
            };
            assert_eq!(matches.len(), 2, "{}: expected 2 matches", seed.username);
            assert_eq!(
                matches[0], matches[1],
                "{}: shared-MID emails must have identical message_match, got {matches:?}",
                seed.username
            );
        }

        assert_received_at_rfc3339_ish(&conn);
        drop(conn);

        suite.assert_incremental(account, &archive);

        if !account.layout.sieve_scripts.is_empty() {
            let sieve_archive = tmp_archive(&format!("dovecot-sieve-{}", seed.username));
            let summary = sieve_config(account, &d.sieve)
                .run(common(&sieve_archive))
                .into_result()
                .expect("sieve import");
            assert!(
                !summary.any_failed(),
                "sieve import had failures for {}: {summary:?}",
                seed.username
            );
            let conn = open_archive(&sieve_archive);
            let script_count = count(&conn, "sieve_scripts") as usize;
            assert_eq!(
                script_count,
                account.layout.sieve_scripts.len(),
                "{}: sieve scripts count mismatch",
                seed.username
            );
            let active_seed = account
                .layout
                .sieve_scripts
                .iter()
                .filter(|s| s.active)
                .count();
            let active_imported: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sieve_scripts WHERE is_active = 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                active_imported as usize, active_seed,
                "{}: active sieve script count mismatch",
                seed.username
            );

            for seeded in account.layout.sieve_scripts {
                let blob_id: i64 = conn
                    .query_row(
                        "SELECT blob_id FROM sieve_scripts WHERE name = ?1",
                        [seeded.name],
                        |r| r.get(0),
                    )
                    .unwrap();
                let body = blob_bytes(&conn, blob_id);
                assert_eq!(
                    body,
                    seeded.body.as_bytes(),
                    "{}: sieve script {} body round-trip",
                    seed.username,
                    seeded.name
                );
                let is_active: i64 = conn
                    .query_row(
                        "SELECT is_active FROM sieve_scripts WHERE name = ?1",
                        [seeded.name],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(
                    is_active == 1,
                    seeded.active,
                    "{}: sieve script {} active flag mismatch",
                    seed.username,
                    seeded.name
                );
            }
            let actives: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sieve_scripts WHERE is_active = 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                actives <= 1,
                "{}: at most one sieve script may be active",
                seed.username
            );
            drop(conn);

            let summary2 = sieve_config(account, &d.sieve)
                .run(common(&sieve_archive))
                .into_result()
                .expect("idempotent sieve re-import");
            assert!(
                !summary2.any_failed(),
                "{}: idempotent sieve re-import had failures",
                seed.username
            );

            cleanup(&sieve_archive);
        }

        cleanup(&archive);
    }

    suite.assert_source_change(&d.accounts[0], &d.accounts[1]);
    d.stop().expect("dovecot stop");
}

fn assert_received_at_rfc3339_ish(conn: &Connection) {
    let mut stmt = conn
        .prepare("SELECT received_at FROM emails LIMIT 50")
        .expect("prepare");
    let rows: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .expect("query")
        .filter_map(|r| r.ok())
        .collect();
    assert!(!rows.is_empty(), "no emails to check received_at");
    for r in rows {
        assert!(
            r.contains('T') && (r.ends_with('Z') || r.contains('+') || r.contains('-')),
            "received_at does not look like RFC 3339: {r}"
        );
    }
}

#[test]
#[ignore = "requires Docker"]
fn dovecot_non_ascii_mailbox_names_round_trip() {
    let d = Dovecot::start().expect("dovecot start");
    let account = d.accounts.first().expect("account");
    suite(&d).assert_non_ascii_round_trip(account);
}

#[test]
#[ignore = "requires Docker"]
fn dovecot_post_login_capability_with_imapsieve_url_is_parsed() {
    let d = Dovecot::start().expect("dovecot start");
    let account = d.accounts.first().expect("account");
    let connector = Connector::new(TlsPolicy::AcceptInvalid).expect("connector");
    let mut client = ImapClient::connect(
        &connector,
        &d.imap.host,
        d.imap.port,
        ConnectMode::StartTls,
        Logger::new(Level::Default),
    )
    .expect("connect");
    assert!(client.capabilities().contains("SASL-IR"));
    client
        .authenticate(SaslPayload::Plain {
            authcid: &account.username,
            password: &account.password,
        })
        .expect("AUTHENTICATE PLAIN must survive the untagged post-login CAPABILITY");
    assert!(
        client
            .capabilities()
            .contains("IMAPSIEVE=sieve://127.0.0.1:4190"),
        "post-login capabilities must carry the IMAPSIEVE URL: {:?}",
        client.capabilities()
    );
}
