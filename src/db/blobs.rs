/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::{Connection, OptionalExtension, params};

pub fn intern_blob(conn: &Connection, bytes: &[u8]) -> Result<i64, rusqlite::Error> {
    let hash = blake3::hash(bytes);
    let hash_bytes = hash.as_bytes().as_slice();
    conn.execute(
        "INSERT OR IGNORE INTO blobs (hash, data) VALUES (?1, ?2)",
        params![hash_bytes, bytes],
    )?;
    conn.query_row(
        "SELECT id FROM blobs WHERE hash = ?1",
        params![hash_bytes],
        |row| row.get(0),
    )
}

pub fn blob_bytes(conn: &Connection, id: i64) -> Result<Option<Vec<u8>>, rusqlite::Error> {
    conn.query_row("SELECT data FROM blobs WHERE id = ?1", params![id], |row| {
        row.get(0)
    })
    .optional()
}

pub fn blob_len(conn: &Connection, id: i64) -> Result<Option<u64>, rusqlite::Error> {
    conn.query_row(
        "SELECT length(data) FROM blobs WHERE id = ?1",
        params![id],
        |row| {
            let len: i64 = row.get(0)?;
            Ok(len.max(0) as u64)
        },
    )
    .optional()
}

pub fn gc_orphan_blobs(conn: &Connection) -> Result<usize, rusqlite::Error> {
    conn.execute(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        init::apply_schema(&c).unwrap();
        c
    }

    #[test]
    fn intern_dedups_identical_bytes() {
        let c = mem();
        let a = intern_blob(&c, b"hello world").unwrap();
        let b = intern_blob(&c, b"hello world").unwrap();
        let other = intern_blob(&c, b"different").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, other);
        let n: i64 = c
            .query_row("SELECT count(*) FROM blobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn blob_bytes_roundtrip() {
        let c = mem();
        let id = intern_blob(&c, b"payload").unwrap();
        assert_eq!(
            blob_bytes(&c, id).unwrap().as_deref(),
            Some(&b"payload"[..])
        );
        assert_eq!(blob_bytes(&c, 9999).unwrap(), None);
    }

    #[test]
    fn blob_len_reports_byte_length() {
        let c = mem();
        let id = intern_blob(&c, b"payload").unwrap();
        assert_eq!(blob_len(&c, id).unwrap(), Some(7));
        assert_eq!(blob_len(&c, 9999).unwrap(), None);
    }

    #[test]
    fn gc_keeps_referenced_and_reaps_orphans() {
        let c = mem();
        let referenced = intern_blob(&c, b"keep me").unwrap();
        let _orphan = intern_blob(&c, b"reap me").unwrap();
        c.execute(
            "INSERT INTO emails (blob_id, received_at, mailbox_ids, keywords)
             VALUES (?1, '2020-01-01T00:00:00Z', '[1]', '[]')",
            params![referenced],
        )
        .unwrap();
        let removed = gc_orphan_blobs(&c).unwrap();
        assert_eq!(removed, 1);
        assert!(blob_bytes(&c, referenced).unwrap().is_some());
    }

    #[test]
    fn gc_follows_blob_sentinel_in_json_data() {
        let c = mem();
        let photo = intern_blob(&c, b"photo bytes").unwrap();
        let _orphan = intern_blob(&c, b"orphan").unwrap();
        c.execute(
            "INSERT INTO contact_cards (uid, address_book_ids, data)
             VALUES ('u1', '[1]', ?1)",
            params![format!(r#"{{"photos":{{"p":{{"@blob":{photo}}}}}}}"#)],
        )
        .unwrap();
        let removed = gc_orphan_blobs(&c).unwrap();
        assert_eq!(removed, 1);
        assert!(blob_bytes(&c, photo).unwrap().is_some());
    }
}
