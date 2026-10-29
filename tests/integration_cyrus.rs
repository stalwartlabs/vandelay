/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod integration;

use std::collections::HashSet;

use integration::cyrus::{AccountSeed, Cyrus, flag_probe_message};
use integration::validate::{
    assert_message_round_trip, blob_bytes, blob_id_for_hash, cleanup, common, count,
    email_rows_for_blob, emails_in_mailbox, keywords_for_blob, mailbox_id_by_name,
    mailbox_id_by_path, mailbox_path, open_archive, tmp_archive,
};
use integration::{Account, Endpoint};

use vandelay::error::Error;
use vandelay::sync::import_imap;
use vandelay::sync::import_imap::{ImapAuth, ImapImportConfig};

fn imap_config(account: &Account, imap: &Endpoint) -> ImapImportConfig {
    ImapImportConfig {
        url: format!("imap://{}:{}", imap.host, imap.port),
        auth: ImapAuth::Basic {
            user: account.username.clone(),
            password: account.password.clone(),
        },
        allow_cleartext: true,
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

#[test]
#[ignore = "requires Docker"]
fn cyrus_starts_seeds_and_imports() {
    let c = Cyrus::start().expect("cyrus start");
    let seeds = c.seed_all().expect("cyrus seed");
    c.verify_seed(&seeds).expect("cyrus verify");

    for seed in &seeds {
        let account = c
            .accounts
            .iter()
            .find(|a| a.username == seed.username)
            .expect("account");
        assert!(
            !seed.paths.is_empty(),
            "{}: expected mailboxes seeded",
            seed.username
        );
        assert!(
            seed.total_appends > 0,
            "{}: expected emails appended",
            seed.username
        );

        let archive = tmp_archive(&format!("cyrus-{}", seed.username));
        let summary =
            import_imap::run(common(&archive), imap_config(account, &c.imap)).expect("imap import");
        assert!(
            !summary.any_failed(),
            "cyrus import failed for {}: {summary:?}",
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
            email_count, seed.total_appends,
            "{}: every seeded email must land in archive (appended {}, imported {email_count})",
            seed.username, seed.total_appends
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

        if let Some(target) = &seed.flagged_target {
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

        if let Some(_target) = &seed.dedup_target
            && let Some(first) = seeds
                .iter()
                .find(|s| s.username == seed.username)
                .and_then(|_| account.layout.mailboxes.first())
        {
            let _ = first;
            let total_dup: i64 = conn
                .query_row(
                    "SELECT count(*) FROM emails e
                     WHERE EXISTS (SELECT 1 FROM emails e2
                                   WHERE e2.blob_id = e.blob_id AND e2.id <> e.id)",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                total_dup >= 2,
                "{}: dedup append should produce at least two rows sharing a blob",
                seed.username
            );
        }

        for path in &seed.paths {
            let id = mailbox_id_by_path(&conn, path, '/').unwrap_or_else(|| {
                panic!("{}: mailbox path {path} missing in archive", seed.username)
            });
            assert_eq!(mailbox_path(&conn, id, '/'), *path);
            let expected = seed.histogram.get(path).copied().unwrap_or(0);
            let got = emails_in_mailbox(&conn, id) as usize;
            assert_eq!(
                got, expected,
                "{}: mailbox {path} email count {got} != appended {expected}",
                seed.username
            );
        }
        for append in &seed.appends {
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
        let inbox_seeded = seed.histogram.get("INBOX").copied().unwrap_or(0);
        assert_eq!(
            emails_in_mailbox(&conn, inbox_id) as usize,
            inbox_seeded,
            "{}: INBOX email count mismatch",
            seed.username
        );
        let probe_blob = blob_id_for_hash(&conn, blake3::hash(&flag_probe_message()).as_bytes());
        if let Some(blob_id) = probe_blob {
            let rows = email_rows_for_blob(&conn, blob_id);
            assert!(rows >= 1, "{}: flag probe missing", seed.username);
        }

        drop(conn);

        let summary2 = import_imap::run(common(&archive), imap_config(account, &c.imap))
            .expect("idempotent imap re-import");
        assert!(
            !summary2.any_failed(),
            "{}: idempotent re-import had failures: {summary2:?}",
            seed.username
        );
        let conn = open_archive(&archive);
        let email_count2 = count(&conn, "emails") as usize;
        let blob_count2 = count(&conn, "blobs") as usize;
        assert_eq!(
            email_count2, email_count,
            "{}: idempotent re-import added emails",
            seed.username
        );
        assert_eq!(
            blob_count2, blob_count,
            "{}: idempotent re-import added blobs",
            seed.username
        );
        drop(conn);

        c.delete_first_inbox_message(account)
            .expect("expunge first INBOX message");
        let summary_after = import_imap::run(common(&archive), imap_config(account, &c.imap))
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

        let (added_raw, added_mid) = c
            .append_new_message(account, "INBOX", "post-import")
            .expect("append new INBOX message");
        let summary_after_append =
            import_imap::run(common(&archive), imap_config(account, &c.imap))
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
        let added_blob =
            blob_id_for_hash(&conn, added_hash.as_bytes()).expect("added message blob present");
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

        cleanup(&archive);
    }

    let primary = &c.accounts[0];
    let other = &c.accounts[1];
    let shared_archive = tmp_archive("cyrus-source-change");
    import_imap::run(common(&shared_archive), imap_config(primary, &c.imap))
        .expect("seed archive with primary user");
    let err = import_imap::run(common(&shared_archive), imap_config(other, &c.imap))
        .expect_err("expected source-change abort");
    assert!(
        matches!(err, Error::SourceChange(_)),
        "expected SourceChange, got {err:?}"
    );
    cleanup(&shared_archive);

    c.stop().expect("cyrus stop");
}

fn _unused(_: &AccountSeed) {}

#[test]
#[ignore = "requires Docker"]
fn cyrus_non_ascii_mailbox_names_round_trip() {
    let c = Cyrus::start().expect("cyrus start");
    let account = c.accounts.first().expect("account").clone();

    let mutf7_name = "Envoy&AOk-s";
    let utf8_name = "Envoyés";
    let message = b"From: accents@vandelay.test\r\n\
         To: user1@vandelay.test\r\n\
         Subject: Accents\r\n\
         Message-ID: <accents-1@vandelay.test>\r\n\
         Date: Wed, 01 Jan 2025 12:00:00 +0000\r\n\
         \r\n\
         Accented folder probe.\r\n";
    c.create_non_ascii_mailbox(&account, mutf7_name, message)
        .expect("create accented mailbox");

    let archive = tmp_archive("cyrus-non-ascii");
    let summary =
        import_imap::run(common(&archive), imap_config(&account, &c.imap)).expect("imap import");
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
