/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::params;

use super::{Identities, Write};
use crate::archive::LocalId;

#[derive(Debug, Clone, Copy)]
pub struct IdentityRecord<'r> {
    pub name: &'r str,
    pub email: &'r str,
    pub reply_to: Option<&'r str>,
    pub bcc: Option<&'r str>,
    pub text_signature: &'r str,
    pub html_signature: &'r str,
}

impl Identities<'_> {
    pub fn insert(&self, i: &IdentityRecord<'_>) -> rusqlite::Result<LocalId> {
        self.conn.execute(
            "INSERT INTO identities (name, email, reply_to, bcc, text_signature, html_signature)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                i.name,
                i.email,
                i.reply_to,
                i.bcc,
                i.text_signature,
                i.html_signature
            ],
        )?;
        Ok(LocalId(self.conn.last_insert_rowid()))
    }

    pub fn update(&self, id: LocalId, i: &IdentityRecord<'_>) -> rusqlite::Result<Write> {
        self.conn
            .execute(
                "UPDATE identities SET name = ?1, email = ?2, reply_to = ?3, bcc = ?4,
                    text_signature = ?5, html_signature = ?6
                 WHERE id = ?7 AND (name IS NOT ?1 OR email IS NOT ?2 OR reply_to IS NOT ?3
                    OR bcc IS NOT ?4 OR text_signature IS NOT ?5 OR html_signature IS NOT ?6)",
                params![
                    i.name,
                    i.email,
                    i.reply_to,
                    i.bcc,
                    i.text_signature,
                    i.html_signature,
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
    fn insert_update_delete() {
        let archive = Archive::open_in_memory().unwrap();
        let identities = archive.store().identities();
        let mut record = IdentityRecord {
            name: "Alice",
            email: "alice@example.com",
            reply_to: None,
            bcc: Some(r#"[{"email":"b@example.com"}]"#),
            text_signature: "",
            html_signature: "",
        };
        let id = identities.insert(&record).unwrap();
        assert_eq!(identities.update(id, &record).unwrap(), Write::Unchanged);
        record.name = "Alice A.";
        assert_eq!(identities.update(id, &record).unwrap(), Write::Changed);
        identities.delete(id).unwrap();
    }
}
