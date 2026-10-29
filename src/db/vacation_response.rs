/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::{Connection, OptionalExtension, params};

use crate::jmap::wire::vacation_response::VacationResponse;

pub fn store(
    conn: &Connection,
    source_id: i64,
    vacation: &VacationResponse,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT OR REPLACE INTO vacation_response
             (id, source_id, is_enabled, from_date, to_date, subject, text_body, html_body)
         VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            source_id,
            vacation.is_enabled,
            vacation.from_date,
            vacation.to_date,
            vacation.subject,
            vacation.text_body,
            vacation.html_body,
        ],
    )?;
    Ok(())
}

pub fn clear(conn: &Connection, source_id: i64) -> Result<(), rusqlite::Error> {
    conn.execute(
        "DELETE FROM vacation_response WHERE source_id = ?1",
        params![source_id],
    )?;
    Ok(())
}

pub fn exists(conn: &Connection) -> Result<bool, rusqlite::Error> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM vacation_response)",
        [],
        |row| row.get(0),
    )
}

pub fn load(conn: &Connection) -> Result<Option<VacationResponse>, rusqlite::Error> {
    conn.query_row(
        "SELECT is_enabled, from_date, to_date, subject, text_body, html_body
         FROM vacation_response WHERE id = 1",
        [],
        |row| {
            Ok(VacationResponse {
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
