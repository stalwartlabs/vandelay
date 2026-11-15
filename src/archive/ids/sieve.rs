/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

use crate::archive::{LocalId, SourceId, Store};

#[derive(Clone, Copy)]
pub struct SieveIds<'a> {
    conn: &'a Connection,
    source: SourceId,
}

impl<'a> Store<'a> {
    pub fn sieve_ids(self, source: SourceId) -> SieveIds<'a> {
        SieveIds {
            conn: self.connection(),
            source,
        }
    }
}

impl SieveIds<'_> {
    pub fn insert(&self, name: &str, local: LocalId) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO sync_id_managesieve (source_id, name, local_id)
             VALUES (?1, ?2, ?3)",
            params![self.source, name, local],
        )?;
        Ok(())
    }

    pub fn local_id(&self, name: &str) -> rusqlite::Result<Option<LocalId>> {
        self.conn
            .query_row(
                "SELECT local_id FROM sync_id_managesieve WHERE source_id = ?1 AND name = ?2",
                params![self.source, name],
                |row| row.get(0),
            )
            .optional()
    }

    pub fn all(&self) -> rusqlite::Result<HashMap<String, LocalId>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT name, local_id FROM sync_id_managesieve WHERE source_id = ?1",
        )?;
        let rows = stmt.query_map([self.source], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect()
    }

    pub fn delete(&self, name: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM sync_id_managesieve WHERE source_id = ?1 AND name = ?2",
            params![self.source, name],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::archive::sources::{SourceKey, SourceKind};
    use crate::archive::{Archive, LocalId};

    fn ids(archive: &Archive) -> super::SieveIds<'_> {
        let sid = archive
            .store()
            .sources()
            .upsert(
                &SourceKey {
                    kind: SourceKind::ManageSieve,
                    url: "sieve://host:4190",
                    account_id: "alice",
                },
                None,
                "alice",
            )
            .unwrap();
        archive.store().sieve_ids(sid)
    }

    fn id(raw: i64) -> LocalId {
        LocalId::from_raw(raw)
    }

    #[test]
    fn insert_and_lookup_by_name() {
        let archive = Archive::open_in_memory().unwrap();
        let ids = ids(&archive);
        ids.insert("vacation", id(7)).unwrap();
        assert_eq!(ids.local_id("vacation").unwrap(), Some(id(7)));
        assert_eq!(ids.local_id("missing").unwrap(), None);
    }

    #[test]
    fn all_returns_full_map() {
        let archive = Archive::open_in_memory().unwrap();
        let ids = ids(&archive);
        ids.insert("a", id(1)).unwrap();
        ids.insert("b", id(2)).unwrap();
        let map = ids.all().unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("a"), Some(&id(1)));
    }

    #[test]
    fn delete_removes_row() {
        let archive = Archive::open_in_memory().unwrap();
        let ids = ids(&archive);
        ids.insert("a", id(1)).unwrap();
        ids.delete("a").unwrap();
        assert_eq!(ids.local_id("a").unwrap(), None);
    }

    #[test]
    fn re_insert_with_same_name_replaces_local_id() {
        let archive = Archive::open_in_memory().unwrap();
        let ids = ids(&archive);
        ids.insert("a", id(1)).unwrap();
        ids.insert("a", id(2)).unwrap();
        assert_eq!(ids.local_id("a").unwrap(), Some(id(2)));
        assert_eq!(ids.all().unwrap().len(), 1);
    }

    #[test]
    fn insert_or_replace_reassigns_when_local_id_clashes() {
        let archive = Archive::open_in_memory().unwrap();
        let ids = ids(&archive);
        ids.insert("a", id(1)).unwrap();
        ids.insert("b", id(1)).unwrap();
        assert!(ids.local_id("a").unwrap().is_none());
        assert_eq!(ids.local_id("b").unwrap(), Some(id(1)));
    }
}
