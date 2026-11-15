/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::params;

use super::{ParticipantIdentities, Write};
use crate::archive::{LocalId, Store};
use crate::types::ObjectType;

#[derive(Debug, Clone, Copy)]
pub struct ParticipantIdentityRecord<'r> {
    pub name: &'r str,
    pub calendar_address: &'r str,
    pub is_default: bool,
}

impl ParticipantIdentities<'_> {
    pub fn insert(&self, p: &ParticipantIdentityRecord<'_>) -> rusqlite::Result<LocalId> {
        let is_default = Store::new(self.conn).unique_default(
            ObjectType::ParticipantIdentity,
            p.is_default,
            None,
        )?;
        self.conn.execute(
            "INSERT INTO participant_identities (name, calendar_address, is_default)
             VALUES (?1, ?2, ?3)",
            params![p.name, p.calendar_address, is_default],
        )?;
        Ok(LocalId(self.conn.last_insert_rowid()))
    }

    pub fn update(
        &self,
        id: LocalId,
        p: &ParticipantIdentityRecord<'_>,
    ) -> rusqlite::Result<Write> {
        let is_default = Store::new(self.conn).unique_default(
            ObjectType::ParticipantIdentity,
            p.is_default,
            Some(id),
        )?;
        self.conn
            .execute(
                "UPDATE participant_identities SET name = ?1, calendar_address = ?2, is_default = ?3
                 WHERE id = ?4 AND (name IS NOT ?1 OR calendar_address IS NOT ?2
                    OR is_default IS NOT ?3)",
                params![p.name, p.calendar_address, is_default, id],
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
        let identities = archive.store().participant_identities();
        let mut record = ParticipantIdentityRecord {
            name: "Alice",
            calendar_address: "mailto:alice@example.com",
            is_default: true,
        };
        let id = identities.insert(&record).unwrap();
        assert_eq!(identities.update(id, &record).unwrap(), Write::Unchanged);
        record.name = "A";
        assert_eq!(identities.update(id, &record).unwrap(), Write::Changed);
        identities.delete(id).unwrap();
    }
}
