/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod integration;

use std::collections::HashSet;

use integration::radicale::{AccountSeed, Radicale};
use integration::validate::{
    all_contact_uids, all_event_uids, assert_contact_round_trip, assert_event_round_trip,
    calendar_event_count, cleanup, collection_id_by_name, collection_names, common,
    contact_card_count, count, open_archive, tmp_archive,
};
use integration::{Account, Endpoint};

use rusqlite::Connection;
use serde_json::Value;
use vandelay::error::Error;
use vandelay::sync::import_dav;
use vandelay::sync::import_dav::{DavAuth, DavImportConfig, DavKindArg, SourceChange};

fn dav_config(kind: DavKindArg, account: &Account, http: &Endpoint) -> DavImportConfig {
    DavImportConfig {
        kind,
        url: format!("http://{}:{}/{}/", http.host, http.port, account.username),
        auth: DavAuth::Basic {
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
        let summary = import_dav::run(
            common(&cal_archive),
            dav_config(DavKindArg::Caldav, account, &r.endpoint),
        )
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

        let summary2 = import_dav::run(
            common(&cal_archive),
            dav_config(DavKindArg::Caldav, account, &r.endpoint),
        )
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
        let summary = import_dav::run(
            common(&card_archive),
            dav_config(DavKindArg::Carddav, account, &r.endpoint),
        )
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

        let summary2 = import_dav::run(
            common(&card_archive),
            dav_config(DavKindArg::Carddav, account, &r.endpoint),
        )
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
        let summary_after_vanish = import_dav::run(
            common(&cal_archive),
            dav_config(DavKindArg::Caldav, account, &r.endpoint),
        )
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
        let summary_after_add_event = import_dav::run(
            common(&cal_archive),
            dav_config(DavKindArg::Caldav, account, &r.endpoint),
        )
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
        let summary_after_add_contact = import_dav::run(
            common(&card_archive),
            dav_config(DavKindArg::Carddav, account, &r.endpoint),
        )
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
    import_dav::run(
        common(&shared_archive),
        dav_config(DavKindArg::Caldav, primary, &r.endpoint),
    )
    .expect("seed archive with primary user");
    let err = import_dav::run(
        common(&shared_archive),
        dav_config(DavKindArg::Caldav, other, &r.endpoint),
    )
    .expect_err("expected source-change abort when pointing at second user");
    assert!(
        matches!(err, Error::SourceChange(_)),
        "expected SourceChange, got {err:?}"
    );
    cleanup(&shared_archive);

    r.stop().expect("radicale stop");
}

fn assert_event_collection_exact(conn: &Connection, seed: &AccountSeed) {
    let names = collection_names(conn, "calendars");
    let expected: HashSet<String> = seed
        .calendars
        .iter()
        .map(|c| c.display_name.clone())
        .collect();
    assert_eq!(
        names, expected,
        "{}: calendar displayname set mismatch",
        seed.username
    );
    for plan in &seed.calendars {
        let cal_id = collection_id_by_name(conn, "calendars", &plan.display_name)
            .unwrap_or_else(|| panic!("calendar {} missing", plan.display_name));
        let got = calendar_event_count(conn, cal_id);
        assert_eq!(
            got as usize,
            plan.items.len(),
            "{}: calendar {} event count {got} != seeded {}",
            seed.username,
            plan.display_name,
            plan.items.len()
        );
    }
}

fn assert_contact_collection_exact(conn: &Connection, seed: &AccountSeed) {
    let names = collection_names(conn, "address_books");
    let expected: HashSet<String> = seed
        .address_books
        .iter()
        .map(|c| c.display_name.clone())
        .collect();
    assert_eq!(
        names, expected,
        "{}: address book displayname set mismatch",
        seed.username
    );
    for plan in &seed.address_books {
        let book_id = collection_id_by_name(conn, "address_books", &plan.display_name)
            .unwrap_or_else(|| panic!("address book {} missing", plan.display_name));
        let got = contact_card_count(conn, book_id);
        assert_eq!(
            got as usize,
            plan.items.len(),
            "{}: book {} contact count {got} != seeded {}",
            seed.username,
            plan.display_name,
            plan.items.len()
        );
    }
}

fn assert_event_uids_round_trip(conn: &Connection, seed: &AccountSeed) {
    let imported = all_event_uids(conn);
    let expected = seed.event_uids();
    let missing: Vec<_> = expected.difference(&imported).collect();
    assert!(
        missing.is_empty(),
        "{}: event uids missing from archive: {missing:?}",
        seed.username
    );
}

fn assert_contact_uids_round_trip(conn: &Connection, seed: &AccountSeed) {
    let imported = all_contact_uids(conn);
    let expected = seed.contact_uids();
    let missing: Vec<_> = expected.difference(&imported).collect();
    assert!(
        missing.is_empty(),
        "{}: contact uids missing from archive: {missing:?}",
        seed.username
    );
}

fn assert_event_data_well_formed(conn: &Connection) {
    let mut stmt = conn
        .prepare("SELECT data, data_type FROM calendar_events")
        .expect("prepare");
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .expect("query")
        .filter_map(|r| r.ok())
        .collect();
    assert!(!rows.is_empty(), "no calendar_events rows");
    for (raw, data_type) in rows {
        assert_eq!(
            data_type, "Event",
            "all VEVENT seeds must land as Event, got data_type={data_type}, raw={raw}"
        );
        let v: Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("calendar_events.data invalid JSON: {e}; raw={raw}"));
        let obj = v.as_object().expect("event data is object");
        let kind = obj
            .get("@type")
            .or_else(|| obj.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        assert_eq!(kind, "Event", "JSCalendar @type must be Event, got {kind}");
        assert!(obj.contains_key("uid"), "event JSON missing uid: {obj:?}");
        assert!(
            obj.contains_key("start"),
            "event JSON missing start: {obj:?}"
        );
    }
}

fn assert_contact_data_well_formed(conn: &Connection) {
    let mut stmt = conn
        .prepare("SELECT uid, data FROM contact_cards")
        .expect("prepare");
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .expect("query")
        .filter_map(|r| r.ok())
        .collect();
    assert!(!rows.is_empty(), "no contact_cards rows");
    for (uid, raw) in rows {
        assert!(!uid.is_empty(), "contact uid empty");
        let v: Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("contact_cards.data invalid JSON: {e}; raw={raw}"));
        let obj = v.as_object().expect("contact data is object");
        let kind = obj
            .get("@type")
            .or_else(|| obj.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        assert_eq!(kind, "Card", "JSContact @type must be Card, got {kind}");
    }
}
