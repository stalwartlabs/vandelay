/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::types::ObjectType;
use rusqlite::{Connection, params};

pub fn unique_default(
    conn: &Connection,
    ty: ObjectType,
    is_default: bool,
    exclude_id: Option<i64>,
) -> rusqlite::Result<bool> {
    if !is_default {
        return Ok(false);
    }
    let Some(table) = table_for(ty) else {
        return Ok(false);
    };
    let taken: bool = conn.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM {table} \
             WHERE is_default = 1 AND (?1 IS NULL OR id != ?1))"
        ),
        params![exclude_id],
        |row| row.get(0),
    )?;
    Ok(!taken)
}

fn table_for(ty: ObjectType) -> Option<&'static str> {
    match ty {
        ObjectType::AddressBook => Some("address_books"),
        ObjectType::Calendar => Some("calendars"),
        ObjectType::ParticipantIdentity => Some("participant_identities"),
        _ => None,
    }
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

    fn insert_book(conn: &Connection, name: &str, is_default: bool) -> i64 {
        conn.execute(
            "INSERT INTO address_books (name, sort_order, is_default, is_subscribed)
             VALUES (?1, 0, ?2, 1)",
            params![name, is_default as i64],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn first_claimant_keeps_the_default_second_is_dropped() {
        let conn = mem();
        let first = unique_default(&conn, ObjectType::AddressBook, true, None).unwrap();
        assert!(first);
        insert_book(&conn, "Contacts", first);

        assert!(
            !unique_default(&conn, ObjectType::AddressBook, true, None).unwrap(),
            "an archive must never hold two default address books"
        );
    }

    #[test]
    fn non_default_stays_non_default() {
        let conn = mem();
        assert!(!unique_default(&conn, ObjectType::AddressBook, false, None).unwrap());
    }

    #[test]
    fn each_type_has_its_own_default() {
        let conn = mem();
        insert_book(&conn, "Contacts", true);
        assert!(
            unique_default(&conn, ObjectType::Calendar, true, None).unwrap(),
            "a default address book must not block a default calendar"
        );
    }

    #[test]
    fn update_excludes_its_own_row() {
        let conn = mem();
        let id = insert_book(&conn, "Contacts", true);
        assert!(unique_default(&conn, ObjectType::AddressBook, true, Some(id)).unwrap());
    }

    #[test]
    fn update_detects_another_holder() {
        let conn = mem();
        insert_book(&conn, "Contacts", true);
        let other = insert_book(&conn, "GAL", false);
        assert!(!unique_default(&conn, ObjectType::AddressBook, true, Some(other)).unwrap());
    }

    #[test]
    fn types_without_a_default_column_are_never_default() {
        let conn = mem();
        assert!(!unique_default(&conn, ObjectType::Mailbox, true, None).unwrap());
    }
}
