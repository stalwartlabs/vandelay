/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::{Connection, OptionalExtension, params};

use crate::archive::{SourceId, Store};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderState {
    pub uidvalidity: u32,
    pub uidnext: u32,
    pub last_seen: String,
}

#[derive(Clone, Copy)]
pub struct ImapFolderState<'a> {
    conn: &'a Connection,
    source: SourceId,
}

impl<'a> Store<'a> {
    pub fn imap_folder_state(self, source: SourceId) -> ImapFolderState<'a> {
        ImapFolderState {
            conn: self.connection(),
            source,
        }
    }
}

impl ImapFolderState<'_> {
    pub fn get(&self, folder: &str) -> rusqlite::Result<Option<FolderState>> {
        self.conn
            .prepare_cached(
                "SELECT uidvalidity, uidnext, last_seen FROM imap_folder_state
                 WHERE source_id = ?1 AND folder = ?2",
            )?
            .query_row(params![self.source, folder], |row| {
                Ok(FolderState {
                    uidvalidity: row.get(0)?,
                    uidnext: row.get(1)?,
                    last_seen: row.get(2)?,
                })
            })
            .optional()
    }

    pub fn upsert(&self, folder: &str, state: &FolderState) -> rusqlite::Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO imap_folder_state (source_id, folder, uidvalidity, uidnext, last_seen)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (source_id, folder)
                 DO UPDATE SET uidvalidity = excluded.uidvalidity,
                               uidnext     = excluded.uidnext,
                               last_seen   = excluded.last_seen",
            )?
            .execute(params![
                self.source,
                folder,
                state.uidvalidity,
                state.uidnext,
                state.last_seen
            ])?;
        Ok(())
    }

    pub fn delete(&self, folder: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM imap_folder_state WHERE source_id = ?1 AND folder = ?2",
            params![self.source, folder],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Archive;
    use crate::archive::sources::{SourceKey, SourceKind};

    fn setup() -> (Archive, SourceId) {
        let archive = Archive::open_in_memory().unwrap();
        let sid = archive
            .store()
            .sources()
            .upsert(
                &SourceKey {
                    kind: SourceKind::Imap,
                    url: "imaps://host:993",
                    account_id: "alice",
                },
                None,
                "alice",
            )
            .unwrap();
        (archive, sid)
    }

    fn state(uidvalidity: u32, uidnext: u32, last_seen: &str) -> FolderState {
        FolderState {
            uidvalidity,
            uidnext,
            last_seen: last_seen.to_owned(),
        }
    }

    #[test]
    fn upsert_then_get_returns_latest() {
        let (archive, sid) = setup();
        let st = archive.store().imap_folder_state(sid);
        assert!(st.get("INBOX").unwrap().is_none());
        st.upsert("INBOX", &state(100, 50, "2026-05-24T00:00:00Z"))
            .unwrap();
        assert_eq!(
            st.get("INBOX").unwrap(),
            Some(state(100, 50, "2026-05-24T00:00:00Z"))
        );
        st.upsert("INBOX", &state(100, 75, "2026-05-24T01:00:00Z"))
            .unwrap();
        assert_eq!(
            st.get("INBOX").unwrap(),
            Some(state(100, 75, "2026-05-24T01:00:00Z"))
        );
    }

    #[test]
    fn delete_removes_only_named_folder() {
        let (archive, sid) = setup();
        let st = archive.store().imap_folder_state(sid);
        st.upsert("INBOX", &state(1, 1, "2026-05-24T00:00:00Z"))
            .unwrap();
        st.upsert("Sent", &state(2, 1, "2026-05-24T00:00:00Z"))
            .unwrap();
        st.delete("INBOX").unwrap();
        assert!(st.get("INBOX").unwrap().is_none());
        assert!(st.get("Sent").unwrap().is_some());
    }
}
