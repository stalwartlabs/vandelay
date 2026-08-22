/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod integration;

use std::collections::HashSet;

use integration::Account;
use integration::dovecot::{AccountSeed, Dovecot, flag_probe_message, shared_message_id_probe};
use integration::validate::{
    assert_message_round_trip, blob_bytes, blob_id_for_hash, cleanup, common, count,
    email_rows_for_blob, emails_in_mailbox, keywords_for_blob, mailbox_id_by_name,
    mailbox_id_by_path, mailbox_path, open_archive, tmp_archive,
};

use rusqlite::Connection;
use vandelay::error::Error;
use vandelay::sync::import_imap::{ImapAuth, ImapImportConfig};
use vandelay::sync::import_managesieve::{ManageSieveAuth, ManageSieveImportConfig};
use vandelay::sync::{import_imap, import_managesieve};

fn imap_config(account: &Account, imap: &integration::Endpoint) -> ImapImportConfig {
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

fn sieve_config(account: &Account, sieve: &integration::Endpoint) -> ManageSieveImportConfig {
    ManageSieveImportConfig {
        url: format!("sieve://{}:{}", sieve.host, sieve.port),
        auth: ManageSieveAuth::Basic {
            user: account.username.clone(),
            password: account.password.clone(),
        },
        allow_cleartext: true,
        allow_source_change: false,
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
        let summary =
            import_imap::run(common(&archive), imap_config(account, &d.imap)).expect("imap import");
        assert!(
            !summary.any_failed(),
            "imap import had failures for {}: {summary:?}",
            seed.username
        );

        let conn = open_archive(&archive);
        let mailbox_count = count(&conn, "mailboxes") as usize;
        let email_count = count(&conn, "emails") as usize;
        let blob_count = count(&conn, "blobs") as usize;
        let expected_mailboxes = account.layout.mailboxes.len() + 1;
        assert_eq!(
            mailbox_count, expected_mailboxes,
            "{}: imported {mailbox_count} mailboxes, expected exactly {expected_mailboxes}",
            seed.username
        );
        assert_eq!(
            email_count, seed.mailbox.total_appends,
            "{}: every seeded email must land in archive (appended {}, imported {email_count})",
            seed.username, seed.mailbox.total_appends
        );
        assert!(blob_count > 0, "{}: blobs interned", seed.username);

        let bad_match: i64 = conn
            .query_row(
                "SELECT count(*) FROM emails WHERE message_match IS NULL OR message_match = ''",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            bad_match, 0,
            "{}: every imported email must have a non-empty message_match",
            seed.username
        );

        let nomid_target = seed
            .mailbox
            .extras
            .nomid_target
            .clone()
            .expect("nomid target");
        let nomid_hash = blake3::hash(&integration::dovecot::no_message_id_probe())
            .as_bytes()
            .to_vec();
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

        if let Some(target) = &seed.mailbox.extras.flagged_target {
            let probe = flag_probe_message();
            let probe_hash = blake3::hash(&probe);
            let blob_id = blob_id_for_hash(&conn, probe_hash.as_bytes()).expect("flag probe blob");
            let stored = blob_bytes(&conn, blob_id);
            assert_eq!(
                stored, probe,
                "{}: flag probe blob bytes mismatch",
                seed.username
            );
            let keywords = keywords_for_blob(&conn, blob_id);
            let kw_set: HashSet<_> = keywords.iter().map(String::as_str).collect();
            assert!(
                kw_set.contains("$seen"),
                "{}: flag probe missing $seen in keywords {keywords:?}",
                seed.username
            );
            assert!(
                kw_set.contains("$flagged"),
                "{}: flag probe missing $flagged in keywords {keywords:?}",
                seed.username
            );
            let _ = target;
        }

        if let Some((a, b)) = &seed.mailbox.extras.mid_dedup_targets {
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
            let _ = (a, b);
        }

        if let Some(target) = &seed.mailbox.extras.dedup_target {
            let _ = target;
        }

        for path in &seed.mailbox.paths {
            let id = mailbox_id_by_path(&conn, path, '/').unwrap_or_else(|| {
                panic!("{}: mailbox path {path} missing in archive", seed.username)
            });
            assert_eq!(mailbox_path(&conn, id, '/'), *path);
            let expected = seed.mailbox.histogram.get(path).copied().unwrap_or(0);
            let got = emails_in_mailbox(&conn, id) as usize;
            assert_eq!(
                got, expected,
                "{}: mailbox {path} email count {got} != appended {expected}",
                seed.username
            );
        }
        for append in &seed.mailbox.appends {
            assert_message_round_trip(
                &conn,
                &append.raw,
                &append.target,
                &format!("{}/append/{:?}", seed.username, append.tag),
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
            "{}: INBOX role tagged",
            seed.username
        );
        let inbox_id = mailbox_id_by_name(&conn, "INBOX").expect("INBOX");
        let inbox_seeded = seed.mailbox.histogram.get("INBOX").copied().unwrap_or(0);
        assert_eq!(
            emails_in_mailbox(&conn, inbox_id) as usize,
            inbox_seeded,
            "{}: INBOX email count mismatch",
            seed.username
        );
        assert_received_at_rfc3339_ish(&conn);

        drop(conn);

        let summary2 = import_imap::run(common(&archive), imap_config(account, &d.imap))
            .expect("idempotent imap re-import");
        assert!(
            !summary2.any_failed(),
            "{}: idempotent re-import had failures: {summary2:?}",
            seed.username
        );
        let conn = open_archive(&archive);
        let email_count2 = count(&conn, "emails") as usize;
        let mailbox_count2 = count(&conn, "mailboxes") as usize;
        let blob_count2 = count(&conn, "blobs") as usize;
        assert_eq!(
            email_count2, email_count,
            "{}: idempotent re-import added emails",
            seed.username
        );
        assert_eq!(
            mailbox_count2, mailbox_count,
            "{}: idempotent re-import added mailboxes",
            seed.username
        );
        assert_eq!(
            blob_count2, blob_count,
            "{}: idempotent re-import added blobs",
            seed.username
        );
        drop(conn);

        d.delete_first_inbox_message(account)
            .expect("expunge first INBOX message");
        let summary_after = import_imap::run(common(&archive), imap_config(account, &d.imap))
            .expect("re-import after expunge");
        assert!(
            !summary_after.any_failed(),
            "{}: re-import after expunge had failures",
            seed.username
        );
        let conn = open_archive(&archive);
        let email_count3 = count(&conn, "emails") as usize;
        assert_eq!(
            email_count3,
            email_count - 1,
            "{}: expunged message was not pruned",
            seed.username
        );
        drop(conn);

        let (added_raw, added_mid) = d
            .append_new_message(account, "INBOX", "post-import")
            .expect("append new INBOX message");
        let summary_after_append =
            import_imap::run(common(&archive), imap_config(account, &d.imap))
                .expect("re-import after append");
        assert!(
            !summary_after_append.any_failed(),
            "{}: re-import after append had failures",
            seed.username
        );
        let conn = open_archive(&archive);
        let email_count_after_append = count(&conn, "emails") as usize;
        assert_eq!(
            email_count_after_append, email_count,
            "{}: append-after-expunge should restore email count to baseline",
            seed.username
        );
        let added_hash = blake3::hash(&added_raw);
        let added_blob = integration::validate::blob_id_for_hash(&conn, added_hash.as_bytes())
            .expect("added message blob present");
        let stored = integration::validate::blob_bytes(&conn, added_blob);
        assert_eq!(
            stored, added_raw,
            "{}: added message blob bytes mismatch",
            seed.username
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
            "{}: added Message-ID {added_mid} not found in message_match",
            seed.username
        );
        let added_inbox_id = mailbox_id_by_name(&conn, "INBOX").expect("INBOX id");
        let added_in_inbox: bool = conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM emails
                                WHERE blob_id = ?1
                                  AND EXISTS (SELECT 1 FROM json_each(mailbox_ids) j
                                              WHERE j.value = ?2))",
                rusqlite::params![added_blob, added_inbox_id],
                |r| r.get::<_, i64>(0).map(|n| n != 0),
            )
            .unwrap();
        assert!(
            added_in_inbox,
            "{}: added message blob not linked to INBOX",
            seed.username
        );
        drop(conn);

        if !account.layout.sieve_scripts.is_empty() {
            let sieve_archive = tmp_archive(&format!("dovecot-sieve-{}", seed.username));
            let summary =
                import_managesieve::run(common(&sieve_archive), sieve_config(account, &d.sieve))
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

            let summary2 =
                import_managesieve::run(common(&sieve_archive), sieve_config(account, &d.sieve))
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

    let primary = &d.accounts[0];
    let other = &d.accounts[1];
    let shared_archive = tmp_archive("dovecot-source-change");
    import_imap::run(common(&shared_archive), imap_config(primary, &d.imap))
        .expect("seed archive with primary user");
    let err = import_imap::run(common(&shared_archive), imap_config(other, &d.imap))
        .expect_err("expected source-change abort");
    assert!(
        matches!(err, Error::SourceChange(_)),
        "expected SourceChange, got {err:?}"
    );
    cleanup(&shared_archive);

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

fn _unused(_: &AccountSeed) {}

#[test]
#[ignore = "requires Docker"]
fn dovecot_non_ascii_mailbox_names_round_trip() {
    let d = Dovecot::start().expect("dovecot start");
    let account = d.accounts.first().expect("account").clone();

    let mutf7_name = "Envoy&AOk-s";
    let utf8_name = "Envoyés";
    let message = b"From: accents@vandelay.test\r\n\
         To: user1@vandelay.test\r\n\
         Subject: Accents\r\n\
         Message-ID: <accents-1@vandelay.test>\r\n\
         Date: Wed, 01 Jan 2025 12:00:00 +0000\r\n\
         \r\n\
         Accented folder probe.\r\n";
    d.create_non_ascii_mailbox(&account, mutf7_name, message)
        .expect("create accented mailbox");

    let archive = tmp_archive("dovecot-non-ascii");
    let summary =
        import_imap::run(common(&archive), imap_config(&account, &d.imap)).expect("imap import");
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
    let emails = emails_in_mailbox(&conn, mailbox_id);
    assert_eq!(
        emails, 1,
        "the message inside the accented folder must be imported"
    );
    cleanup(&archive);
}
