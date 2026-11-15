/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::params;

use super::{TypeColumn, flat_id_table};
use crate::types::ObjectType;

flat_id_table!(
    TakeoutIds,
    takeout_ids,
    "sync_id_takeout",
    "source_obj_id",
    TypeColumn::Token,
    "INSERT"
);

impl TakeoutIds<'_> {
    pub fn count(&self, ty: ObjectType) -> rusqlite::Result<u64> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sync_id_takeout WHERE source_id = ?1 AND type_name = ?2",
            params![self.source, Self::TYPE_COLUMN.encode(ty)],
            |row| row.get(0),
        )?;
        Ok(count.unsigned_abs())
    }
}

#[cfg(test)]
mod tests {
    use crate::archive::sources::{SourceKey, SourceKind};
    use crate::archive::{Archive, LocalId};
    use crate::types::ObjectType;

    fn setup() -> Archive {
        Archive::open_in_memory().unwrap()
    }

    fn ids(archive: &Archive) -> super::TakeoutIds<'_> {
        let sid = archive
            .store()
            .sources()
            .upsert(
                &SourceKey {
                    kind: SourceKind::Takeout,
                    url: "file:///tmp/Takeout",
                    account_id: "/tmp/Takeout",
                },
                Some("Takeout"),
                "",
            )
            .unwrap();
        archive.store().takeout_ids(sid)
    }

    fn id(raw: i64) -> LocalId {
        LocalId::from_raw(raw)
    }

    #[test]
    fn insert_and_lookup_roundtrip() {
        let archive = setup();
        let ids = ids(&archive);
        ids.insert(ObjectType::Mailbox, "Inbox", id(7)).unwrap();
        ids.insert(ObjectType::Mailbox, "Label/Sub", id(8)).unwrap();
        assert_eq!(
            ids.local_id(ObjectType::Mailbox, "Inbox").unwrap(),
            Some(id(7))
        );
        assert_eq!(
            ids.local_id(ObjectType::Mailbox, "Label/Sub").unwrap(),
            Some(id(8))
        );
        assert_eq!(ids.local_id(ObjectType::Mailbox, "Missing").unwrap(), None);
    }

    #[test]
    fn stored_type_names_are_unchanged() {
        let archive = setup();
        let ids = ids(&archive);
        let types = [
            (ObjectType::Mailbox, "mailbox"),
            (ObjectType::Email, "email"),
            (ObjectType::AddressBook, "addressbook"),
            (ObjectType::ContactCard, "contactcard"),
            (ObjectType::Calendar, "calendar"),
            (ObjectType::CalendarEvent, "calendarevent"),
        ];
        for (n, (ty, _)) in (1..).zip(types) {
            ids.insert(ty, "x", id(n)).unwrap();
        }
        for (ty, stored) in types {
            let n: i64 = archive
                .connection()
                .query_row(
                    "SELECT COUNT(*) FROM sync_id_takeout WHERE type_name = ?1",
                    [stored],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "{stored}");
            assert_eq!(ids.all(ty).unwrap().len(), 1);
            assert_eq!(ids.count(ty).unwrap(), 1);
        }
    }

    #[test]
    fn delete_removes_only_target_row() {
        let archive = setup();
        let ids = ids(&archive);
        ids.insert(ObjectType::Email, "hash-a", id(1)).unwrap();
        ids.insert(ObjectType::Email, "hash-b", id(2)).unwrap();
        ids.delete(ObjectType::Email, "hash-a").unwrap();
        assert_eq!(ids.local_id(ObjectType::Email, "hash-a").unwrap(), None);
        assert_eq!(
            ids.local_id(ObjectType::Email, "hash-b").unwrap(),
            Some(id(2))
        );
    }

    #[test]
    fn pk_rejects_duplicate_source_obj_id() {
        let archive = setup();
        let ids = ids(&archive);
        ids.insert(ObjectType::Email, "h", id(1)).unwrap();
        let err = ids.insert(ObjectType::Email, "h", id(2)).unwrap_err();
        assert!(matches!(err, rusqlite::Error::SqliteFailure(_, _)));
    }

    #[test]
    fn unique_constraint_rejects_duplicate_local_id_within_type() {
        let archive = setup();
        let ids = ids(&archive);
        ids.insert(ObjectType::Email, "h1", id(42)).unwrap();
        let err = ids.insert(ObjectType::Email, "h2", id(42)).unwrap_err();
        assert!(matches!(err, rusqlite::Error::SqliteFailure(_, _)));
    }

    #[test]
    fn same_local_id_allowed_across_different_types() {
        let archive = setup();
        let ids = ids(&archive);
        ids.insert(ObjectType::Mailbox, "Inbox", id(42)).unwrap();
        ids.insert(ObjectType::Email, "h1", id(42)).unwrap();
    }

    #[test]
    fn all_returns_full_map() {
        let archive = setup();
        let ids = ids(&archive);
        for (n, hash) in (1..).zip(["h1", "h2", "h3"]) {
            ids.insert(ObjectType::Email, hash, id(n)).unwrap();
        }
        let all = ids.all(ObjectType::Email).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all.get("h1"), Some(&id(1)));
        assert_eq!(all.get("h3"), Some(&id(3)));
        assert_eq!(ids.count(ObjectType::Email).unwrap(), 3);
    }
}
