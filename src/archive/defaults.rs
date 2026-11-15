/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::archive::{LocalId, Store};
use crate::types::ObjectType;

macro_rules! default_taken {
    ($table:literal) => {
        concat!(
            "SELECT EXISTS(SELECT 1 FROM ",
            $table,
            " WHERE is_default = 1 AND (?1 IS NULL OR id != ?1))"
        )
    };
}

impl Store<'_> {
    pub fn unique_default(
        self,
        ty: ObjectType,
        is_default: bool,
        exclude: Option<LocalId>,
    ) -> rusqlite::Result<bool> {
        if !is_default {
            return Ok(false);
        }
        let sql = match ty {
            ObjectType::AddressBook => default_taken!("address_books"),
            ObjectType::Calendar => default_taken!("calendars"),
            ObjectType::ParticipantIdentity => default_taken!("participant_identities"),
            _ => return Ok(false),
        };
        let taken: bool = self.conn.query_row(sql, [exclude], |row| row.get(0))?;
        Ok(!taken)
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::params;

    use crate::archive::{Archive, LocalId};
    use crate::types::ObjectType;

    fn insert_book(archive: &Archive, name: &str, is_default: bool) -> LocalId {
        let conn = archive.connection();
        conn.execute(
            "INSERT INTO address_books (name, sort_order, is_default, is_subscribed)
             VALUES (?1, 0, ?2, 1)",
            params![name, is_default as i64],
        )
        .unwrap();
        LocalId::from_raw(conn.last_insert_rowid())
    }

    #[test]
    fn first_claimant_keeps_the_default_second_is_dropped() {
        let archive = Archive::open_in_memory().unwrap();
        let store = archive.store();
        let first = store
            .unique_default(ObjectType::AddressBook, true, None)
            .unwrap();
        assert!(first);
        insert_book(&archive, "Contacts", first);

        assert!(
            !store
                .unique_default(ObjectType::AddressBook, true, None)
                .unwrap(),
            "an archive must never hold two default address books"
        );
    }

    #[test]
    fn non_default_stays_non_default() {
        let archive = Archive::open_in_memory().unwrap();
        assert!(
            !archive
                .store()
                .unique_default(ObjectType::AddressBook, false, None)
                .unwrap()
        );
    }

    #[test]
    fn each_type_has_its_own_default() {
        let archive = Archive::open_in_memory().unwrap();
        insert_book(&archive, "Contacts", true);
        assert!(
            archive
                .store()
                .unique_default(ObjectType::Calendar, true, None)
                .unwrap(),
            "a default address book must not block a default calendar"
        );
    }

    #[test]
    fn update_excludes_its_own_row() {
        let archive = Archive::open_in_memory().unwrap();
        let id = insert_book(&archive, "Contacts", true);
        assert!(
            archive
                .store()
                .unique_default(ObjectType::AddressBook, true, Some(id))
                .unwrap()
        );
    }

    #[test]
    fn update_detects_another_holder() {
        let archive = Archive::open_in_memory().unwrap();
        insert_book(&archive, "Contacts", true);
        let other = insert_book(&archive, "GAL", false);
        assert!(
            !archive
                .store()
                .unique_default(ObjectType::AddressBook, true, Some(other))
                .unwrap()
        );
    }

    #[test]
    fn types_without_a_default_column_are_never_default() {
        let archive = Archive::open_in_memory().unwrap();
        assert!(
            !archive
                .store()
                .unique_default(ObjectType::Mailbox, true, None)
                .unwrap()
        );
    }

    #[test]
    fn participant_identities_have_their_own_default() {
        let archive = Archive::open_in_memory().unwrap();
        let store = archive.store();
        archive
            .connection()
            .execute(
                "INSERT INTO participant_identities (name, calendar_address, is_default)
                 VALUES ('me', 'mailto:me@example.org', 1)",
                [],
            )
            .unwrap();
        assert!(
            !store
                .unique_default(ObjectType::ParticipantIdentity, true, None)
                .unwrap()
        );
    }
}
