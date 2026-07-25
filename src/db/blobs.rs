/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::{Connection, ErrorCode, OptionalExtension, Row, params, types::ValueRef};

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

// `data` is declared BLOB, but SQLite is dynamically typed: a raw SQL UPDATE
// using a text function (e.g. replace()) on this column stores the result
// with TEXT storage class instead of BLOB. Accept either.
fn column_bytes(row: &Row, idx: usize, name: &str) -> Result<Vec<u8>, rusqlite::Error> {
    match row.get_ref(idx)? {
        ValueRef::Blob(b) => Ok(b.to_vec()),
        ValueRef::Text(t) => Ok(t.to_vec()),
        other => Err(rusqlite::Error::InvalidColumnType(
            idx,
            name.to_owned(),
            other.data_type(),
        )),
    }
}

pub fn blob_bytes(conn: &Connection, id: i64) -> Result<Option<Vec<u8>>, rusqlite::Error> {
    conn.query_row("SELECT data FROM blobs WHERE id = ?1", params![id], |row| {
        column_bytes(row, 0, "data")
    })
    .optional()
}

fn is_unique_violation(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::ConstraintViolation
    )
}

/// Recomputes the BLAKE3 hash for any blob row that was touched by a raw SQL
/// edit outside vandelay (e.g. `UPDATE blobs SET data = replace(data, ...)`
/// to hand-patch a sieve script). SQLite stores such a row's `data` with TEXT
/// storage class even though the column is declared BLOB, which is how a
/// legitimate vandelay write (always bound as a `Vec<u8>` parameter) never
/// looks — so that's the marker used to find rows whose stored hash may no
/// longer match their content. Rewriting hash+data together also restores
/// proper BLOB storage class. Rows whose recomputed hash collides with an
/// existing blob are left untouched (stale hash) for manual resolution,
/// rather than failing the whole archive open.
pub fn repair_stale_hashes(conn: &Connection) -> Result<usize, rusqlite::Error> {
    let candidates: Vec<(i64, Vec<u8>)> = {
        let mut stmt = conn.prepare("SELECT id, data FROM blobs WHERE typeof(data) = 'text'")?;
        stmt.query_map([], |row| Ok((row.get(0)?, column_bytes(row, 1, "data")?)))?
            .collect::<Result<_, _>>()?
    };

    let mut repaired = 0usize;
    for (id, data) in candidates {
        let hash = blake3::hash(&data);
        let result = conn.execute(
            "UPDATE blobs SET hash = ?1, data = ?2 WHERE id = ?3",
            params![hash.as_bytes().as_slice(), data, id],
        );
        match result {
            Ok(_) => repaired += 1,
            Err(e) if is_unique_violation(&e) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(repaired)
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
    fn blob_bytes_tolerates_text_storage_class() {
        // A raw SQL UPDATE using a text function (e.g. replace()) on the
        // `data` column stores its result as TEXT storage class even though
        // the column is declared BLOB — SQLite doesn't coerce it back.
        let c = mem();
        let id = intern_blob(&c, b"require \"fileinto\";\nkeep;").unwrap();
        c.execute(
            "UPDATE blobs SET data = replace(data, 'keep', 'discard') WHERE id = ?1",
            params![id],
        )
        .unwrap();
        assert_eq!(
            blob_bytes(&c, id).unwrap().as_deref(),
            Some(&b"require \"fileinto\";\ndiscard;"[..])
        );
    }

    #[test]
    fn repair_stale_hashes_rehashes_rows_edited_by_raw_sql() {
        let c = mem();
        let id = intern_blob(&c, b"require \"fileinto\";\nkeep;").unwrap();
        c.execute(
            "UPDATE blobs SET data = replace(data, 'keep', 'discard') WHERE id = ?1",
            params![id],
        )
        .unwrap();

        let repaired = repair_stale_hashes(&c).unwrap();
        assert_eq!(repaired, 1);

        let expected = blake3::hash(b"require \"fileinto\";\ndiscard;");
        let stored_hash: Vec<u8> = c
            .query_row("SELECT hash FROM blobs WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stored_hash, expected.as_bytes().as_slice());
        assert_eq!(
            blob_bytes(&c, id).unwrap().as_deref(),
            Some(&b"require \"fileinto\";\ndiscard;"[..])
        );

        // Second call is a no-op: nothing left with TEXT storage class.
        assert_eq!(repair_stale_hashes(&c).unwrap(), 0);
    }

    #[test]
    fn repair_stale_hashes_skips_rows_that_collide_with_an_existing_blob() {
        let c = mem();
        let kept = intern_blob(&c, b"same content").unwrap();
        let edited = intern_blob(&c, b"will collide").unwrap();
        c.execute(
            "UPDATE blobs SET data = 'same content' WHERE id = ?1",
            params![edited],
        )
        .unwrap();

        let repaired = repair_stale_hashes(&c).unwrap();
        assert_eq!(repaired, 0);

        let edited_hash: Vec<u8> = c
            .query_row("SELECT hash FROM blobs WHERE id = ?1", params![edited], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            edited_hash,
            blake3::hash(b"will collide").as_bytes().as_slice(),
            "colliding row keeps its stale hash for manual resolution"
        );
        assert_eq!(
            blob_bytes(&c, edited).unwrap().as_deref(),
            Some(&b"same content"[..]),
            "content is still readable via the TEXT-storage-class tolerance"
        );
        assert!(blob_bytes(&c, kept).unwrap().is_some());
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
