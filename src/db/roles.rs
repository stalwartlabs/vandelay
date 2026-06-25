/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::{Connection, params};

pub fn unique_role(
    conn: &Connection,
    role: Option<&str>,
    exclude_id: Option<i64>,
) -> rusqlite::Result<Option<String>> {
    let Some(r) = role else {
        return Ok(None);
    };
    let taken: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM mailboxes WHERE role = ?1 AND (?2 IS NULL OR id != ?2))",
        params![r, exclude_id],
        |row| row.get(0),
    )?;
    Ok((!taken).then(|| r.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init::apply_schema(&conn).unwrap();
        conn
    }

    fn insert(conn: &Connection, name: &str, role: Option<&str>) -> i64 {
        conn.execute(
            "INSERT INTO mailboxes (name, parent_id, role, sort_order, is_subscribed)
             VALUES (?1, NULL, ?2, 0, 1)",
            params![name, role],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn first_claimant_keeps_role_second_is_dropped() {
        let conn = mem();
        let first = unique_role(&conn, Some("sent"), None).unwrap();
        assert_eq!(first.as_deref(), Some("sent"));
        insert(&conn, "Sent", first.as_deref());

        let second = unique_role(&conn, Some("sent"), None).unwrap();
        assert_eq!(second, None);
    }

    #[test]
    fn null_role_stays_null() {
        let conn = mem();
        assert_eq!(unique_role(&conn, None, None).unwrap(), None);
    }

    #[test]
    fn distinct_roles_are_independent() {
        let conn = mem();
        insert(&conn, "Sent", Some("sent"));
        assert_eq!(
            unique_role(&conn, Some("trash"), None).unwrap().as_deref(),
            Some("trash")
        );
    }

    #[test]
    fn update_excludes_own_row() {
        let conn = mem();
        let id = insert(&conn, "Sent", Some("sent"));
        assert_eq!(
            unique_role(&conn, Some("sent"), Some(id))
                .unwrap()
                .as_deref(),
            Some("sent")
        );
    }

    #[test]
    fn update_detects_other_holder() {
        let conn = mem();
        insert(&conn, "Sent", Some("sent"));
        let other = insert(&conn, "Envoyés", None);
        assert_eq!(unique_role(&conn, Some("sent"), Some(other)).unwrap(), None);
    }
}
