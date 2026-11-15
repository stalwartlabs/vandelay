/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use super::{TypeColumn, flat_id_table};

flat_id_table!(
    GraphIds,
    graph_ids,
    "sync_id_exchange_graph",
    "graph_id",
    TypeColumn::Token,
    "INSERT"
);

#[cfg(test)]
mod tests {
    use crate::archive::sources::{SourceKey, SourceKind};
    use crate::archive::{Archive, LocalId, SourceId};
    use crate::types::ObjectType;

    fn setup() -> (Archive, SourceId) {
        let archive = Archive::open_in_memory().unwrap();
        let key = SourceKey {
            kind: SourceKind::ExchangeGraph,
            url: "https://x|https://graph.microsoft.com/v1.0",
            account_id: "u-uuid",
        };
        let sid = archive
            .store()
            .sources()
            .upsert(&key, Some("u@d"), "u@d")
            .unwrap();
        (archive, sid)
    }

    #[test]
    fn insert_and_lookup_round_trip() {
        let (archive, sid) = setup();
        let ids = archive.store().graph_ids(sid);
        ids.insert(ObjectType::Email, "AAkA-base64==", LocalId::from_raw(17))
            .unwrap();
        let id = ids
            .local_id(ObjectType::Email, "AAkA-base64==")
            .unwrap()
            .unwrap();
        assert_eq!(id, LocalId::from_raw(17));
    }

    #[test]
    fn graph_id_is_case_sensitive() {
        let (archive, sid) = setup();
        let ids = archive.store().graph_ids(sid);
        ids.insert(ObjectType::Email, "AaBb", LocalId::from_raw(1))
            .unwrap();
        assert!(ids.local_id(ObjectType::Email, "aabb").unwrap().is_none());
        assert!(ids.local_id(ObjectType::Email, "AaBb").unwrap().is_some());
    }

    #[test]
    fn unique_local_id_per_type_is_enforced() {
        let (archive, sid) = setup();
        let ids = archive.store().graph_ids(sid);
        ids.insert(ObjectType::Email, "ID-A", LocalId::from_raw(1))
            .unwrap();
        let err = ids
            .insert(ObjectType::Email, "ID-B", LocalId::from_raw(1))
            .unwrap_err();
        assert!(err.to_string().contains("UNIQUE"));
    }

    #[test]
    fn all_filters_by_type() {
        let (archive, sid) = setup();
        let ids = archive.store().graph_ids(sid);
        ids.insert(ObjectType::Email, "M1", LocalId::from_raw(1))
            .unwrap();
        ids.insert(ObjectType::Email, "M2", LocalId::from_raw(2))
            .unwrap();
        ids.insert(ObjectType::Mailbox, "F1", LocalId::from_raw(3))
            .unwrap();
        let emails = ids.all(ObjectType::Email).unwrap();
        let folders = ids.all(ObjectType::Mailbox).unwrap();
        assert_eq!(emails.len(), 2);
        assert_eq!(folders.len(), 1);
        assert_eq!(emails.get("M1"), Some(&LocalId::from_raw(1)));
        assert_eq!(folders.get("F1"), Some(&LocalId::from_raw(3)));
    }

    #[test]
    fn delete_removes_only_one() {
        let (archive, sid) = setup();
        let ids = archive.store().graph_ids(sid);
        ids.insert(ObjectType::Email, "M1", LocalId::from_raw(1))
            .unwrap();
        ids.insert(ObjectType::Email, "M2", LocalId::from_raw(2))
            .unwrap();
        ids.delete(ObjectType::Email, "M1").unwrap();
        let remaining = ids.all(ObjectType::Email).unwrap();
        assert_eq!(remaining.len(), 1);
        assert!(remaining.contains_key("M2"));
    }
}
