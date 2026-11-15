/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod integration;

use integration::cyrus::Cyrus;
use integration::layouts::ImapSuite;
use integration::validate::{cleanup, open_archive, tmp_archive};

use vandelay::net::stream::Cleartext;

fn suite(c: &Cyrus) -> ImapSuite<'_> {
    ImapSuite {
        server: "cyrus",
        imap: &c.imap,
        cleartext: Cleartext::Allow,
    }
}

#[test]
#[ignore = "requires Docker"]
fn cyrus_starts_seeds_and_imports() {
    let c = Cyrus::start().expect("cyrus start");
    let seeds = c.seed_all().expect("cyrus seed");
    c.verify_seed(&seeds).expect("cyrus verify");
    let suite = suite(&c);

    for seed in &seeds {
        let account = c
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

        let archive = tmp_archive(&format!("cyrus-{}", seed.username));
        suite.import(account, &archive, "imap import");

        let conn = open_archive(&archive);
        suite.assert_seeded(&conn, account, &seed.mailbox);

        if seed.mailbox.extras.dedup_target.is_some() && !account.layout.mailboxes.is_empty() {
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
        drop(conn);

        suite.assert_incremental(account, &archive);
        cleanup(&archive);
    }

    suite.assert_source_change(&c.accounts[0], &c.accounts[1]);
    c.stop().expect("cyrus stop");
}

#[test]
#[ignore = "requires Docker"]
fn cyrus_non_ascii_mailbox_names_round_trip() {
    let c = Cyrus::start().expect("cyrus start");
    let account = c.accounts.first().expect("account");
    suite(&c).assert_non_ascii_round_trip(account);
}
