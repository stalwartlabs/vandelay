/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */
use super::{TypeColumn, flat_id_table};

flat_id_table!(
    JmapIds,
    jmap_ids,
    "sync_id_jmap",
    "jmap_id",
    TypeColumn::JmapName,
    "INSERT OR REPLACE"
);

#[cfg(test)]
mod tests {
    use crate::archive::sources::{SourceKey, SourceKind};
    use crate::archive::{Archive, LocalId};
    use crate::types::ObjectType;

    #[test]
    fn crud_roundtrip() {
        let archive = Archive::open_in_memory().unwrap();
        let store = archive.store();
        let source = store
            .sources()
            .upsert(
                &SourceKey {
                    kind: SourceKind::Jmap,
                    url: "u",
                    account_id: "w",
                },
                None,
                "alice",
            )
            .unwrap();
        let ids = store.jmap_ids(source);
        ids.insert(ObjectType::Mailbox, "M1", LocalId(1)).unwrap();
        ids.insert(ObjectType::Mailbox, "M2", LocalId(2)).unwrap();
        assert_eq!(
            ids.local_id(ObjectType::Mailbox, "M1").unwrap(),
            Some(LocalId(1))
        );
        let map = ids.all(ObjectType::Mailbox).unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("M2"), Some(&LocalId(2)));
        ids.delete(ObjectType::Mailbox, "M1").unwrap();
        assert_eq!(ids.local_id(ObjectType::Mailbox, "M1").unwrap(), None);
    }

    #[test]
    fn jmap_ids_store_the_jmap_type_name_and_replace_on_conflict() {
        let archive = Archive::open_in_memory().unwrap();
        let store = archive.store();
        let source = store
            .sources()
            .upsert(
                &SourceKey {
                    kind: SourceKind::Jmap,
                    url: "u",
                    account_id: "w",
                },
                None,
                "alice",
            )
            .unwrap();
        let ids = store.jmap_ids(source);
        ids.insert(ObjectType::CalendarEvent, "E1", LocalId(7))
            .unwrap();
        ids.insert(ObjectType::CalendarEvent, "E1", LocalId(8))
            .unwrap();
        let stored: String = archive
            .connection()
            .query_row("SELECT type_name FROM sync_id_jmap", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, "CalendarEvent");
        assert_eq!(
            ids.local_id(ObjectType::CalendarEvent, "E1").unwrap(),
            Some(LocalId(8))
        );
        assert_eq!(ids.all(ObjectType::CalendarEvent).unwrap().len(), 1);
    }
}
