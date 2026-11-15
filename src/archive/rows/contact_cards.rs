/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::params;

use super::{ContactCards, LocalIdArray, Write};
use crate::archive::LocalId;

#[derive(Debug, Clone, Copy)]
pub struct ContactCardRecord<'r> {
    pub uid: &'r str,
    pub address_books: &'r [LocalId],
    pub data: &'r str,
}

impl ContactCards<'_> {
    pub fn insert(&self, c: &ContactCardRecord<'_>) -> rusqlite::Result<LocalId> {
        self.conn.execute(
            "INSERT INTO contact_cards (uid, address_book_ids, data) VALUES (?1, ?2, ?3)",
            params![c.uid, LocalIdArray(c.address_books), c.data],
        )?;
        Ok(LocalId(self.conn.last_insert_rowid()))
    }

    pub fn update(&self, id: LocalId, c: &ContactCardRecord<'_>) -> rusqlite::Result<Write> {
        self.conn
            .execute(
                "UPDATE contact_cards SET uid = ?1, address_book_ids = ?2, data = ?3
                 WHERE id = ?4 AND (uid IS NOT ?1 OR address_book_ids IS NOT ?2 OR data IS NOT ?3)",
                params![c.uid, LocalIdArray(c.address_books), c.data, id],
            )
            .map(Write::from_rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Archive;

    #[test]
    fn insert_update_delete() {
        let archive = Archive::open_in_memory().unwrap();
        let cards = archive.store().contact_cards();
        let books = [LocalId::from_raw(3)];
        let mut record = ContactCardRecord {
            uid: "u1",
            address_books: &books,
            data: r#"{"name":{"full":"A"}}"#,
        };
        let id = cards.insert(&record).unwrap();
        assert_eq!(cards.update(id, &record).unwrap(), Write::Unchanged);
        record.data = r#"{"name":{"full":"B"}}"#;
        assert_eq!(cards.update(id, &record).unwrap(), Write::Changed);
        cards.delete(id).unwrap();
    }
}
