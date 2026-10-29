/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

pub const MAILBOX: &str = "mailbox";
pub const EMAIL: &str = "email";
pub const CALENDAR: &str = "calendar";
pub const CALENDAR_EVENT: &str = "calendarevent";
pub const ADDRESS_BOOK: &str = "addressbook";
pub const CONTACT_CARD: &str = "contactcard";
pub const FILE_NODE: &str = "filenode";

#[derive(Debug, Clone)]
pub struct IdRow {
    pub graph_id: String,
    pub local_id: i64,
}

pub fn insert(
    conn: &Connection,
    source_id: i64,
    type_name: &str,
    graph_id: &str,
    local_id: i64,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO sync_id_exchange_graph (source_id, type_name, graph_id, local_id)
         VALUES (?1, ?2, ?3, ?4)",
        params![source_id, type_name, graph_id, local_id],
    )?;
    Ok(())
}

pub fn delete(
    conn: &Connection,
    source_id: i64,
    type_name: &str,
    graph_id: &str,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "DELETE FROM sync_id_exchange_graph
         WHERE source_id = ?1 AND type_name = ?2 AND graph_id = ?3",
        params![source_id, type_name, graph_id],
    )?;
    Ok(())
}

pub fn local_for_graph_id(
    conn: &Connection,
    source_id: i64,
    type_name: &str,
    graph_id: &str,
) -> Result<Option<i64>, rusqlite::Error> {
    conn.query_row(
        "SELECT local_id FROM sync_id_exchange_graph
         WHERE source_id = ?1 AND type_name = ?2 AND graph_id = ?3",
        params![source_id, type_name, graph_id],
        |row| row.get(0),
    )
    .optional()
}

pub fn ids_of_type(
    conn: &Connection,
    source_id: i64,
    type_name: &str,
) -> Result<HashMap<String, i64>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT graph_id, local_id FROM sync_id_exchange_graph
         WHERE source_id = ?1 AND type_name = ?2",
    )?;
    let rows = stmt.query_map(params![source_id, type_name], |row| {
        Ok(IdRow {
            graph_id: row.get(0)?,
            local_id: row.get(1)?,
        })
    })?;
    let mut map = HashMap::new();
    for r in rows {
        let row = r?;
        map.insert(row.graph_id, row.local_id);
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init;
    use crate::db::sources::{SourceKey, upsert_source};

    fn setup() -> (Connection, i64) {
        let conn = Connection::open_in_memory().unwrap();
        init::apply_schema(&conn).unwrap();
        let key = SourceKey {
            kind: "exchange_graph".to_owned(),
            session_url: "https://x|https://graph.microsoft.com/v1.0".to_owned(),
            account_id: "u-uuid".to_owned(),
        };
        let sid = upsert_source(&conn, &key, Some("u@d"), "u@d").unwrap();
        (conn, sid)
    }

    #[test]
    fn insert_and_lookup_round_trip() {
        let (conn, sid) = setup();
        insert(&conn, sid, EMAIL, "AAkA-base64==", 17).unwrap();
        let id = local_for_graph_id(&conn, sid, EMAIL, "AAkA-base64==")
            .unwrap()
            .unwrap();
        assert_eq!(id, 17);
    }

    #[test]
    fn graph_id_is_case_sensitive() {
        let (conn, sid) = setup();
        insert(&conn, sid, EMAIL, "AaBb", 1).unwrap();
        assert!(
            local_for_graph_id(&conn, sid, EMAIL, "aabb")
                .unwrap()
                .is_none()
        );
        assert!(
            local_for_graph_id(&conn, sid, EMAIL, "AaBb")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn unique_local_id_per_type_is_enforced() {
        let (conn, sid) = setup();
        insert(&conn, sid, EMAIL, "ID-A", 1).unwrap();
        let err = insert(&conn, sid, EMAIL, "ID-B", 1).unwrap_err();
        assert!(err.to_string().contains("UNIQUE"));
    }

    #[test]
    fn ids_of_type_filters_by_type() {
        let (conn, sid) = setup();
        insert(&conn, sid, EMAIL, "M1", 1).unwrap();
        insert(&conn, sid, EMAIL, "M2", 2).unwrap();
        insert(&conn, sid, MAILBOX, "F1", 3).unwrap();
        let emails = ids_of_type(&conn, sid, EMAIL).unwrap();
        let folders = ids_of_type(&conn, sid, MAILBOX).unwrap();
        assert_eq!(emails.len(), 2);
        assert_eq!(folders.len(), 1);
        assert_eq!(emails.get("M1"), Some(&1));
        assert_eq!(folders.get("F1"), Some(&3));
    }

    #[test]
    fn delete_removes_only_one() {
        let (conn, sid) = setup();
        insert(&conn, sid, EMAIL, "M1", 1).unwrap();
        insert(&conn, sid, EMAIL, "M2", 2).unwrap();
        delete(&conn, sid, EMAIL, "M1").unwrap();
        let remaining = ids_of_type(&conn, sid, EMAIL).unwrap();
        assert_eq!(remaining.len(), 1);
        assert!(remaining.contains_key("M2"));
    }
}
