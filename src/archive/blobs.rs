/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::{Connection, OptionalExtension, params};

use super::{BlobId, Store};

#[derive(Clone, Copy)]
pub struct Blobs<'a> {
    conn: &'a Connection,
}

impl<'a> Store<'a> {
    pub fn blobs(self) -> Blobs<'a> {
        Blobs { conn: self.conn }
    }
}

impl Blobs<'_> {
    pub fn intern(&self, bytes: &[u8]) -> rusqlite::Result<BlobId> {
        let hash = blake3::hash(bytes);
        let hash_bytes = hash.as_bytes().as_slice();
        self.conn.execute(
            "INSERT OR IGNORE INTO blobs (hash, data) VALUES (?1, ?2)",
            params![hash_bytes, bytes],
        )?;
        self.conn.query_row(
            "SELECT id FROM blobs WHERE hash = ?1",
            params![hash_bytes],
            |row| row.get(0),
        )
    }

    pub fn bytes(&self, id: BlobId) -> rusqlite::Result<Option<Vec<u8>>> {
        self.conn
            .query_row("SELECT data FROM blobs WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()
    }

    pub fn len(&self, id: BlobId) -> rusqlite::Result<Option<u64>> {
        self.conn
            .query_row(
                "SELECT length(data) FROM blobs WHERE id = ?1",
                [id],
                |row| {
                    let len: i64 = row.get(0)?;
                    Ok(u64::try_from(len).unwrap_or(0))
                },
            )
            .optional()
    }

    pub fn gc_orphans(&self) -> rusqlite::Result<usize> {
        self.conn.execute(
            "DELETE FROM blobs WHERE id NOT IN (
                 SELECT blob_id FROM emails
                 UNION SELECT blob_id FROM sieve_scripts
                 UNION SELECT blob_id FROM file_nodes WHERE blob_id IS NOT NULL
                 UNION SELECT json_tree.atom FROM contact_cards, json_tree(contact_cards.data)
                       WHERE json_tree.key = '@blob'
                 UNION SELECT json_tree.atom FROM calendar_events, json_tree(calendar_events.data)
                       WHERE json_tree.key = '@blob')",
            [],
        )
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::params;

    use crate::archive::{Archive, BlobId};

    #[test]
    fn intern_dedups_identical_bytes() {
        let archive = Archive::open_in_memory().unwrap();
        let blobs = archive.store().blobs();
        let a = blobs.intern(b"hello world").unwrap();
        let b = blobs.intern(b"hello world").unwrap();
        let other = blobs.intern(b"different").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, other);
        let n: i64 = archive
            .connection()
            .query_row("SELECT count(*) FROM blobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn bytes_roundtrip() {
        let archive = Archive::open_in_memory().unwrap();
        let blobs = archive.store().blobs();
        let id = blobs.intern(b"payload").unwrap();
        assert_eq!(blobs.bytes(id).unwrap().as_deref(), Some(&b"payload"[..]));
        assert_eq!(blobs.bytes(BlobId::from_raw(9999)).unwrap(), None);
    }

    #[test]
    fn len_reports_byte_length() {
        let archive = Archive::open_in_memory().unwrap();
        let blobs = archive.store().blobs();
        let id = blobs.intern(b"payload").unwrap();
        assert_eq!(blobs.len(id).unwrap(), Some(7));
        assert_eq!(blobs.len(BlobId::from_raw(9999)).unwrap(), None);
    }

    #[test]
    fn gc_keeps_referenced_and_reaps_orphans() {
        let archive = Archive::open_in_memory().unwrap();
        let blobs = archive.store().blobs();
        let referenced = blobs.intern(b"keep me").unwrap();
        blobs.intern(b"reap me").unwrap();
        archive
            .connection()
            .execute(
                "INSERT INTO emails (blob_id, received_at, mailbox_ids, keywords)
                 VALUES (?1, '2020-01-01T00:00:00Z', '[1]', '[]')",
                params![referenced],
            )
            .unwrap();
        assert_eq!(blobs.gc_orphans().unwrap(), 1);
        assert!(blobs.bytes(referenced).unwrap().is_some());
    }

    #[test]
    fn gc_follows_blob_sentinel_in_json_data() {
        let archive = Archive::open_in_memory().unwrap();
        let blobs = archive.store().blobs();
        let photo = blobs.intern(b"photo bytes").unwrap();
        blobs.intern(b"orphan").unwrap();
        archive
            .connection()
            .execute(
                "INSERT INTO contact_cards (uid, address_book_ids, data)
                 VALUES ('u1', '[1]', ?1)",
                params![format!(r#"{{"photos":{{"p":{{"@blob":{photo}}}}}}}"#)],
            )
            .unwrap();
        assert_eq!(blobs.gc_orphans().unwrap(), 1);
        assert!(blobs.bytes(photo).unwrap().is_some());
    }
}
