/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::{Connection, OptionalExtension, params};

use crate::archive::{SourceId, Store};
use crate::types::ObjectType;

#[derive(Clone, Copy)]
pub struct JmapState<'a> {
    conn: &'a Connection,
    source: SourceId,
}

impl<'a> Store<'a> {
    pub fn jmap_state(self, source: SourceId) -> JmapState<'a> {
        JmapState {
            conn: self.connection(),
            source,
        }
    }
}

impl JmapState<'_> {
    pub fn get(&self, ty: ObjectType) -> rusqlite::Result<Option<String>> {
        self.conn
            .prepare_cached(
                "SELECT state FROM sync_state_jmap WHERE source_id = ?1 AND type_name = ?2",
            )?
            .query_row(params![self.source, ty.jmap_name()], |row| row.get(0))
            .optional()
    }

    pub fn upsert(&self, ty: ObjectType, state: &str) -> rusqlite::Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO sync_state_jmap (source_id, type_name, state)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT (source_id, type_name)
                 DO UPDATE SET state = excluded.state",
            )?
            .execute(params![self.source, ty.jmap_name(), state])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::archive::Archive;
    use crate::archive::sources::{SourceKey, SourceKind};
    use crate::types::ObjectType;

    #[test]
    fn upsert_then_get_returns_latest_per_type() {
        let archive = Archive::open_in_memory().unwrap();
        let store = archive.store();
        let source = store
            .sources()
            .upsert(
                &SourceKey {
                    kind: SourceKind::Jmap,
                    url: "https://host",
                    account_id: "alice",
                },
                None,
                "alice",
            )
            .unwrap();
        let state = store.jmap_state(source);
        assert!(state.get(ObjectType::Email).unwrap().is_none());
        state.upsert(ObjectType::Email, "s1").unwrap();
        state.upsert(ObjectType::Mailbox, "m1").unwrap();
        assert_eq!(state.get(ObjectType::Email).unwrap().as_deref(), Some("s1"));
        assert_eq!(
            state.get(ObjectType::Mailbox).unwrap().as_deref(),
            Some("m1")
        );
        state.upsert(ObjectType::Email, "s2").unwrap();
        assert_eq!(state.get(ObjectType::Email).unwrap().as_deref(), Some("s2"));
        assert_eq!(
            state.get(ObjectType::Mailbox).unwrap().as_deref(),
            Some("m1")
        );
    }
}
