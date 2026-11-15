/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod integration;

use integration::Account;
use integration::baikal::Baikal;
use integration::layouts::{
    assert_contact_collection_exact, assert_contact_data_well_formed,
    assert_contact_uids_round_trip, assert_event_collection_exact, assert_event_data_well_formed,
    assert_event_uids_round_trip,
};
use integration::validate::{
    all_contact_uids, all_event_uids, assert_contact_round_trip, assert_event_round_trip, cleanup,
    common, count, open_archive, tmp_archive,
};

use vandelay::archive::sources::SourceChange;
use vandelay::error::Error;
use vandelay::import::dav::{DavImport, DavKindArg};
use vandelay::net::auth::Auth;

fn dav_config(kind: DavKindArg, account: &Account, dav_root: &str) -> DavImport {
    let path = match kind {
        DavKindArg::Caldav => format!("{dav_root}/calendars/{}/", account.username),
        DavKindArg::Carddav => format!("{dav_root}/addressbooks/{}/", account.username),
        DavKindArg::Webdav => format!("{dav_root}/"),
    };
    DavImport {
        kind,
        url: path,
        auth: Auth::Basic {
            user: account.username.clone(),
            password: account.password.clone(),
        },
        allow_cleartext: true,
        dav_connections: 2,
        multiget_batch: 25,
        source_change: SourceChange::Reject,
    }
}

fn dav_config_via_discovery(kind: DavKindArg, account: &Account, dav_root: &str) -> DavImport {
    DavImport {
        kind,
        url: format!("{dav_root}/"),
        auth: Auth::Basic {
            user: account.username.clone(),
            password: account.password.clone(),
        },
        allow_cleartext: true,
        dav_connections: 2,
        multiget_batch: 25,
        source_change: SourceChange::Reject,
    }
}

#[test]
#[ignore = "requires Docker"]
fn baikal_starts_seeds_and_imports() {
    let b = Baikal::start().expect("baikal start");
    let per_account = b.seed_all().expect("baikal seed");
    assert_eq!(
        per_account.len(),
        b.accounts.len(),
        "seed should return stats for every account"
    );
    b.verify_seed(&per_account).expect("baikal verify");

    let dav_root = b.dav_root();

    for seed in &per_account {
        let account = b
            .accounts
            .iter()
            .find(|a| a.username == seed.username)
            .expect("account lookup");
        assert!(
            !seed.calendars.is_empty(),
            "{}: expected calendars seeded",
            seed.username
        );
        assert!(
            !seed.address_books.is_empty(),
            "{}: expected address books seeded",
            seed.username
        );
        assert!(
            seed.total_events() > 0,
            "{}: expected events seeded",
            seed.username
        );
        assert!(
            seed.total_contacts() > 0,
            "{}: expected contacts seeded",
            seed.username
        );

        let cal_archive = tmp_archive(&format!("baikal-cal-{}", seed.username));
        let summary = dav_config(DavKindArg::Caldav, account, &dav_root)
            .run(common(&cal_archive))
            .into_result()
            .expect("caldav import");
        assert!(
            !summary.any_failed(),
            "{}: first caldav import had failures: {summary:?}",
            seed.username
        );
        let conn = open_archive(&cal_archive);
        assert_event_collection_exact(&conn, seed);
        assert_event_uids_round_trip(&conn, seed);
        assert_event_data_well_formed(&conn);
        for plan in &seed.calendars {
            for item in &plan.items {
                assert_event_round_trip(
                    &conn,
                    &item.source,
                    &item.uid,
                    &format!("{}/{}", seed.username, plan.display_name),
                );
            }
        }
        let cal_count = count(&conn, "calendars") as usize;
        let ev_count = count(&conn, "calendar_events") as usize;
        let blob_count = count(&conn, "blobs") as usize;
        drop(conn);

        let summary2 = dav_config(DavKindArg::Caldav, account, &dav_root)
            .run(common(&cal_archive))
            .into_result()
            .expect("idempotent caldav re-import");
        assert!(
            !summary2.any_failed(),
            "{}: idempotent caldav re-import had failures",
            seed.username
        );
        let conn = open_archive(&cal_archive);
        assert_eq!(
            count(&conn, "calendars") as usize,
            cal_count,
            "{}: idempotent re-import changed calendar count",
            seed.username
        );
        assert_eq!(
            count(&conn, "calendar_events") as usize,
            ev_count,
            "{}: idempotent re-import changed event count",
            seed.username
        );
        assert_eq!(
            count(&conn, "blobs") as usize,
            blob_count,
            "{}: idempotent re-import changed blob count",
            seed.username
        );
        drop(conn);

        let card_archive = tmp_archive(&format!("baikal-card-{}", seed.username));
        let summary = dav_config(DavKindArg::Carddav, account, &dav_root)
            .run(common(&card_archive))
            .into_result()
            .expect("carddav import");
        assert!(
            !summary.any_failed(),
            "{}: first carddav import had failures: {summary:?}",
            seed.username
        );
        let conn = open_archive(&card_archive);
        assert_contact_collection_exact(&conn, seed);
        assert_contact_uids_round_trip(&conn, seed);
        assert_contact_data_well_formed(&conn);
        for plan in &seed.address_books {
            for item in &plan.items {
                assert_contact_round_trip(
                    &conn,
                    &item.source,
                    &item.uid,
                    &format!("{}/{}", seed.username, plan.display_name),
                );
            }
        }
        let book_count = count(&conn, "address_books") as usize;
        let card_count = count(&conn, "contact_cards") as usize;
        drop(conn);

        let summary2 = dav_config(DavKindArg::Carddav, account, &dav_root)
            .run(common(&card_archive))
            .into_result()
            .expect("idempotent carddav re-import");
        assert!(
            !summary2.any_failed(),
            "{}: idempotent carddav re-import had failures",
            seed.username
        );
        let conn = open_archive(&card_archive);
        assert_eq!(
            count(&conn, "address_books") as usize,
            book_count,
            "{}: idempotent re-import changed address book count",
            seed.username
        );
        assert_eq!(
            count(&conn, "contact_cards") as usize,
            card_count,
            "{}: idempotent re-import changed contact count",
            seed.username
        );
        drop(conn);

        let vanish_href = seed.calendars[0]
            .items
            .first()
            .map(|i| i.href.clone())
            .expect("at least one event seeded");
        let vanish_uid = seed.calendars[0].items.first().unwrap().uid.clone();
        b.delete_item(account, &vanish_href)
            .expect("baikal delete vanished item");
        let summary_after_vanish = dav_config(DavKindArg::Caldav, account, &dav_root)
            .run(common(&cal_archive))
            .into_result()
            .expect("caldav re-import after vanish");
        assert!(
            !summary_after_vanish.any_failed(),
            "{}: re-import after vanish had failures",
            seed.username
        );
        let conn = open_archive(&cal_archive);
        assert_eq!(
            count(&conn, "calendar_events") as usize,
            ev_count - 1,
            "{}: vanished event was not pruned",
            seed.username
        );
        let uids_now = all_event_uids(&conn);
        assert!(
            !uids_now.contains(&vanish_uid),
            "{}: vanished event uid {vanish_uid} still in archive",
            seed.username
        );
        drop(conn);

        let icals = integration::data::load_icals().expect("load icals");
        let (_added_event_href, added_event_uid) = b
            .add_event(account, 0, &format!("bk-{}", seed.username), &icals)
            .expect("baikal add event");
        let summary_after_add_event = dav_config(DavKindArg::Caldav, account, &dav_root)
            .run(common(&cal_archive))
            .into_result()
            .expect("caldav re-import after add");
        assert!(
            !summary_after_add_event.any_failed(),
            "{}: re-import after add-event had failures",
            seed.username
        );
        let conn = open_archive(&cal_archive);
        let after_add_events = count(&conn, "calendar_events") as usize;
        assert_eq!(
            after_add_events, ev_count,
            "{}: add-then-import should restore count to baseline",
            seed.username
        );
        let uids_now = all_event_uids(&conn);
        assert!(
            uids_now.contains(&added_event_uid),
            "{}: added event uid {added_event_uid} not found in archive",
            seed.username
        );
        drop(conn);

        let vcards = integration::data::load_vcards().expect("load vcards");
        let (_added_card_href, added_card_uid) = b
            .add_contact(account, 0, &format!("bk-{}", seed.username), &vcards)
            .expect("baikal add contact");
        let summary_after_add_contact = dav_config(DavKindArg::Carddav, account, &dav_root)
            .run(common(&card_archive))
            .into_result()
            .expect("carddav re-import after add");
        assert!(
            !summary_after_add_contact.any_failed(),
            "{}: re-import after add-contact had failures",
            seed.username
        );
        let conn = open_archive(&card_archive);
        let after_add_contacts = count(&conn, "contact_cards") as usize;
        assert_eq!(
            after_add_contacts,
            card_count + 1,
            "{}: add-then-import should grow contact count by 1",
            seed.username
        );
        let imported_contact_uids = all_contact_uids(&conn);
        assert!(
            imported_contact_uids.contains(&added_card_uid),
            "{}: added contact uid {added_card_uid} not found",
            seed.username
        );
        drop(conn);

        let discovery_archive = tmp_archive(&format!("baikal-cal-discovery-{}", seed.username));
        let discovery_summary = dav_config_via_discovery(DavKindArg::Caldav, account, &dav_root)
            .run(common(&discovery_archive))
            .into_result()
            .expect("caldav import via discovery from dav root");
        assert!(
            !discovery_summary.any_failed(),
            "{}: caldav import via discovery had failures: {discovery_summary:?}",
            seed.username
        );
        let conn = open_archive(&discovery_archive);
        let discovered_events = count(&conn, "calendar_events") as usize;
        assert_eq!(
            discovered_events, ev_count,
            "{}: discovery-based import event count mismatch (server holds {ev_count} after vanish+add)",
            seed.username
        );
        drop(conn);
        cleanup(&discovery_archive);

        cleanup(&cal_archive);
        cleanup(&card_archive);
    }

    let primary = &b.accounts[0];
    let other = &b.accounts[1];
    let shared_archive = tmp_archive("baikal-source-change");
    dav_config(DavKindArg::Caldav, primary, &dav_root)
        .run(common(&shared_archive))
        .into_result()
        .expect("seed archive with primary user");
    let err = dav_config(DavKindArg::Caldav, other, &dav_root)
        .run(common(&shared_archive))
        .into_result()
        .expect_err("expected source-change abort");
    assert!(
        matches!(err, Error::SourceChange(_)),
        "expected SourceChange, got {err:?}"
    );
    cleanup(&shared_archive);

    b.stop().expect("baikal stop");
}

#[test]
#[ignore = "requires Docker"]
fn baikal_carddav_preserves_apple_item_labels() {
    let b = Baikal::start().expect("baikal start");
    let account = &b.accounts[0];
    let dav_root = b.dav_root();

    let client = integration::dav_client::DavSeed::new(
        dav_root.clone(),
        &account.username,
        &account.password,
    );
    let book = format!("/addressbooks/{}/ablabels/", account.username);
    let mkbook = r#"<?xml version="1.0" encoding="utf-8"?>
<d:mkcol xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav">
  <d:set><d:prop>
    <d:resourcetype><d:collection/><c:addressbook/></d:resourcetype>
    <d:displayname>AB Labels</d:displayname>
  </d:prop></d:set>
</d:mkcol>"#;
    client
        .mkcol(&book, Some(mkbook))
        .expect("mkcol addressbook");

    let uid = "apple-item-labels-1";
    let vcard = format!(
        "BEGIN:VCARD\r\n\
         VERSION:3.0\r\n\
         UID:{uid}\r\n\
         FN:Apple Labels\r\n\
         ITEM1.X-ABLABEL:Name1\r\n\
         ITEM2.X-ABLABEL:Name2\r\n\
         ITEM1.X-ABDATE:20171111\r\n\
         ITEM2.X-ABDATE:20111111\r\n\
         END:VCARD\r\n"
    );
    client
        .put(
            &format!("{book}apple.vcf"),
            "text/vcard; charset=utf-8",
            vcard.as_bytes(),
        )
        .expect("put vcard");

    let archive = tmp_archive("baikal-ablabels");
    let cfg = DavImport {
        kind: DavKindArg::Carddav,
        url: format!("{dav_root}/addressbooks/{}/", account.username),
        auth: Auth::Basic {
            user: account.username.clone(),
            password: account.password.clone(),
        },
        allow_cleartext: true,
        dav_connections: 2,
        multiget_batch: 25,
        source_change: SourceChange::Reject,
    };
    let summary = cfg
        .run(common(&archive))
        .into_result()
        .expect("carddav import");
    assert!(
        !summary.any_failed(),
        "carddav import had failures: {summary:?}"
    );

    let conn = open_archive(&archive);
    let data: String = conn
        .query_row(
            "SELECT data FROM contact_cards WHERE uid = ?1",
            [uid],
            |r| r.get(0),
        )
        .expect("apple-labels contact missing from archive");
    drop(conn);
    eprintln!("archived JSContact:\n{data}");

    for needle in [
        "x-abdate",
        "x-ablabel",
        "20171111",
        "20111111",
        "Name1",
        "Name2",
    ] {
        assert!(
            data.to_lowercase().contains(&needle.to_lowercase()),
            "Baikal CardDAV import dropped {needle:?}; stored: {data}"
        );
    }

    cleanup(&archive);
    b.stop().expect("baikal stop");
}
