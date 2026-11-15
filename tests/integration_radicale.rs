/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod integration;

use integration::layouts::{
    assert_contact_collection_exact, assert_contact_data_well_formed,
    assert_contact_uids_round_trip, assert_event_collection_exact, assert_event_data_well_formed,
    assert_event_uids_round_trip,
};
use integration::radicale::Radicale;
use integration::validate::{
    all_contact_uids, all_event_uids, assert_contact_round_trip, assert_event_round_trip,
    calendar_event_count, cleanup, collection_id_by_name, common, count, open_archive, tmp_archive,
};
use integration::{Account, Endpoint};

use vandelay::archive::sources::SourceChange;
use vandelay::error::Error;
use vandelay::import::dav::{DavImport, DavKindArg};
use vandelay::net::auth::Auth;

fn dav_config(kind: DavKindArg, account: &Account, http: &Endpoint) -> DavImport {
    DavImport {
        kind,
        url: format!("http://{}:{}/{}/", http.host, http.port, account.username),
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
fn radicale_starts_seeds_and_imports() {
    let r = Radicale::start().expect("radicale start");
    let per_account = r.seed_all().expect("radicale seed");
    assert_eq!(
        per_account.len(),
        r.accounts.len(),
        "seed should return stats for every account"
    );
    r.verify_seed(&per_account).expect("radicale verify");

    for seed in &per_account {
        let account = r
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

        let cal_archive = tmp_archive(&format!("radicale-cal-{}", seed.username));
        let summary = dav_config(DavKindArg::Caldav, account, &r.endpoint)
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

        let summary2 = dav_config(DavKindArg::Caldav, account, &r.endpoint)
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

        let card_archive = tmp_archive(&format!("radicale-card-{}", seed.username));
        let summary = dav_config(DavKindArg::Carddav, account, &r.endpoint)
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

        let summary2 = dav_config(DavKindArg::Carddav, account, &r.endpoint)
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

        let vanish_target_href = seed.calendars[0]
            .items
            .first()
            .map(|i| i.href.clone())
            .expect("at least one event seeded");
        let vanish_uid = seed.calendars[0]
            .items
            .first()
            .map(|i| i.uid.clone())
            .expect("uid");
        r.delete_item(account, &vanish_target_href)
            .expect("radicale delete vanished item");
        let summary_after_vanish = dav_config(DavKindArg::Caldav, account, &r.endpoint)
            .run(common(&cal_archive))
            .into_result()
            .expect("caldav re-import after vanish");
        assert!(
            !summary_after_vanish.any_failed(),
            "{}: re-import after vanish had failures",
            seed.username
        );
        let conn = open_archive(&cal_archive);
        let after_vanish_events = count(&conn, "calendar_events") as usize;
        assert_eq!(
            after_vanish_events,
            ev_count - 1,
            "{}: vanished event was not pruned from archive",
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
        let (_added_event_href, added_event_uid) = r
            .add_event(account, 0, &format!("rad-{}", seed.username), &icals)
            .expect("radicale add event");
        let summary_after_add_event = dav_config(DavKindArg::Caldav, account, &r.endpoint)
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
            after_add_events,
            ev_count,
            "{}: add-then-import should restore count to baseline (was {} after vanish, expected {ev_count})",
            seed.username,
            ev_count - 1
        );
        let uids_now = all_event_uids(&conn);
        assert!(
            uids_now.contains(&added_event_uid),
            "{}: added event uid {added_event_uid} not found in archive after re-import",
            seed.username
        );
        let added_cal_id =
            collection_id_by_name(&conn, "calendars", &seed.calendars[0].display_name)
                .expect("calendar id");
        let new_calendar_count = calendar_event_count(&conn, added_cal_id);
        assert_eq!(
            new_calendar_count as usize,
            seed.calendars[0].items.len(),
            "{}: target calendar event count after add+vanish should match original",
            seed.username
        );
        drop(conn);

        let vcards = integration::data::load_vcards().expect("load vcards");
        let (_added_card_href, added_card_uid) = r
            .add_contact(account, 0, &format!("rad-{}", seed.username), &vcards)
            .expect("radicale add contact");
        let summary_after_add_contact = dav_config(DavKindArg::Carddav, account, &r.endpoint)
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

        cleanup(&cal_archive);
        cleanup(&card_archive);
    }

    let primary = &r.accounts[0];
    let other = &r.accounts[1];
    let shared_archive = tmp_archive("radicale-source-change");
    dav_config(DavKindArg::Caldav, primary, &r.endpoint)
        .run(common(&shared_archive))
        .into_result()
        .expect("seed archive with primary user");
    let err = dav_config(DavKindArg::Caldav, other, &r.endpoint)
        .run(common(&shared_archive))
        .into_result()
        .expect_err("expected source-change abort when pointing at second user");
    assert!(
        matches!(err, Error::SourceChange(_)),
        "expected SourceChange, got {err:?}"
    );
    cleanup(&shared_archive);

    r.stop().expect("radicale stop");
}
