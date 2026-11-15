/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::{OptionalExtension, params};

use super::Connection;
use crate::archive::{SourceId, Store};

#[derive(Debug, Clone, Copy)]
pub struct VacationRecord<'r> {
    pub is_enabled: bool,
    pub from_date: Option<&'r str>,
    pub to_date: Option<&'r str>,
    pub subject: Option<&'r str>,
    pub text_body: Option<&'r str>,
    pub html_body: Option<&'r str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredVacation {
    pub is_enabled: bool,
    pub from_date: Option<String>,
    pub to_date: Option<String>,
    pub subject: Option<String>,
    pub text_body: Option<String>,
    pub html_body: Option<String>,
}

#[derive(Clone, Copy)]
pub struct Vacation<'a> {
    conn: &'a Connection,
}

impl<'a> Store<'a> {
    pub fn vacation(self) -> Vacation<'a> {
        Vacation { conn: self.conn }
    }
}

impl Vacation<'_> {
    pub fn store(&self, source: SourceId, v: &VacationRecord<'_>) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO vacation_response
                 (id, source_id, is_enabled, from_date, to_date, subject, text_body, html_body)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                source,
                v.is_enabled,
                v.from_date,
                v.to_date,
                v.subject,
                v.text_body,
                v.html_body
            ],
        )?;
        Ok(())
    }

    pub fn clear(&self, source: SourceId) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM vacation_response WHERE source_id = ?1",
            [source],
        )?;
        Ok(())
    }

    pub fn exists(&self) -> rusqlite::Result<bool> {
        self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM vacation_response)",
            [],
            |row| row.get(0),
        )
    }

    pub fn load(&self) -> rusqlite::Result<Option<StoredVacation>> {
        self.conn
            .query_row(
                "SELECT is_enabled, from_date, to_date, subject, text_body, html_body
                 FROM vacation_response WHERE id = 1",
                [],
                |row| {
                    Ok(StoredVacation {
                        is_enabled: row.get(0)?,
                        from_date: row.get(1)?,
                        to_date: row.get(2)?,
                        subject: row.get(3)?,
                        text_body: row.get(4)?,
                        html_body: row.get(5)?,
                    })
                },
            )
            .optional()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Archive;
    use crate::archive::sources::{SourceKey, SourceKind};

    #[test]
    fn store_load_clear() {
        let archive = Archive::open_in_memory().unwrap();
        let store = archive.store();
        let source = store
            .sources()
            .upsert(
                &SourceKey {
                    kind: SourceKind::Jmap,
                    url: "https://a/jmap",
                    account_id: "u",
                },
                None,
                "u",
            )
            .unwrap();
        let vacation = store.vacation();
        assert!(!vacation.exists().unwrap());
        vacation
            .store(
                source,
                &VacationRecord {
                    is_enabled: true,
                    from_date: None,
                    to_date: None,
                    subject: Some("Away"),
                    text_body: Some("Back soon"),
                    html_body: None,
                },
            )
            .unwrap();
        let loaded = vacation.load().unwrap().unwrap();
        assert!(loaded.is_enabled);
        assert_eq!(loaded.subject.as_deref(), Some("Away"));
        vacation.clear(source).unwrap();
        assert!(!vacation.exists().unwrap());
    }
}
