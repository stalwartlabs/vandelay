/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::{OptionalExtension, params};

use super::{SieveScripts, Write};
use crate::archive::{BlobId, LocalId, SourceId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SieveScriptState {
    pub id: LocalId,
    pub hash: Option<[u8; 32]>,
    pub is_active: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct SieveScriptRecord<'r> {
    pub name: Option<&'r str>,
    pub is_active: bool,
    pub blob: BlobId,
}

impl SieveScripts<'_> {
    pub fn insert(&self, s: &SieveScriptRecord<'_>) -> rusqlite::Result<LocalId> {
        self.conn.execute(
            "INSERT INTO sieve_scripts (name, is_active, blob_id) VALUES (?1, ?2, ?3)",
            params![s.name, s.is_active, s.blob],
        )?;
        Ok(LocalId(self.conn.last_insert_rowid()))
    }

    pub fn update(&self, id: LocalId, s: &SieveScriptRecord<'_>) -> rusqlite::Result<Write> {
        self.conn
            .execute(
                "UPDATE sieve_scripts SET name = ?1, is_active = ?2, blob_id = ?3
                 WHERE id = ?4 AND (name IS NOT ?1 OR is_active IS NOT ?2 OR blob_id IS NOT ?3)",
                params![s.name, s.is_active, s.blob, id],
            )
            .map(Write::from_rows)
    }
}

impl SieveScripts<'_> {
    pub fn active(&self) -> rusqlite::Result<Option<LocalId>> {
        self.conn
            .query_row(
                "SELECT id FROM sieve_scripts WHERE is_active = 1",
                [],
                |r| r.get(0),
            )
            .optional()
    }

    pub fn set_active(&self, active: Option<LocalId>) -> rusqlite::Result<()> {
        let current = self.active()?;
        if current == active {
            return Ok(());
        }
        if current.is_some() {
            self.conn.execute(
                "UPDATE sieve_scripts SET is_active = 0 WHERE is_active = 1",
                [],
            )?;
        }
        if let Some(id) = active {
            self.conn
                .execute("UPDATE sieve_scripts SET is_active = 1 WHERE id = ?1", [id])?;
        }
        Ok(())
    }

    pub fn managesieve_states(&self, source: SourceId) -> rusqlite::Result<Vec<SieveScriptState>> {
        self.conn
            .prepare(
                "SELECT s.id, b.hash, s.is_active
                 FROM sieve_scripts s JOIN blobs b ON b.id = s.blob_id
                 JOIN sync_id_managesieve m ON m.local_id = s.id
                 WHERE m.source_id = ?1",
            )?
            .query_map([source], |r| {
                let hash: Vec<u8> = r.get(1)?;
                Ok(SieveScriptState {
                    id: r.get(0)?,
                    hash: <[u8; 32]>::try_from(hash.as_slice()).ok(),
                    is_active: r.get(2)?,
                })
            })?
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Archive;
    use crate::archive::sources::{SourceKey, SourceKind};

    #[test]
    fn insert_update_delete() {
        let archive = Archive::open_in_memory().unwrap();
        let store = archive.store();
        let blob = store.blobs().intern(b"keep;").unwrap();
        let scripts = store.sieve_scripts();
        let mut record = SieveScriptRecord {
            name: Some("main"),
            is_active: true,
            blob,
        };
        let id = scripts.insert(&record).unwrap();
        assert_eq!(scripts.update(id, &record).unwrap(), Write::Unchanged);
        record.is_active = false;
        assert_eq!(scripts.update(id, &record).unwrap(), Write::Changed);
        scripts.delete(id).unwrap();
    }

    #[test]
    fn set_active_moves_the_single_active_flag() {
        let archive = Archive::open_in_memory().unwrap();
        let store = archive.store();
        let blob = store.blobs().intern(b"keep;").unwrap();
        let scripts = store.sieve_scripts();
        let record = |name| SieveScriptRecord {
            name: Some(name),
            is_active: false,
            blob,
        };
        let a = scripts.insert(&record("a")).unwrap();
        let b = scripts.insert(&record("b")).unwrap();
        assert_eq!(scripts.active().unwrap(), None);
        scripts.set_active(Some(a)).unwrap();
        assert_eq!(scripts.active().unwrap(), Some(a));
        scripts.set_active(Some(b)).unwrap();
        assert_eq!(scripts.active().unwrap(), Some(b));
        scripts.set_active(None).unwrap();
        assert_eq!(scripts.active().unwrap(), None);
    }

    // only scripts mapped to the given ManageSieve source are listed, with their blob hash
    #[test]
    fn managesieve_states_lists_scripts_of_the_source() {
        let archive = Archive::open_in_memory().unwrap();
        let store = archive.store();
        let sources = store.sources();
        let key = |account_id| SourceKey {
            kind: SourceKind::ManageSieve,
            url: "sieve://mail.example.org",
            account_id,
        };
        let mine = sources.upsert(&key("alice"), None, "alice").unwrap();
        let other = sources.upsert(&key("bob"), None, "bob").unwrap();
        let keep = store.blobs().intern(b"keep;").unwrap();
        let discard = store.blobs().intern(b"discard;").unwrap();
        let scripts = store.sieve_scripts();
        let record = |name, is_active, blob| SieveScriptRecord {
            name: Some(name),
            is_active,
            blob,
        };
        let a = scripts.insert(&record("a", true, keep)).unwrap();
        let b = scripts.insert(&record("b", false, discard)).unwrap();
        let c = scripts.insert(&record("c", false, keep)).unwrap();
        scripts.insert(&record("unmapped", false, keep)).unwrap();
        let mut map = archive
            .connection()
            .prepare(
                "INSERT INTO sync_id_managesieve (source_id, name, local_id) VALUES (?1, ?2, ?3)",
            )
            .unwrap();
        for (source, name, id) in [(mine, "a", a), (mine, "b", b), (other, "c", c)] {
            map.execute(params![source, name, id]).unwrap();
        }

        let mut states = scripts.managesieve_states(mine).unwrap();
        states.sort_unstable_by_key(|s| s.id);
        assert_eq!(
            states,
            [
                SieveScriptState {
                    id: a,
                    hash: Some(*blake3::hash(b"keep;").as_bytes()),
                    is_active: true,
                },
                SieveScriptState {
                    id: b,
                    hash: Some(*blake3::hash(b"discard;").as_bytes()),
                    is_active: false,
                },
            ]
        );
        assert_eq!(scripts.managesieve_states(other).unwrap().len(), 1);
    }
}
