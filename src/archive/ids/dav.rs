/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

use crate::archive::{LocalId, SourceId, Store};
use crate::types::ObjectType;

const PURGE_CALENDAR_EVENTS: &str = "DELETE FROM calendar_events WHERE id IN
     (SELECT local_id FROM sync_id_dav WHERE source_id = ?1 AND type_name = ?2)";
const PURGE_CONTACT_CARDS: &str = "DELETE FROM contact_cards WHERE id IN
     (SELECT local_id FROM sync_id_dav WHERE source_id = ?1 AND type_name = ?2)";
const PURGE_CALENDARS: &str = "DELETE FROM calendars WHERE id IN
     (SELECT local_id FROM sync_id_dav WHERE source_id = ?1 AND type_name = ?2)";
const PURGE_ADDRESS_BOOKS: &str = "DELETE FROM address_books WHERE id IN
     (SELECT local_id FROM sync_id_dav WHERE source_id = ?1 AND type_name = ?2)";
const PURGE_LEAF_FILE_NODES: &str = "DELETE FROM file_nodes WHERE id IN
     (SELECT local_id FROM sync_id_dav WHERE source_id = ?1 AND type_name = ?2)
     AND NOT EXISTS (SELECT 1 FROM file_nodes AS child WHERE child.parent_id = file_nodes.id)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DavItem {
    pub etag: String,
    pub local: LocalId,
}

#[derive(Clone, Copy)]
pub struct DavIds<'a> {
    conn: &'a Connection,
    source: SourceId,
}

impl<'a> Store<'a> {
    pub fn dav_ids(self, source: SourceId) -> DavIds<'a> {
        DavIds {
            conn: self.connection(),
            source,
        }
    }
}

impl DavIds<'_> {
    pub fn insert(
        &self,
        ty: ObjectType,
        collection: &str,
        item: &str,
        etag: &str,
        local: LocalId,
    ) -> rusqlite::Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO sync_id_dav
                 (source_id, type_name, collection_href, item_href, etag, local_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![
                self.source,
                ty.token(),
                collection,
                item,
                etag,
                local
            ])?;
        Ok(())
    }

    pub fn update_etag(&self, ty: ObjectType, item: &str, etag: &str) -> rusqlite::Result<()> {
        self.conn
            .prepare_cached(
                "UPDATE sync_id_dav SET etag = ?4
                 WHERE source_id = ?1 AND type_name = ?2 AND item_href = ?3",
            )?
            .execute(params![self.source, ty.token(), item, etag])?;
        Ok(())
    }

    pub fn local_id(&self, ty: ObjectType, item: &str) -> rusqlite::Result<Option<LocalId>> {
        self.conn
            .prepare_cached(
                "SELECT local_id FROM sync_id_dav
                 WHERE source_id = ?1 AND type_name = ?2 AND item_href = ?3",
            )?
            .query_row(params![self.source, ty.token(), item], |row| row.get(0))
            .optional()
    }

    pub fn item(&self, ty: ObjectType, item: &str) -> rusqlite::Result<Option<DavItem>> {
        self.conn
            .prepare_cached(
                "SELECT etag, local_id FROM sync_id_dav
                 WHERE source_id = ?1 AND type_name = ?2 AND item_href = ?3",
            )?
            .query_row(params![self.source, ty.token(), item], |row| {
                Ok(DavItem {
                    etag: row.get(0)?,
                    local: row.get(1)?,
                })
            })
            .optional()
    }

    pub fn items_in(
        &self,
        ty: ObjectType,
        collection: &str,
    ) -> rusqlite::Result<HashMap<String, DavItem>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT item_href, etag, local_id FROM sync_id_dav
             WHERE source_id = ?1 AND type_name = ?2 AND collection_href = ?3",
        )?;
        let rows = stmt.query_map(params![self.source, ty.token(), collection], |row| {
            Ok((
                row.get(0)?,
                DavItem {
                    etag: row.get(1)?,
                    local: row.get(2)?,
                },
            ))
        })?;
        rows.collect()
    }

    pub fn all(&self, ty: ObjectType) -> rusqlite::Result<HashMap<String, LocalId>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT item_href, local_id FROM sync_id_dav
             WHERE source_id = ?1 AND type_name = ?2",
        )?;
        let rows = stmt.query_map(params![self.source, ty.token()], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        rows.collect()
    }

    pub fn delete(&self, ty: ObjectType, item: &str) -> rusqlite::Result<()> {
        self.conn
            .prepare_cached(
                "DELETE FROM sync_id_dav
                 WHERE source_id = ?1 AND type_name = ?2 AND item_href = ?3",
            )?
            .execute(params![self.source, ty.token(), item])?;
        Ok(())
    }

    pub fn delete_collection(&self, ty: ObjectType, collection: &str) -> rusqlite::Result<usize> {
        self.conn.execute(
            "DELETE FROM sync_id_dav
             WHERE source_id = ?1 AND type_name = ?2
               AND (item_href = ?3 OR collection_href = ?3)",
            params![self.source, ty.token(), collection],
        )
    }

    pub fn purge(&self, ty: ObjectType) -> rusqlite::Result<u64> {
        let sql = match ty {
            ObjectType::CalendarEvent => PURGE_CALENDAR_EVENTS,
            ObjectType::ContactCard => PURGE_CONTACT_CARDS,
            ObjectType::Calendar => PURGE_CALENDARS,
            ObjectType::AddressBook => PURGE_ADDRESS_BOOKS,
            ObjectType::FileNode => return self.purge_file_nodes(),
            _ => return Ok(0),
        };
        let removed = self.conn.execute(sql, params![self.source, ty.token()])?;
        Ok(removed as u64)
    }

    fn purge_file_nodes(&self) -> rusqlite::Result<u64> {
        let mut leaves = self.conn.prepare(PURGE_LEAF_FILE_NODES)?;
        let mut removed = 0;
        loop {
            let n = leaves.execute(params![self.source, ObjectType::FileNode.token()])?;
            if n == 0 {
                return Ok(removed);
            }
            removed += n as u64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DavIds;
    use crate::archive::sources::{SourceKey, SourceKind};
    use crate::archive::{Archive, LocalId};
    use crate::types::ObjectType::{Calendar, CalendarEvent};

    fn ids(archive: &Archive) -> DavIds<'_> {
        let sid = archive
            .store()
            .sources()
            .upsert(
                &SourceKey {
                    kind: SourceKind::CalDav,
                    url: "https://x/",
                    account_id: "u",
                },
                Some("u"),
                "u",
            )
            .unwrap();
        archive.store().dav_ids(sid)
    }

    fn id(raw: i64) -> LocalId {
        LocalId::from_raw(raw)
    }

    #[test]
    fn insert_and_lookup_round_trip() {
        let archive = Archive::open_in_memory().unwrap();
        let ids = ids(&archive);
        ids.insert(CalendarEvent, "/cal/a/", "/cal/a/e.ics", "\"v1\"", id(7))
            .unwrap();
        assert_eq!(
            ids.local_id(CalendarEvent, "/cal/a/e.ics").unwrap(),
            Some(id(7))
        );
        let item = ids.item(CalendarEvent, "/cal/a/e.ics").unwrap().unwrap();
        assert_eq!((item.etag.as_str(), item.local), ("\"v1\"", id(7)));
        assert!(ids.item(Calendar, "/cal/a/e.ics").unwrap().is_none());
    }

    #[test]
    fn update_etag_keeps_local_id() {
        let archive = Archive::open_in_memory().unwrap();
        let ids = ids(&archive);
        ids.insert(CalendarEvent, "/cal/a/", "/cal/a/e.ics", "\"v1\"", id(7))
            .unwrap();
        ids.update_etag(CalendarEvent, "/cal/a/e.ics", "\"v2\"")
            .unwrap();
        let items = ids.items_in(CalendarEvent, "/cal/a/").unwrap();
        assert_eq!(items.len(), 1);
        let item = &items["/cal/a/e.ics"];
        assert_eq!((item.etag.as_str(), item.local), ("\"v2\"", id(7)));
    }

    #[test]
    fn items_in_filters_by_collection_href() {
        let archive = Archive::open_in_memory().unwrap();
        let ids = ids(&archive);
        ids.insert(CalendarEvent, "/cal/a/", "/cal/a/e1.ics", "v1", id(1))
            .unwrap();
        ids.insert(CalendarEvent, "/cal/a/", "/cal/a/e2.ics", "v2", id(2))
            .unwrap();
        ids.insert(CalendarEvent, "/cal/b/", "/cal/b/e3.ics", "v3", id(3))
            .unwrap();
        assert_eq!(ids.items_in(CalendarEvent, "/cal/a/").unwrap().len(), 2);
    }

    #[test]
    fn delete_removes_only_one_row() {
        let archive = Archive::open_in_memory().unwrap();
        let ids = ids(&archive);
        ids.insert(CalendarEvent, "/cal/a/", "/cal/a/e1.ics", "v", id(1))
            .unwrap();
        ids.insert(CalendarEvent, "/cal/a/", "/cal/a/e2.ics", "v", id(2))
            .unwrap();
        ids.delete(CalendarEvent, "/cal/a/e1.ics").unwrap();
        let items = ids.items_in(CalendarEvent, "/cal/a/").unwrap();
        assert_eq!(items.len(), 1);
        assert!(items.contains_key("/cal/a/e2.ics"));
    }

    #[test]
    fn delete_collection_removes_collection_row_and_items() {
        let archive = Archive::open_in_memory().unwrap();
        let ids = ids(&archive);
        ids.insert(Calendar, "/cal/a/", "/cal/a/", "", id(10))
            .unwrap();
        ids.insert(CalendarEvent, "/cal/a/", "/cal/a/e.ics", "v", id(1))
            .unwrap();
        assert_eq!(ids.delete_collection(CalendarEvent, "/cal/a/").unwrap(), 1);
        assert!(ids.items_in(CalendarEvent, "/cal/a/").unwrap().is_empty());
        assert_eq!(ids.all(Calendar).unwrap().len(), 1);
    }

    #[test]
    fn all_lists_every_known_collection() {
        let archive = Archive::open_in_memory().unwrap();
        let ids = ids(&archive);
        ids.insert(Calendar, "/cal/a/", "/cal/a/", "", id(1))
            .unwrap();
        ids.insert(Calendar, "/cal/b/", "/cal/b/", "", id(2))
            .unwrap();
        let map = ids.all(Calendar).unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("/cal/a/"), Some(&id(1)));
        assert_eq!(map.get("/cal/b/"), Some(&id(2)));
    }

    #[test]
    fn unique_local_id_per_type_enforced() {
        let archive = Archive::open_in_memory().unwrap();
        let ids = ids(&archive);
        ids.insert(CalendarEvent, "/cal/a/", "/cal/a/e1.ics", "v", id(1))
            .unwrap();
        let err = ids
            .insert(CalendarEvent, "/cal/a/", "/cal/a/e2.ics", "v", id(1))
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("UNIQUE"),
            "expected UNIQUE violation, got {msg}"
        );
    }
}
