/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::params;

use super::{AddressBooks, Subscribed, Write};
use crate::archive::{LocalId, Store};
use crate::types::ObjectType;

#[derive(Debug, Clone, Copy)]
pub struct AddressBookRecord<'r> {
    pub name: &'r str,
    pub description: Option<&'r str>,
    pub sort_order: i64,
    pub is_default: bool,
    pub subscribed: Subscribed,
}

impl AddressBooks<'_> {
    pub fn insert(&self, a: &AddressBookRecord<'_>) -> rusqlite::Result<LocalId> {
        let is_default =
            Store::new(self.conn).unique_default(ObjectType::AddressBook, a.is_default, None)?;
        self.conn.execute(
            "INSERT INTO address_books (name, description, sort_order, is_default, is_subscribed)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                a.name,
                a.description,
                a.sort_order,
                is_default,
                a.subscribed
            ],
        )?;
        Ok(LocalId(self.conn.last_insert_rowid()))
    }

    pub fn update(&self, id: LocalId, a: &AddressBookRecord<'_>) -> rusqlite::Result<Write> {
        let is_default = Store::new(self.conn).unique_default(
            ObjectType::AddressBook,
            a.is_default,
            Some(id),
        )?;
        self.conn
            .execute(
                "UPDATE address_books SET name = ?1, description = ?2, sort_order = ?3,
                    is_default = ?4, is_subscribed = ?5
                 WHERE id = ?6 AND (name IS NOT ?1 OR description IS NOT ?2 OR sort_order IS NOT ?3
                    OR is_default IS NOT ?4 OR is_subscribed IS NOT ?5)",
                params![
                    a.name,
                    a.description,
                    a.sort_order,
                    is_default,
                    a.subscribed,
                    id
                ],
            )
            .map(Write::from_rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Archive;

    #[test]
    fn second_default_is_demoted() {
        let archive = Archive::open_in_memory().unwrap();
        let books = archive.store().address_books();
        let record = AddressBookRecord {
            name: "Personal",
            description: None,
            sort_order: 0,
            is_default: true,
            subscribed: Subscribed::Yes,
        };
        let first = books.insert(&record).unwrap();
        let second = books.insert(&record).unwrap();
        let is_default = |id: LocalId| -> bool {
            archive
                .connection()
                .query_row(
                    "SELECT is_default FROM address_books WHERE id = ?1",
                    [id],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert!(is_default(first));
        assert!(!is_default(second));
        assert_eq!(books.update(first, &record).unwrap(), Write::Unchanged);
        books.delete(second).unwrap();
    }
}
