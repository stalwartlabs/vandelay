/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;

use rusqlite::params;

use super::folder_id_table;
use crate::archive::LocalId;

folder_id_table!(
    ImapIds,
    imap_ids,
    "sync_id_imap",
    "INSERT OR REPLACE",
    (u32, u32),
    uidvalidity: u32 => u32 = "0",
    uid: u32 => u32 = "0"
);

impl ImapIds<'_> {
    pub fn uids_at_validity(&self, folder: &str, uidvalidity: u32) -> rusqlite::Result<Vec<u32>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT uid FROM sync_id_imap
             WHERE source_id = ?1 AND type_name = ?2 AND folder = ?3
               AND uidvalidity = ?4
             ORDER BY uid",
        )?;
        let rows = stmt.query_map(
            params![self.source, Self::ITEM, folder, uidvalidity],
            |row| row.get(0),
        )?;
        rows.collect()
    }

    pub fn items_at_validity(
        &self,
        folder: &str,
        uidvalidity: u32,
    ) -> rusqlite::Result<HashMap<u32, LocalId>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT uid, local_id FROM sync_id_imap
             WHERE source_id = ?1 AND type_name = ?2 AND folder = ?3
               AND uidvalidity = ?4",
        )?;
        let rows = stmt.query_map(
            params![self.source, Self::ITEM, folder, uidvalidity],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        rows.collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::archive::sources::{SourceKey, SourceKind};
    use crate::archive::{Archive, LocalId};

    fn setup() -> Archive {
        Archive::open_in_memory().unwrap()
    }

    fn ids(archive: &Archive) -> super::ImapIds<'_> {
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
        archive.store().imap_ids(sid)
    }

    fn id(raw: i64) -> LocalId {
        LocalId::from_raw(raw)
    }

    #[test]
    fn mailbox_roundtrip() {
        let archive = setup();
        let ids = ids(&archive);
        ids.insert_folder("INBOX", id(7)).unwrap();
        ids.insert_folder("INBOX/Sub", id(8)).unwrap();
        assert_eq!(ids.folder_id("INBOX").unwrap(), Some(id(7)));
        let all = ids.folders().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all.get("INBOX/Sub"), Some(&id(8)));
        ids.delete_folder("INBOX").unwrap();
        assert_eq!(ids.folder_id("INBOX").unwrap(), None);
    }

    #[test]
    fn email_roundtrip() {
        let archive = setup();
        let ids = ids(&archive);
        ids.insert_item("INBOX", 12345, 100, id(1)).unwrap();
        ids.insert_item("INBOX", 12345, 101, id(2)).unwrap();
        ids.insert_item("Sent", 7777, 1, id(3)).unwrap();
        assert_eq!(ids.item_id("INBOX", 12345, 100).unwrap(), Some(id(1)));
        let inbox = ids.items_in("INBOX").unwrap();
        assert_eq!(inbox.len(), 2);
        assert_eq!(inbox.get(&(12345, 100)), Some(&id(1)));
        let folders = ids.folders_with_items().unwrap();
        assert!(folders.contains("INBOX"));
        assert!(folders.contains("Sent"));
    }

    #[test]
    fn items_at_validity_ignores_other_validities() {
        let archive = setup();
        let ids = ids(&archive);
        ids.insert_item("INBOX", 1, 10, id(1)).unwrap();
        ids.insert_item("INBOX", 2, 11, id(2)).unwrap();
        let at = ids.items_at_validity("INBOX", 2).unwrap();
        assert_eq!(at.len(), 1);
        assert_eq!(at.get(&11), Some(&id(2)));
    }

    #[test]
    fn delete_items_in_wipes_only_that_folder() {
        let archive = setup();
        let ids = ids(&archive);
        ids.insert_item("INBOX", 1, 10, id(1)).unwrap();
        ids.insert_item("INBOX", 1, 11, id(2)).unwrap();
        ids.insert_item("Sent", 1, 10, id(3)).unwrap();
        assert_eq!(ids.delete_items_in("INBOX").unwrap(), 2);
        assert!(ids.items_in("INBOX").unwrap().is_empty());
        assert_eq!(ids.items_in("Sent").unwrap().len(), 1);
    }

    #[test]
    fn re_insert_with_same_local_id_replaces() {
        let archive = setup();
        let ids = ids(&archive);
        ids.insert_folder("A", id(1)).unwrap();
        ids.insert_folder("B", id(1)).unwrap();
        assert_eq!(ids.folder_id("A").unwrap(), None);
        assert_eq!(ids.folder_id("B").unwrap(), Some(id(1)));
        assert_eq!(ids.folders().unwrap().len(), 1);
    }

    #[test]
    fn pk_composite_is_per_type_per_folder_per_uid() {
        let archive = setup();
        let ids = ids(&archive);
        ids.insert_item("INBOX", 1, 10, id(1)).unwrap();
        ids.insert_item("INBOX", 1, 10, id(1)).unwrap();
        assert_eq!(ids.items_in("INBOX").unwrap().len(), 1);
    }

    #[test]
    fn mailbox_and_email_namespaces_do_not_collide_on_local_id() {
        let archive = setup();
        let ids = ids(&archive);
        ids.insert_folder("INBOX", id(42)).unwrap();
        ids.insert_item("INBOX", 1, 1, id(42)).unwrap();
    }
}
