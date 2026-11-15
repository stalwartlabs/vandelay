/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

use crate::archive::{LocalId, SourceId, Store};
use crate::types::ObjectType;

#[derive(Debug, Clone)]
pub struct ItemRow {
    pub item_id: String,
    pub change_key: String,
    pub local_id: LocalId,
}

#[derive(Debug, Clone)]
pub struct FolderRow {
    pub item_id: String,
    pub folder_id: String,
    pub change_key: String,
    pub local_id: LocalId,
}

#[derive(Clone, Copy)]
pub struct EwsIds<'a> {
    conn: &'a Connection,
    source: SourceId,
}

impl<'a> Store<'a> {
    pub fn ews_ids(self, source: SourceId) -> EwsIds<'a> {
        EwsIds {
            conn: self.connection(),
            source,
        }
    }
}

impl EwsIds<'_> {
    pub fn insert(
        &self,
        ty: ObjectType,
        folder_id: &str,
        item_id: &str,
        change_key: &str,
        local: LocalId,
    ) -> rusqlite::Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO sync_id_exchange_ews
                 (source_id, type_name, folder_id, item_id, change_key, sync_state, local_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, '', ?6)",
            )?
            .execute(params![
                self.source,
                ty.token(),
                folder_id,
                item_id,
                change_key,
                local
            ])?;
        Ok(())
    }

    pub fn update_change_key(
        &self,
        ty: ObjectType,
        item_id: &str,
        change_key: &str,
    ) -> rusqlite::Result<()> {
        self.conn
            .prepare_cached(
                "UPDATE sync_id_exchange_ews SET change_key = ?4
                 WHERE source_id = ?1 AND type_name = ?2 AND item_id = ?3",
            )?
            .execute(params![self.source, ty.token(), item_id, change_key])?;
        Ok(())
    }

    pub fn delete(&self, ty: ObjectType, item_id: &str) -> rusqlite::Result<()> {
        self.conn
            .prepare_cached(
                "DELETE FROM sync_id_exchange_ews
                 WHERE source_id = ?1 AND type_name = ?2 AND item_id = ?3",
            )?
            .execute(params![self.source, ty.token(), item_id])?;
        Ok(())
    }

    pub fn local_id(&self, ty: ObjectType, item_id: &str) -> rusqlite::Result<Option<LocalId>> {
        self.conn
            .prepare_cached(
                "SELECT local_id FROM sync_id_exchange_ews
                 WHERE source_id = ?1 AND type_name = ?2 AND item_id = ?3",
            )?
            .query_row(params![self.source, ty.token(), item_id], |row| row.get(0))
            .optional()
    }

    pub fn items_in(&self, ty: ObjectType, folder_id: &str) -> rusqlite::Result<Vec<ItemRow>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT item_id, change_key, local_id FROM sync_id_exchange_ews
             WHERE source_id = ?1 AND type_name = ?2 AND folder_id = ?3",
        )?;
        let rows = stmt.query_map(params![self.source, ty.token(), folder_id], |row| {
            Ok(ItemRow {
                item_id: row.get(0)?,
                change_key: row.get(1)?,
                local_id: row.get(2)?,
            })
        })?;
        rows.collect()
    }

    pub fn all(&self, ty: ObjectType) -> rusqlite::Result<HashMap<String, FolderRow>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT item_id, folder_id, change_key, local_id FROM sync_id_exchange_ews
             WHERE source_id = ?1 AND type_name = ?2",
        )?;
        let rows = stmt.query_map(params![self.source, ty.token()], |row| {
            Ok(FolderRow {
                item_id: row.get(0)?,
                folder_id: row.get(1)?,
                change_key: row.get(2)?,
                local_id: row.get(3)?,
            })
        })?;
        rows.map(|row| row.map(|row| (row.item_id.clone(), row)))
            .collect()
    }

    pub fn sync_state(&self, ty: ObjectType, item_id: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .prepare_cached(
                "SELECT sync_state FROM sync_id_exchange_ews
                 WHERE source_id = ?1 AND type_name = ?2 AND item_id = ?3",
            )?
            .query_row(params![self.source, ty.token(), item_id], |row| row.get(0))
            .optional()
    }

    pub fn set_sync_state(
        &self,
        ty: ObjectType,
        item_id: &str,
        sync_state: &str,
    ) -> rusqlite::Result<()> {
        self.conn
            .prepare_cached(
                "UPDATE sync_id_exchange_ews SET sync_state = ?4
                 WHERE source_id = ?1 AND type_name = ?2 AND item_id = ?3",
            )?
            .execute(params![self.source, ty.token(), item_id, sync_state])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Archive;
    use crate::archive::sources::{SourceKey, SourceKind};

    const MAILBOX: ObjectType = ObjectType::Mailbox;
    const EMAIL: ObjectType = ObjectType::Email;

    fn setup() -> (Archive, SourceId) {
        let archive = Archive::open_in_memory().unwrap();
        let key = SourceKey {
            kind: SourceKind::ExchangeEws,
            url: "https://x/EWS/Exchange.asmx",
            account_id: "u@d",
        };
        let sid = archive
            .store()
            .sources()
            .upsert(&key, Some("u"), "u@d")
            .unwrap();
        (archive, sid)
    }

    fn id(raw: i64) -> LocalId {
        LocalId::from_raw(raw)
    }

    #[test]
    fn insert_and_lookup_round_trip() {
        let (archive, sid) = setup();
        let ids = archive.store().ews_ids(sid);
        ids.insert(EMAIL, "FOLDER1", "ITEM1", "CK1", id(42))
            .unwrap();
        assert_eq!(ids.local_id(EMAIL, "ITEM1").unwrap(), Some(id(42)));
        let rows = ids.items_in(EMAIL, "FOLDER1").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].change_key, "CK1");
        let token: String = archive
            .connection()
            .query_row("SELECT type_name FROM sync_id_exchange_ews", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(token, "email");
    }

    #[test]
    fn update_change_key_preserves_local_id() {
        let (archive, sid) = setup();
        let ids = archive.store().ews_ids(sid);
        ids.insert(EMAIL, "F1", "I1", "A", id(7)).unwrap();
        ids.update_change_key(EMAIL, "I1", "B").unwrap();
        let rows = ids.items_in(EMAIL, "F1").unwrap();
        assert_eq!(rows[0].change_key, "B");
        assert_eq!(rows[0].local_id, id(7));
    }

    #[test]
    fn all_lists_by_item_id() {
        let (archive, sid) = setup();
        let ids = archive.store().ews_ids(sid);
        ids.insert(MAILBOX, "", "F1", "CK1", id(1)).unwrap();
        ids.insert(MAILBOX, "F1", "F2", "CK2", id(2)).unwrap();
        let map = ids.all(MAILBOX).unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("F1").map(|r| r.local_id), Some(id(1)));
        assert_eq!(map.get("F2").map(|r| r.folder_id.as_str()), Some("F1"));
    }

    #[test]
    fn sync_state_round_trips() {
        let (archive, sid) = setup();
        let ids = archive.store().ews_ids(sid);
        ids.insert(MAILBOX, "", "F1", "", id(1)).unwrap();
        assert_eq!(ids.sync_state(MAILBOX, "F1").unwrap().as_deref(), Some(""));
        ids.set_sync_state(MAILBOX, "F1", "opaque-state").unwrap();
        assert_eq!(
            ids.sync_state(MAILBOX, "F1").unwrap().as_deref(),
            Some("opaque-state")
        );
        assert_eq!(ids.sync_state(MAILBOX, "missing").unwrap(), None);
    }

    #[test]
    fn delete_removes_only_one() {
        let (archive, sid) = setup();
        let ids = archive.store().ews_ids(sid);
        ids.insert(EMAIL, "F1", "I1", "A", id(1)).unwrap();
        ids.insert(EMAIL, "F1", "I2", "A", id(2)).unwrap();
        ids.delete(EMAIL, "I1").unwrap();
        let rows = ids.items_in(EMAIL, "F1").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].item_id, "I2");
    }

    #[test]
    fn unique_local_id_per_type_is_enforced() {
        let (archive, sid) = setup();
        let ids = archive.store().ews_ids(sid);
        ids.insert(EMAIL, "F1", "I1", "A", id(1)).unwrap();
        let err = ids.insert(EMAIL, "F1", "I2", "A", id(1)).unwrap_err();
        assert!(err.to_string().contains("UNIQUE"));
    }
}
