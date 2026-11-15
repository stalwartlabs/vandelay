/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::params;
use rusqlite::types::{ToSql, ToSqlOutput};

use super::{CalendarEvents, LocalIdArray, Write};
use crate::archive::LocalId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventDataType {
    Event,
    Task,
    Note,
    Group,
}

impl EventDataType {
    pub fn as_str(self) -> &'static str {
        match self {
            EventDataType::Event => "Event",
            EventDataType::Task => "Task",
            EventDataType::Note => "Note",
            EventDataType::Group => "Group",
        }
    }
}

impl ToSql for EventDataType {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CalendarEventRecord<'r> {
    pub calendars: &'r [LocalId],
    pub is_draft: bool,
    pub use_default_alerts: bool,
    pub data_type: EventDataType,
    pub data: &'r str,
}

impl CalendarEvents<'_> {
    pub fn insert(&self, e: &CalendarEventRecord<'_>) -> rusqlite::Result<LocalId> {
        self.conn.execute(
            "INSERT INTO calendar_events (calendar_ids, is_draft, use_default_alerts, data, data_type)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                LocalIdArray(e.calendars),
                e.is_draft,
                e.use_default_alerts,
                e.data,
                e.data_type
            ],
        )?;
        Ok(LocalId(self.conn.last_insert_rowid()))
    }

    pub fn update(&self, id: LocalId, e: &CalendarEventRecord<'_>) -> rusqlite::Result<Write> {
        self.conn
            .execute(
                "UPDATE calendar_events SET calendar_ids = ?1, is_draft = ?2,
                    use_default_alerts = ?3, data = ?4, data_type = ?5
                 WHERE id = ?6 AND (calendar_ids IS NOT ?1 OR is_draft IS NOT ?2
                    OR use_default_alerts IS NOT ?3 OR data IS NOT ?4 OR data_type IS NOT ?5)",
                params![
                    LocalIdArray(e.calendars),
                    e.is_draft,
                    e.use_default_alerts,
                    e.data,
                    e.data_type,
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
        let events = archive.store().calendar_events();
        let calendars = [LocalId::from_raw(1), LocalId::from_raw(2)];
        let mut record = CalendarEventRecord {
            calendars: &calendars,
            is_draft: false,
            use_default_alerts: false,
            data_type: EventDataType::Event,
            data: r#"{"uid":"e1"}"#,
        };
        let id = events.insert(&record).unwrap();
        assert_eq!(events.update(id, &record).unwrap(), Write::Unchanged);
        record.data_type = EventDataType::Task;
        assert_eq!(events.update(id, &record).unwrap(), Write::Changed);
        let stored: String = archive
            .connection()
            .query_row(
                "SELECT data_type FROM calendar_events WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, "Task");
        events.delete(id).unwrap();
    }
}
