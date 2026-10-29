/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::{Connection, OptionalExtension, params};

#[derive(Debug, Clone)]
pub struct SourceKey {
    pub kind: String,
    pub session_url: String,
    pub account_id: String,
}

pub fn find_source(conn: &Connection, key: &SourceKey) -> Result<Option<i64>, rusqlite::Error> {
    conn.query_row(
        "SELECT id FROM sources WHERE kind = ?1 AND session_url = ?2 AND account_id = ?3",
        params![key.kind, key.session_url, key.account_id],
        |row| row.get(0),
    )
    .optional()
}

pub fn conflicting_source(
    conn: &Connection,
    kind: &str,
    session_url: &str,
    account_id: &str,
) -> Result<Option<(String, String)>, rusqlite::Error> {
    conn.query_row(
        "SELECT session_url, account_id FROM sources
         WHERE kind = ?1 AND NOT (session_url = ?2 AND account_id = ?3) LIMIT 1",
        params![kind, session_url, account_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
}

pub fn other_sources(
    conn: &Connection,
    kind: &str,
    session_url: &str,
    account_id: &str,
) -> Result<Vec<(i64, String, String)>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, session_url, account_id FROM sources
         WHERE kind = ?1 AND NOT (session_url = ?2 AND account_id = ?3) ORDER BY id",
    )?;
    stmt.query_map(params![kind, session_url, account_id], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    })?
    .collect()
}

pub fn delete_source(conn: &Connection, id: i64) -> Result<(), rusqlite::Error> {
    conn.execute("DELETE FROM sources WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn upsert_source(
    conn: &Connection,
    key: &SourceKey,
    account_name: Option<&str>,
    username: &str,
) -> Result<i64, rusqlite::Error> {
    conn.execute(
        "INSERT INTO sources (kind, session_url, account_id, account_name, username)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (kind, session_url, account_id)
         DO UPDATE SET account_name = excluded.account_name, username = excluded.username",
        params![
            key.kind,
            key.session_url,
            key.account_id,
            account_name,
            username
        ],
    )?;
    find_source(conn, key).map(|opt| opt.unwrap_or_else(|| conn.last_insert_rowid()))
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

    fn key(url: &str, acc: &str) -> SourceKey {
        SourceKey {
            kind: "jmap".to_owned(),
            session_url: url.to_owned(),
            account_id: acc.to_owned(),
        }
    }

    #[test]
    fn upsert_is_idempotent_and_returns_stable_id() {
        let c = mem();
        let k = key("https://a/jmap", "w");
        let id1 = upsert_source(&c, &k, Some("alice"), "alice").unwrap();
        let id2 = upsert_source(&c, &k, Some("alice2"), "alice").unwrap();
        assert_eq!(id1, id2);
        assert!(find_source(&c, &k).unwrap().is_some());
    }

    #[test]
    fn conflicting_source_detects_a_different_account() {
        let c = mem();
        upsert_source(&c, &key("https://a/jmap", "w"), None, "alice").unwrap();
        assert!(
            conflicting_source(&c, "jmap", "https://a/jmap", "w")
                .unwrap()
                .is_none()
        );
        let conflict = conflicting_source(&c, "jmap", "https://b/jmap", "v")
            .unwrap()
            .unwrap();
        assert_eq!(conflict, ("https://a/jmap".to_owned(), "w".to_owned()));
    }

    #[test]
    fn other_sources_lists_every_other_source_of_the_kind() {
        let c = mem();
        let a = upsert_source(&c, &key("https://a/jmap", "w"), None, "alice").unwrap();
        let b = upsert_source(&c, &key("https://b/jmap", "w"), None, "alice").unwrap();
        upsert_source(&c, &key("https://c/jmap", "w"), None, "alice").unwrap();
        let others = other_sources(&c, "jmap", "https://c/jmap", "w").unwrap();
        let ids: Vec<i64> = others.iter().map(|(id, _, _)| *id).collect();
        assert_eq!(ids, [a, b]);
        assert!(
            other_sources(&c, "imap", "https://c/jmap", "w")
                .unwrap()
                .is_empty()
        );
    }
}
