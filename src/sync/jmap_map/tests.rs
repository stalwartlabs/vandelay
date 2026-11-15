/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;

use encodify::base64::STANDARD;
use rusqlite::{Connection, Row, params};
use serde_json::Value;

use super::{EmailRow, FileNodeRow, SieveRow};
use crate::archive::rows::Write;
use crate::archive::{Archive, BlobId, LocalId, Store};
use crate::error::ExitStatus;
use crate::jmap::error::JmapError;
use crate::jmap::wire::JmapId;
use crate::jmap::wire::address_book::AddressBook;
use crate::jmap::wire::calendar::Calendar;
use crate::jmap::wire::calendar_event::CalendarEvent;
use crate::jmap::wire::contact_card::ContactCard;
use crate::jmap::wire::email::Email;
use crate::jmap::wire::file_node::{FileNode, NodeKind};
use crate::jmap::wire::identity::Identity;
use crate::jmap::wire::mailbox::Mailbox;
use crate::jmap::wire::participant_identity::ParticipantIdentity;
use crate::jmap::wire::sieve_script::SieveScript;
use crate::types::ObjectType;

const MAILBOX_SELECT: &str = Mailbox::SELECT;
const EMAIL_SELECT: &str = EmailRow::SELECT;
const IDENTITY_SELECT: &str = Identity::SELECT;
const SIEVE_SELECT: &str = SieveRow::SELECT;
const ADDRESS_BOOK_SELECT: &str = AddressBook::SELECT;
const CALENDAR_SELECT: &str = Calendar::SELECT;
const PARTICIPANT_IDENTITY_SELECT: &str =
    "SELECT id, name, calendar_address, is_default FROM participant_identities";
const FILE_NODE_SELECT: &str = FileNodeRow::SELECT;
const CONTACT_CARD_SELECT: &str = ContactCard::SELECT;
const CALENDAR_EVENT_SELECT: &str = CalendarEvent::SELECT;

struct MapResolver {
    to_local: HashMap<(ObjectType, String), i64>,
    to_target: HashMap<(ObjectType, i64), String>,
}

impl MapResolver {
    fn local(&self) -> impl Fn(ObjectType, &str) -> Option<LocalId> + '_ {
        |ty, id| {
            self.to_local
                .get(&(ty, id.to_owned()))
                .copied()
                .map(LocalId::from_raw)
        }
    }

    fn target(&self) -> impl Fn(ObjectType, LocalId) -> Option<JmapId> + '_ {
        |ty, id| {
            self.to_target
                .get(&(ty, id.get()))
                .map(|s| JmapId(s.clone()))
        }
    }
}

trait BlobIntern {
    fn intern(&mut self, jmap_blob_id: &str) -> Result<i64, JmapError>;
}

trait BlobBytes {
    fn bytes(&self, local_id: i64) -> Result<Vec<u8>, JmapError>;
}

struct FakeBlobs;

impl BlobIntern for FakeBlobs {
    fn intern(&mut self, _jmap_blob_id: &str) -> Result<i64, JmapError> {
        Ok(42)
    }
}

impl BlobBytes for FakeBlobs {
    fn bytes(&self, local_id: i64) -> Result<Vec<u8>, JmapError> {
        Ok(format!("payload-{local_id}").into_bytes())
    }
}

fn changed(write: Result<Write, JmapError>) -> Result<bool, JmapError> {
    write.map(|w| w == Write::Changed)
}

fn intern_blob(c: &Connection, bytes: &[u8]) -> rusqlite::Result<i64> {
    Store::new(c).blobs().intern(bytes).map(BlobId::get)
}

fn blob_id(id: i64) -> BlobId {
    BlobId::from_raw(id)
}

fn interner(blobs: &mut impl BlobIntern) -> impl FnMut(&str) -> Result<BlobId, JmapError> + '_ {
    |id| blobs.intern(id).map(BlobId::from_raw)
}

fn reader(blobs: &impl BlobBytes) -> impl Fn(BlobId) -> Result<Vec<u8>, JmapError> + '_ {
    |id| blobs.bytes(id.get())
}

fn insert_mailbox(c: &Connection, w: &Mailbox, r: &MapResolver) -> Result<i64, JmapError> {
    w.insert(Store::new(c), &r.local()).map(LocalId::get)
}

fn update_mailbox(
    c: &Connection,
    id: i64,
    w: &Mailbox,
    r: &MapResolver,
) -> Result<bool, JmapError> {
    changed(w.update(Store::new(c), LocalId::from_raw(id), &r.local()))
}

fn row_to_mailbox(row: &Row<'_>, r: &MapResolver) -> Result<Mailbox, JmapError> {
    Mailbox::from_row(row, &r.target())
}

fn insert_email(
    c: &Connection,
    w: &Email,
    blob: i64,
    message_match: &str,
    r: &MapResolver,
) -> Result<i64, JmapError> {
    w.insert(Store::new(c), blob_id(blob), message_match, &r.local())
        .map(LocalId::get)
}

fn update_email(c: &Connection, id: i64, obj: &Value, r: &MapResolver) -> Result<bool, JmapError> {
    changed(Email::update_placement(
        Store::new(c),
        LocalId::from_raw(id),
        obj,
        &r.local(),
    ))
}

fn row_to_email(row: &Row<'_>) -> Result<EmailRow, JmapError> {
    EmailRow::from_row(row)
}

fn insert_identity(c: &Connection, w: &Identity) -> Result<i64, JmapError> {
    w.insert(Store::new(c)).map(LocalId::get)
}

fn update_identity(c: &Connection, id: i64, w: &Identity) -> Result<bool, JmapError> {
    changed(w.update(Store::new(c), LocalId::from_raw(id)))
}

fn row_to_identity(row: &Row<'_>) -> Result<Identity, JmapError> {
    Identity::from_row(row)
}

fn insert_sieve_script(c: &Connection, w: &SieveScript, blob: i64) -> Result<i64, JmapError> {
    w.insert(Store::new(c), blob_id(blob)).map(LocalId::get)
}

fn update_sieve_script(
    c: &Connection,
    id: i64,
    w: &SieveScript,
    blob: i64,
) -> Result<bool, JmapError> {
    changed(w.update(Store::new(c), LocalId::from_raw(id), blob_id(blob)))
}

fn row_to_sieve_script(row: &Row<'_>) -> Result<SieveRow, JmapError> {
    SieveRow::from_row(row)
}

fn insert_address_book(c: &Connection, w: &AddressBook) -> Result<i64, JmapError> {
    w.insert(Store::new(c)).map(LocalId::get)
}

fn update_address_book(c: &Connection, id: i64, w: &AddressBook) -> Result<bool, JmapError> {
    changed(w.update(Store::new(c), LocalId::from_raw(id)))
}

fn row_to_address_book(row: &Row<'_>) -> Result<AddressBook, JmapError> {
    AddressBook::from_row(row)
}

fn insert_calendar(c: &Connection, w: &Calendar) -> Result<i64, JmapError> {
    w.insert(Store::new(c)).map(LocalId::get)
}

fn update_calendar(c: &Connection, id: i64, w: &Calendar) -> Result<bool, JmapError> {
    changed(w.update(Store::new(c), LocalId::from_raw(id)))
}

fn row_to_calendar(row: &Row<'_>) -> Result<Calendar, JmapError> {
    Calendar::from_row(row)
}

fn insert_participant_identity(c: &Connection, w: &ParticipantIdentity) -> Result<i64, JmapError> {
    w.insert(Store::new(c)).map(LocalId::get)
}

fn update_participant_identity(
    c: &Connection,
    id: i64,
    w: &ParticipantIdentity,
) -> Result<bool, JmapError> {
    changed(w.update(Store::new(c), LocalId::from_raw(id)))
}

fn row_to_participant_identity(row: &Row<'_>) -> Result<ParticipantIdentity, JmapError> {
    Ok(ParticipantIdentity {
        id: None,
        name: row.get(1)?,
        calendar_address: row.get(2)?,
        is_default: row.get::<_, i64>(3)? != 0,
    })
}

fn insert_file_node(
    c: &Connection,
    w: &FileNode,
    blob: Option<i64>,
    r: &MapResolver,
) -> Result<i64, JmapError> {
    w.insert(Store::new(c), blob.map(blob_id), &r.local())
        .map(LocalId::get)
}

fn update_file_node(
    c: &Connection,
    id: i64,
    w: &FileNode,
    blob: Option<i64>,
    r: &MapResolver,
) -> Result<bool, JmapError> {
    changed(w.update(
        Store::new(c),
        LocalId::from_raw(id),
        blob.map(blob_id),
        &r.local(),
    ))
}

fn row_to_file_node(row: &Row<'_>, r: &MapResolver) -> Result<FileNodeRow, JmapError> {
    FileNodeRow::from_row(row, &r.target())
}

fn insert_contact_card(
    c: &Connection,
    w: &ContactCard,
    r: &MapResolver,
    blobs: &mut impl BlobIntern,
) -> Result<i64, JmapError> {
    w.clone()
        .insert(Store::new(c), &r.local(), &mut interner(blobs))
        .map(LocalId::get)
}

fn update_contact_card(
    c: &Connection,
    id: i64,
    w: &ContactCard,
    r: &MapResolver,
    blobs: &mut impl BlobIntern,
) -> Result<bool, JmapError> {
    changed(w.clone().update(
        Store::new(c),
        LocalId::from_raw(id),
        &r.local(),
        &mut interner(blobs),
    ))
}

fn contact_card_to_wire(
    uid: &str,
    address_book_ids: &str,
    data: &str,
    r: &MapResolver,
    blobs: &impl BlobBytes,
) -> Result<Value, JmapError> {
    ContactCard::export_json(uid, address_book_ids, data, &r.target(), &reader(blobs))
}

fn insert_calendar_event(
    c: &Connection,
    w: &CalendarEvent,
    r: &MapResolver,
    blobs: &mut impl BlobIntern,
) -> Result<i64, JmapError> {
    w.clone()
        .insert(Store::new(c), &r.local(), &mut interner(blobs))
        .map(LocalId::get)
}

fn update_calendar_event(
    c: &Connection,
    id: i64,
    w: &CalendarEvent,
    r: &MapResolver,
    blobs: &mut impl BlobIntern,
) -> Result<bool, JmapError> {
    changed(w.clone().update(
        Store::new(c),
        LocalId::from_raw(id),
        &r.local(),
        &mut interner(blobs),
    ))
}

fn calendar_event_to_wire(
    calendar_ids: &str,
    is_draft: bool,
    use_default_alerts: bool,
    data: &str,
    r: &MapResolver,
    blobs: &impl BlobBytes,
) -> Result<Value, JmapError> {
    CalendarEvent::export_json(
        calendar_ids,
        is_draft,
        use_default_alerts,
        data,
        &r.target(),
        &reader(blobs),
    )
}

fn decode_data_uri(value: &Value, media_type: &str) -> Vec<u8> {
    let uri = value.as_str().unwrap_or_else(|| panic!("{value} is a URI"));
    let prefix = format!("data:{media_type};base64,");
    let payload = uri
        .strip_prefix(&prefix)
        .unwrap_or_else(|| panic!("{uri} does not start with {prefix}"));
    STANDARD.decode(payload).expect("base64 payload")
}

fn assert_no_blob_id(value: &Value) {
    match value {
        Value::Object(map) => {
            assert!(map.get("blobId").is_none(), "blobId present in {value}");
            assert!(map.get("@blob").is_none(), "@blob present in {value}");
            for child in map.values() {
                assert_no_blob_id(child);
            }
        }
        Value::Array(items) => items.iter().for_each(assert_no_blob_id),
        _ => {}
    }
}

fn mem() -> Archive {
    Archive::open_in_memory().unwrap()
}

#[test]
fn mailbox_roundtrips_with_parent_translation() {
    let archive = mem();
    let c = archive.connection();
    let root: Mailbox = serde_json::from_value(serde_json::json!({
        "id": "M1", "name": "Root", "role": "archive", "sortOrder": 3
    }))
    .unwrap();
    let empty = MapResolver {
        to_local: HashMap::new(),
        to_target: HashMap::new(),
    };
    let root_local = insert_mailbox(c, &root, &empty).unwrap();

    let child: Mailbox = serde_json::from_value(serde_json::json!({
        "id": "M2", "name": "Child", "parentId": "M1"
    }))
    .unwrap();
    let mut res = MapResolver {
        to_local: HashMap::new(),
        to_target: HashMap::new(),
    };
    res.to_local
        .insert((ObjectType::Mailbox, "M1".to_owned()), root_local);
    let child_local = insert_mailbox(c, &child, &res).unwrap();

    res.to_target
        .insert((ObjectType::Mailbox, root_local), "TGT1".to_owned());
    let wire = c
        .query_row(
            &format!("{MAILBOX_SELECT} WHERE id = ?1"),
            params![child_local],
            |row| Ok(row_to_mailbox(row, &res)),
        )
        .unwrap()
        .unwrap();
    assert_eq!(wire.name, "Child");
    assert_eq!(wire.parent_id, Some(JmapId("TGT1".to_owned())));
}

#[test]
fn email_keywords_keep_verbatim_case_and_mailbox_translation() {
    let archive = mem();
    let c = archive.connection();
    let mut res = MapResolver {
        to_local: HashMap::new(),
        to_target: HashMap::new(),
    };
    res.to_local
        .insert((ObjectType::Mailbox, "MB".to_owned()), 7);
    let email: Email = serde_json::from_value(serde_json::json!({
        "id": "E1",
        "blobId": "B1",
        "receivedAt": "2021-05-04T10:00:00Z",
        "mailboxIds": { "MB": true },
        "keywords": { "$Seen": true, "MyTag": true }
    }))
    .unwrap();
    let blob_id = intern_blob(c, b"rfc5322 bytes").unwrap();
    let local = insert_email(c, &email, blob_id, "{}", &res).unwrap();
    let got = c
        .query_row(
            &format!("{EMAIL_SELECT} WHERE id = ?1"),
            params![local],
            |row| Ok(row_to_email(row)),
        )
        .unwrap()
        .unwrap();
    assert_eq!(got.blob.get(), blob_id);
    assert_eq!(
        got.mailboxes
            .as_slice()
            .iter()
            .map(|l| l.get())
            .collect::<Vec<_>>(),
        vec![7]
    );
    assert_eq!(got.keywords, vec!["$Seen".to_owned(), "MyTag".to_owned()]);
}

#[test]
fn unresolved_foreign_key_on_insert_is_malformed() {
    let archive = mem();
    let c = archive.connection();
    let empty = MapResolver {
        to_local: HashMap::new(),
        to_target: HashMap::new(),
    };
    let email: Email = serde_json::from_value(serde_json::json!({
        "id": "E1",
        "blobId": "B1",
        "receivedAt": "2021-05-04T10:00:00Z",
        "mailboxIds": { "UNKNOWN": true },
        "keywords": {}
    }))
    .unwrap();
    let err = insert_email(c, &email, 1, "{}", &empty).unwrap_err();
    assert!(matches!(err, JmapError::Malformed(_)), "got {err:?}");
}

#[test]
fn unresolved_target_on_export_is_malformed() {
    let archive = mem();
    let c = archive.connection();
    let root: Mailbox = serde_json::from_value(serde_json::json!({
        "id": "M1", "name": "Root", "parentId": "P9"
    }))
    .unwrap();
    let mut res = MapResolver {
        to_local: HashMap::new(),
        to_target: HashMap::new(),
    };
    res.to_local
        .insert((ObjectType::Mailbox, "P9".to_owned()), 1);
    let local = insert_mailbox(c, &root, &res).unwrap();
    let err = c
        .query_row(
            &format!("{MAILBOX_SELECT} WHERE id = ?1"),
            params![local],
            |row| Ok(row_to_mailbox(row, &res)),
        )
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, JmapError::Malformed(_)), "got {err:?}");
}

#[test]
fn contact_card_strips_uid_and_inlines_media_as_data_uri() {
    let archive = mem();
    let c = archive.connection();
    let res = MapResolver {
        to_local: HashMap::from([((ObjectType::AddressBook, "AB".to_owned()), 1)]),
        to_target: HashMap::from([((ObjectType::AddressBook, 1), "TAB".to_owned())]),
    };
    let card: ContactCard = serde_json::from_value(serde_json::json!({
        "id": "C1",
        "addressBookIds": { "AB": true },
        "uid": "urn:uuid:42",
        "name": { "full": "Jane" },
        "media": { "photo": {
            "@type": "Media", "kind": "photo",
            "blobId": "PB", "mediaType": "image/png"
        } }
    }))
    .unwrap();
    let mut blobs = FakeBlobs;
    let local = insert_contact_card(c, &card, &res, &mut blobs).unwrap();
    let (uid, abids, data): (String, String, String) = c
        .query_row(
            &format!("{CONTACT_CARD_SELECT} WHERE id = ?1"),
            params![local],
            |r| Ok((r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(uid, "urn:uuid:42");
    let stored: Value = serde_json::from_str(&data).unwrap();
    assert!(stored.get("uid").is_none());
    assert_eq!(stored["media"]["photo"]["@blob"], Value::from(42));

    let up = FakeBlobs;
    let wire = contact_card_to_wire(&uid, &abids, &data, &res, &up).unwrap();
    assert_eq!(wire["uid"], Value::from("urn:uuid:42"));
    assert_eq!(wire["addressBookIds"]["TAB"], Value::Bool(true));
    assert_eq!(
        decode_data_uri(&wire["media"]["photo"]["uri"], "image/png"),
        b"payload-42"
    );
    assert_eq!(
        wire["media"]["photo"]["mediaType"],
        Value::from("image/png")
    );
    assert!(wire["media"]["photo"].get("href").is_none());
    assert_no_blob_id(&wire);
}

#[test]
fn calendar_event_inlines_link_enclosure_as_data_uri() {
    let archive = mem();
    let c = archive.connection();
    let res = MapResolver {
        to_local: HashMap::from([((ObjectType::Calendar, "CAL".to_owned()), 5)]),
        to_target: HashMap::from([((ObjectType::Calendar, 5), "TCAL".to_owned())]),
    };
    let ev: CalendarEvent = serde_json::from_value(serde_json::json!({
        "id": "EV1",
        "calendarIds": { "CAL": true },
        "uid": "ev-with-enclosure",
        "title": "Review",
        "@type": "Event",
        "links": { "1": {
            "@type": "Link", "rel": "enclosure",
            "blobId": "AB", "contentType": "text/plain", "title": "agenda.txt"
        } }
    }))
    .unwrap();
    let mut blobs = FakeBlobs;
    let local = insert_calendar_event(c, &ev, &res, &mut blobs).unwrap();
    let (cal, dr, ud, data): (String, i64, i64, String) = c
        .query_row(
            &format!("{CALENDAR_EVENT_SELECT} AND id = ?1"),
            params![local],
            |r| Ok((r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    let stored: Value = serde_json::from_str(&data).unwrap();
    assert_eq!(stored["links"]["1"]["@blob"], Value::from(42));

    let up = FakeBlobs;
    let wire = calendar_event_to_wire(&cal, dr != 0, ud != 0, &data, &res, &up).unwrap();
    assert_eq!(
        decode_data_uri(&wire["links"]["1"]["href"], "text/plain"),
        b"payload-42"
    );
    assert_eq!(wire["links"]["1"]["contentType"], Value::from("text/plain"));
    assert_eq!(wire["links"]["1"]["rel"], Value::from("enclosure"));
    assert!(wire["links"]["1"].get("uri").is_none());
    assert_no_blob_id(&wire);
}

#[test]
fn contact_card_media_without_media_type_defaults_to_octet_stream() {
    let archive = mem();
    let c = archive.connection();
    let res = MapResolver {
        to_local: HashMap::from([((ObjectType::AddressBook, "AB".to_owned()), 1)]),
        to_target: HashMap::from([((ObjectType::AddressBook, 1), "TAB".to_owned())]),
    };
    c.execute(
        "INSERT INTO contact_cards (id,uid,address_book_ids,data)
         VALUES (1,'u-1','[1]',?1)",
        params![
            serde_json::json!({ "@type": "Card", "media": { "photo": { "@blob": 9 } } })
                .to_string()
        ],
    )
    .unwrap();
    let (uid, abids, data): (String, String, String) = c
        .query_row(&format!("{CONTACT_CARD_SELECT} WHERE id = 1"), [], |r| {
            Ok((r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .unwrap();
    let up = FakeBlobs;
    let wire = contact_card_to_wire(&uid, &abids, &data, &res, &up).unwrap();
    assert_eq!(
        decode_data_uri(&wire["media"]["photo"]["uri"], "application/octet-stream"),
        b"payload-9"
    );
    assert_no_blob_id(&wire);
}

#[test]
fn an_archive_read_failure_while_inlining_is_an_archive_error_not_a_unit_failure() {
    struct BrokenBlobs;
    impl BlobBytes for BrokenBlobs {
        fn bytes(&self, _local_id: i64) -> Result<Vec<u8>, JmapError> {
            Err(JmapError::Sqlite(rusqlite::Error::QueryReturnedNoRows))
        }
    }
    let res = MapResolver {
        to_local: HashMap::new(),
        to_target: HashMap::from([
            ((ObjectType::AddressBook, 1), "TAB".to_owned()),
            ((ObjectType::Calendar, 1), "TCAL".to_owned()),
        ]),
    };
    let card =
        serde_json::json!({ "@type": "Card", "media": { "photo": { "@blob": 9 } } }).to_string();
    let event =
        serde_json::json!({ "@type": "Event", "links": { "1": { "@blob": 9 } } }).to_string();
    let failures = [
        contact_card_to_wire("u-1", "[1]", &card, &res, &BrokenBlobs).expect_err("card"),
        calendar_event_to_wire("[1]", false, false, &event, &res, &BrokenBlobs).expect_err("event"),
    ];
    for err in failures {
        assert!(matches!(err, JmapError::Sqlite(_)), "{err:?}");
        let mapped = crate::error::Error::from(err);
        assert!(mapped.aborts_run(), "{mapped} must abort the run");
        assert_eq!(mapped.exit_status(), ExitStatus::Archive);
    }
}

#[test]
fn calendar_event_extracts_columns_and_drops_method() {
    let archive = mem();
    let c = archive.connection();
    let res = MapResolver {
        to_local: HashMap::from([((ObjectType::Calendar, "CAL".to_owned()), 5)]),
        to_target: HashMap::from([((ObjectType::Calendar, 5), "TCAL".to_owned())]),
    };
    let ev: CalendarEvent = serde_json::from_value(serde_json::json!({
        "id": "EV1",
        "calendarIds": { "CAL": true },
        "isDraft": true,
        "useDefaultAlerts": false,
        "method": "request",
        "utcStart": "2020-01-01T00:00:00Z",
        "title": "Sprint",
        "@type": "Event"
    }))
    .unwrap();
    let mut blobs = FakeBlobs;
    let local = insert_calendar_event(c, &ev, &res, &mut blobs).unwrap();
    let (cal, dr, ud, data): (String, i64, i64, String) = c
        .query_row(
            &format!("{CALENDAR_EVENT_SELECT} AND id = ?1"),
            params![local],
            |r| Ok((r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(dr, 1);
    assert_eq!(ud, 0);
    let stored: Value = serde_json::from_str(&data).unwrap();
    assert!(stored.get("method").is_none());
    assert!(stored.get("utcStart").is_none());
    assert!(stored.get("calendarIds").is_none());
    assert_eq!(stored["title"], Value::from("Sprint"));

    let up = FakeBlobs;
    let wire = calendar_event_to_wire(&cal, dr != 0, ud != 0, &data, &res, &up).unwrap();
    assert_eq!(wire["calendarIds"]["TCAL"], Value::Bool(true));
    assert_eq!(wire["isDraft"], Value::Bool(true));
    assert_eq!(wire["title"], Value::from("Sprint"));
}

#[test]
fn identity_json_columns_roundtrip() {
    let archive = mem();
    let c = archive.connection();
    let id: Identity = serde_json::from_value(serde_json::json!({
        "id": "I1",
        "name": "Alice",
        "email": "a@x.test",
        "replyTo": [ { "name": "A", "email": "a@x.test" } ]
    }))
    .unwrap();
    let local = insert_identity(c, &id).unwrap();
    let wire = c
        .query_row(
            &format!("{IDENTITY_SELECT} WHERE id = ?1"),
            params![local],
            |row| Ok(row_to_identity(row)),
        )
        .unwrap()
        .unwrap();
    assert_eq!(wire.email, "a@x.test");
    assert_eq!(wire.reply_to.unwrap()[0].email, "a@x.test");
    assert!(wire.bcc.is_none());
}

#[test]
fn file_node_enum_and_target_roundtrip() {
    let archive = mem();
    let c = archive.connection();
    let empty = MapResolver {
        to_local: HashMap::new(),
        to_target: HashMap::new(),
    };
    let node: FileNode = serde_json::from_value(serde_json::json!({
        "id": "F1",
        "@type": "SymLink",
        "target": ["..", "etc", "hosts"],
        "name": "link",
        "created": "2022-02-02T02:02:02Z"
    }))
    .unwrap();
    let local = insert_file_node(c, &node, None, &empty).unwrap();
    let (nt, tgt): (String, Option<String>) = c
        .query_row(
            &format!("{FILE_NODE_SELECT} WHERE id = ?1"),
            params![local],
            |r| Ok((r.get(2)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(nt, "symlink");
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&tgt.unwrap()).unwrap(),
        vec!["..", "etc", "hosts"]
    );
}

#[test]
fn address_book_and_calendar_row_to_wire_roundtrip() {
    let archive = mem();
    let c = archive.connection();
    let ab: AddressBook = serde_json::from_value(serde_json::json!({
        "id": "A1", "name": "Personal", "description": "d", "sortOrder": 2,
        "isDefault": true, "isSubscribed": false
    }))
    .unwrap();
    let abl = insert_address_book(c, &ab).unwrap();
    let got = c
        .query_row(
            &format!("{ADDRESS_BOOK_SELECT} WHERE id = ?1"),
            params![abl],
            |r| Ok(row_to_address_book(r)),
        )
        .unwrap()
        .unwrap();
    assert_eq!(got.name, "Personal");
    assert!(got.is_default);
    assert!(!got.is_subscribed);

    let cal: Calendar = serde_json::from_value(serde_json::json!({
        "id": "C1", "name": "Work", "color": "#ff0000", "timeZone": "Europe/Rome",
        "isVisible": false
    }))
    .unwrap();
    let cl = insert_calendar(c, &cal).unwrap();
    let gc = c
        .query_row(
            &format!("{CALENDAR_SELECT} WHERE id = ?1"),
            params![cl],
            |r| Ok(row_to_calendar(r)),
        )
        .unwrap()
        .unwrap();
    assert_eq!(gc.name, "Work");
    assert_eq!(gc.color.as_deref(), Some("#ff0000"));
    assert_eq!(gc.time_zone.as_deref(), Some("Europe/Rome"));
    assert!(!gc.is_visible);
}

#[test]
fn participant_identity_and_sieve_row_to_wire_roundtrip() {
    let archive = mem();
    let c = archive.connection();
    let pi: ParticipantIdentity = serde_json::from_value(serde_json::json!({
        "id": "P1", "name": "Me", "calendarAddress": "mailto:me@x.test",
        "isDefault": true
    }))
    .unwrap();
    let pil = insert_participant_identity(c, &pi).unwrap();
    let gp = c
        .query_row(
            &format!("{PARTICIPANT_IDENTITY_SELECT} WHERE id = ?1"),
            params![pil],
            |r| Ok(row_to_participant_identity(r)),
        )
        .unwrap()
        .unwrap();
    assert_eq!(gp.calendar_address, "mailto:me@x.test");
    assert!(gp.is_default);

    let ss: SieveScript = serde_json::from_value(serde_json::json!({
        "id": "S1", "name": "main", "isActive": true, "blobId": "B1"
    }))
    .unwrap();
    let blob = intern_blob(c, b"keep;").unwrap();
    let ssl = insert_sieve_script(c, &ss, blob).unwrap();
    let gs = c
        .query_row(
            &format!("{SIEVE_SELECT} WHERE id = ?1"),
            params![ssl],
            |r| Ok(row_to_sieve_script(r)),
        )
        .unwrap()
        .unwrap();
    assert_eq!(gs.name.as_deref(), Some("main"));
    assert!(gs.is_active);
    assert_eq!(gs.blob.get(), blob);
}

#[test]
fn file_node_row_to_wire_translates_parent() {
    let archive = mem();
    let c = archive.connection();
    let mut res = MapResolver {
        to_local: HashMap::new(),
        to_target: HashMap::new(),
    };
    let dir: FileNode = serde_json::from_value(serde_json::json!({
        "id": "D1", "@type": "Directory", "name": "Docs",
        "created": "2022-02-02T02:02:02Z"
    }))
    .unwrap();
    let dirl = insert_file_node(c, &dir, None, &res).unwrap();
    res.to_local
        .insert((ObjectType::FileNode, "D1".to_owned()), dirl);
    let file: FileNode = serde_json::from_value(serde_json::json!({
        "id": "F1", "@type": "File", "parentId": "D1", "name": "a.bin",
        "mediaType": "application/octet-stream", "created": "2022-02-02T02:02:02Z"
    }))
    .unwrap();
    let blob = intern_blob(c, b"bytes").unwrap();
    let fl = insert_file_node(c, &file, Some(blob), &res).unwrap();
    res.to_target
        .insert((ObjectType::FileNode, dirl), "TGTD".to_owned());
    let got = c
        .query_row(
            &format!("{FILE_NODE_SELECT} WHERE id = ?1"),
            params![fl],
            |r| Ok(row_to_file_node(r, &res)),
        )
        .unwrap()
        .unwrap();
    assert_eq!(got.wire.name, "a.bin");
    assert_eq!(got.wire.parent_id, Some(JmapId("TGTD".to_owned())));
    assert_eq!(got.blob.map(BlobId::get), Some(blob));
}

// Each @type serializes only the properties the draft defines for it.
#[test]
fn file_node_row_to_wire_emits_only_type_specific_properties() {
    let archive = mem();
    let c = archive.connection();
    let mut res = MapResolver {
        to_local: HashMap::new(),
        to_target: HashMap::new(),
    };
    let nodes = [
        serde_json::json!({"id": "D1", "@type": "Directory", "name": "Docs",
            "role": "documents", "isSubscribed": false,
            "created": "2022-02-02T02:02:02Z"}),
        serde_json::json!({"id": "F1", "@type": "File", "name": "run.sh",
            "blobId": "B1", "mediaType": "text/x-sh", "executable": true, "size": 3,
            "created": "2022-02-02T02:02:02Z"}),
        serde_json::json!({"id": "L1", "@type": "SymLink", "name": "latest",
            "target": ["", "Docs"], "created": "2022-02-02T02:02:02Z"}),
    ];
    let blob = intern_blob(c, b"#!\n").unwrap();
    let mut out = Vec::new();
    for (i, n) in nodes.into_iter().enumerate() {
        let w: FileNode = serde_json::from_value(n).unwrap();
        let blob_local = matches!(w.kind, NodeKind::File(_)).then_some(blob);
        let l = insert_file_node(c, &w, blob_local, &res).unwrap();
        res.to_target
            .insert((ObjectType::FileNode, l), format!("T{i}"));
        let row = c
            .query_row(
                &format!("{FILE_NODE_SELECT} WHERE id = ?1"),
                params![l],
                |r| Ok(row_to_file_node(r, &res)),
            )
            .unwrap()
            .unwrap();
        out.push(serde_json::to_value(&row.wire).unwrap());
    }
    let keys = |v: &Value| {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort_unstable();
        k
    };
    assert_eq!(
        keys(&out[0]),
        ["@type", "created", "isSubscribed", "name", "role"]
    );
    assert_eq!(out[0]["@type"], "Directory");
    assert_eq!(out[0]["isSubscribed"], false);
    assert_eq!(
        keys(&out[1]),
        ["@type", "created", "executable", "mediaType", "name"]
    );
    assert_eq!(out[1]["executable"], true);
    assert_eq!(out[1]["mediaType"], "text/x-sh");
    assert_eq!(keys(&out[2]), ["@type", "created", "name", "target"]);
    assert_eq!(out[2]["@type"], "SymLink");
    assert_eq!(out[2]["target"], serde_json::json!(["", "Docs"]));
}

#[test]
fn file_node_rejects_unknown_type() {
    let r: Result<FileNode, _> = serde_json::from_value(serde_json::json!({
        "id": "X1", "@type": "Socket", "name": "s", "created": "2022-02-02T02:02:02Z"
    }));
    assert!(r.is_err());
}

fn one_row(c: &Connection, table: &str) -> i64 {
    c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

#[test]
fn delta_update_mailbox_in_place_preserves_id() {
    let archive = mem();
    let c = archive.connection();
    let empty = MapResolver {
        to_local: HashMap::new(),
        to_target: HashMap::new(),
    };
    let m: Mailbox =
        serde_json::from_value(serde_json::json!({"id":"M1","name":"Personal"})).unwrap();
    let local = insert_mailbox(c, &m, &empty).unwrap();
    let changed: Mailbox = serde_json::from_value(
        serde_json::json!({"id":"M1","name":"PersonalRenamed","role":"archive","sortOrder":4}),
    )
    .unwrap();
    assert!(
        update_mailbox(c, local, &changed, &empty).unwrap(),
        "a real change reports changed=true"
    );
    assert!(
        !update_mailbox(c, local, &changed, &empty).unwrap(),
        "re-applying identical values is a no-op (changed=false), so re-runs converge"
    );
    assert_eq!(one_row(c, "mailboxes"), 1);
    let (name, role, sort): (String, Option<String>, i64) = c
        .query_row(
            "SELECT name, role, sort_order FROM mailboxes WHERE id=?1",
            params![local],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(name, "PersonalRenamed");
    assert_eq!(role.as_deref(), Some("archive"));
    assert_eq!(sort, 4);
}

#[test]
fn delta_update_email_changes_keywords_and_mailboxes_keeps_blob() {
    let archive = mem();
    let c = archive.connection();
    let mut res = MapResolver {
        to_local: HashMap::new(),
        to_target: HashMap::new(),
    };
    res.to_local
        .insert((ObjectType::Mailbox, "MB".to_owned()), 7);
    res.to_local
        .insert((ObjectType::Mailbox, "MB2".to_owned()), 8);
    let email: Email = serde_json::from_value(serde_json::json!({
        "id":"E1","blobId":"B1","receivedAt":"2021-05-04T10:00:00Z",
        "mailboxIds":{"MB":true},"keywords":{"$seen":true}
    }))
    .unwrap();
    let blob = intern_blob(c, b"rfc5322").unwrap();
    let local = insert_email(c, &email, blob, "{}", &res).unwrap();

    let changed = serde_json::json!({
        "id":"E1","mailboxIds":{"MB2":true},"keywords":{"$seen":true,"$flagged":true}
    });
    update_email(c, local, &changed, &res).unwrap();
    assert_eq!(one_row(c, "emails"), 1);
    let got = c
        .query_row(
            &format!("{EMAIL_SELECT} WHERE id=?1"),
            params![local],
            |row| Ok(row_to_email(row)),
        )
        .unwrap()
        .unwrap();
    assert_eq!(got.blob.get(), blob, "immutable body blob is untouched");
    assert_eq!(
        got.mailboxes
            .as_slice()
            .iter()
            .map(|l| l.get())
            .collect::<Vec<_>>(),
        vec![8],
        "mailbox membership moved"
    );
    assert!(got.keywords.contains(&"$flagged".to_owned()));
    assert!(got.keywords.contains(&"$seen".to_owned()));
}

#[test]
fn delta_update_identity_in_place() {
    let archive = mem();
    let c = archive.connection();
    let id: Identity =
        serde_json::from_value(serde_json::json!({"id":"I1","name":"Old","email":"a@x.test"}))
            .unwrap();
    let local = insert_identity(c, &id).unwrap();
    let changed: Identity = serde_json::from_value(
        serde_json::json!({"id":"I1","name":"New Name","email":"a@x.test","textSignature":"sig"}),
    )
    .unwrap();
    update_identity(c, local, &changed).unwrap();
    assert_eq!(one_row(c, "identities"), 1);
    let got = c
        .query_row(
            &format!("{IDENTITY_SELECT} WHERE id=?1"),
            params![local],
            |row| Ok(row_to_identity(row)),
        )
        .unwrap()
        .unwrap();
    assert_eq!(got.name, "New Name");
    assert_eq!(got.text_signature, "sig");
}

#[test]
fn delta_update_address_book_in_place() {
    let archive = mem();
    let c = archive.connection();
    let ab: AddressBook =
        serde_json::from_value(serde_json::json!({"id":"A1","name":"Old"})).unwrap();
    let local = insert_address_book(c, &ab).unwrap();
    let changed: AddressBook = serde_json::from_value(
        serde_json::json!({"id":"A1","name":"Renamed","description":"d2","sortOrder":3}),
    )
    .unwrap();
    update_address_book(c, local, &changed).unwrap();
    assert_eq!(one_row(c, "address_books"), 1);
    let got = c
        .query_row(
            &format!("{ADDRESS_BOOK_SELECT} WHERE id=?1"),
            params![local],
            |r| Ok(row_to_address_book(r)),
        )
        .unwrap()
        .unwrap();
    assert_eq!(got.name, "Renamed");
    assert_eq!(got.description.as_deref(), Some("d2"));
}

#[test]
fn delta_update_calendar_in_place() {
    let archive = mem();
    let c = archive.connection();
    let cal: Calendar =
        serde_json::from_value(serde_json::json!({"id":"C1","name":"Old","color":"#000"})).unwrap();
    let local = insert_calendar(c, &cal).unwrap();
    let changed: Calendar = serde_json::from_value(
        serde_json::json!({"id":"C1","name":"Renamed","color":"#abcdef","timeZone":"Europe/Rome"}),
    )
    .unwrap();
    update_calendar(c, local, &changed).unwrap();
    assert_eq!(one_row(c, "calendars"), 1);
    let got = c
        .query_row(
            &format!("{CALENDAR_SELECT} WHERE id=?1"),
            params![local],
            |r| Ok(row_to_calendar(r)),
        )
        .unwrap()
        .unwrap();
    assert_eq!(got.name, "Renamed");
    assert_eq!(got.color.as_deref(), Some("#abcdef"));
    assert_eq!(got.time_zone.as_deref(), Some("Europe/Rome"));
}

#[test]
fn delta_update_participant_identity_in_place() {
    let archive = mem();
    let c = archive.connection();
    let pi: ParticipantIdentity = serde_json::from_value(
        serde_json::json!({"id":"P1","name":"Old","calendarAddress":"mailto:me@x.test"}),
    )
    .unwrap();
    let local = insert_participant_identity(c, &pi).unwrap();
    let changed: ParticipantIdentity = serde_json::from_value(
        serde_json::json!({"id":"P1","name":"New","calendarAddress":"mailto:me@x.test"}),
    )
    .unwrap();
    update_participant_identity(c, local, &changed).unwrap();
    assert_eq!(one_row(c, "participant_identities"), 1);
    let got = c
        .query_row(
            &format!("{PARTICIPANT_IDENTITY_SELECT} WHERE id=?1"),
            params![local],
            |r| Ok(row_to_participant_identity(r)),
        )
        .unwrap()
        .unwrap();
    assert_eq!(got.name, "New");
}

#[test]
fn delta_update_sieve_script_swaps_blob_and_active() {
    let archive = mem();
    let c = archive.connection();
    let ss: SieveScript = serde_json::from_value(
        serde_json::json!({"id":"S1","name":"main","isActive":false,"blobId":"B1"}),
    )
    .unwrap();
    let blob1 = intern_blob(c, b"keep;").unwrap();
    let local = insert_sieve_script(c, &ss, blob1).unwrap();
    let blob2 = intern_blob(c, b"discard;").unwrap();
    let changed: SieveScript = serde_json::from_value(
        serde_json::json!({"id":"S1","name":"main","isActive":true,"blobId":"B2"}),
    )
    .unwrap();
    update_sieve_script(c, local, &changed, blob2).unwrap();
    assert_eq!(one_row(c, "sieve_scripts"), 1);
    let got = c
        .query_row(
            &format!("{SIEVE_SELECT} WHERE id=?1"),
            params![local],
            |r| Ok(row_to_sieve_script(r)),
        )
        .unwrap()
        .unwrap();
    assert!(got.is_active);
    assert_eq!(got.blob.get(), blob2, "content blob swapped");
}

#[test]
fn delta_update_file_node_changes_blob_and_name() {
    let archive = mem();
    let c = archive.connection();
    let res = MapResolver {
        to_local: HashMap::new(),
        to_target: HashMap::new(),
    };
    let file: FileNode = serde_json::from_value(serde_json::json!({
        "id":"F1","@type":"File","name":"a.bin",
        "mediaType":"application/octet-stream","created":"2022-02-02T02:02:02Z"
    }))
    .unwrap();
    let blob1 = intern_blob(c, b"v1").unwrap();
    let local = insert_file_node(c, &file, Some(blob1), &res).unwrap();
    let blob2 = intern_blob(c, b"v2").unwrap();
    let changed: FileNode = serde_json::from_value(serde_json::json!({
        "id":"F1","@type":"File","name":"renamed.bin",
        "mediaType":"text/plain","executable":true,"created":"2022-02-02T02:02:02Z"
    }))
    .unwrap();
    update_file_node(c, local, &changed, Some(blob2), &res).unwrap();
    assert_eq!(one_row(c, "file_nodes"), 1);
    let (name, mt, b, x): (String, Option<String>, Option<i64>, i64) = c
        .query_row(
            "SELECT name, media_type, blob_id, executable FROM file_nodes WHERE id=?1",
            params![local],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(name, "renamed.bin");
    assert_eq!(mt.as_deref(), Some("text/plain"));
    assert_eq!(b, Some(blob2));
    assert_eq!(x, 1);
}

#[test]
fn delta_update_contact_card_rewrites_data_and_columns() {
    let archive = mem();
    let c = archive.connection();
    let res = MapResolver {
        to_local: HashMap::from([((ObjectType::AddressBook, "AB".to_owned()), 1)]),
        to_target: HashMap::new(),
    };
    let card: ContactCard = serde_json::from_value(serde_json::json!({
        "id":"C1","addressBookIds":{"AB":true},"uid":"u-1","name":{"full":"Old"}
    }))
    .unwrap();
    let mut blobs = FakeBlobs;
    let local = insert_contact_card(c, &card, &res, &mut blobs).unwrap();
    let changed: ContactCard = serde_json::from_value(serde_json::json!({
        "id":"C1","addressBookIds":{"AB":true},"uid":"u-1","name":{"full":"New Name"}
    }))
    .unwrap();
    update_contact_card(c, local, &changed, &res, &mut blobs).unwrap();
    assert_eq!(one_row(c, "contact_cards"), 1);
    let (uid, data): (String, String) = c
        .query_row(
            &format!("{CONTACT_CARD_SELECT} WHERE id=?1"),
            params![local],
            |r| Ok((r.get(1)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(uid, "u-1");
    let stored: Value = serde_json::from_str(&data).unwrap();
    assert_eq!(stored["name"]["full"], Value::from("New Name"));
}

#[test]
fn delta_update_calendar_event_rewrites_data_and_columns() {
    let archive = mem();
    let c = archive.connection();
    let res = MapResolver {
        to_local: HashMap::from([((ObjectType::Calendar, "CAL".to_owned()), 5)]),
        to_target: HashMap::new(),
    };
    let ev: CalendarEvent = serde_json::from_value(serde_json::json!({
        "id":"EV1","calendarIds":{"CAL":true},"isDraft":false,
        "useDefaultAlerts":false,"title":"Old","@type":"Event"
    }))
    .unwrap();
    let mut blobs = FakeBlobs;
    let local = insert_calendar_event(c, &ev, &res, &mut blobs).unwrap();
    let changed: CalendarEvent = serde_json::from_value(serde_json::json!({
        "id":"EV1","calendarIds":{"CAL":true},"isDraft":true,
        "useDefaultAlerts":false,"title":"Rescheduled","@type":"Event"
    }))
    .unwrap();
    update_calendar_event(c, local, &changed, &res, &mut blobs).unwrap();
    assert_eq!(one_row(c, "calendar_events"), 1);
    let (dr, data): (i64, String) = c
        .query_row(
            &format!("{CALENDAR_EVENT_SELECT} AND id=?1"),
            params![local],
            |r| Ok((r.get(2)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(dr, 1, "isDraft column updated");
    let stored: Value = serde_json::from_str(&data).unwrap();
    assert_eq!(stored["title"], Value::from("Rescheduled"));
}
