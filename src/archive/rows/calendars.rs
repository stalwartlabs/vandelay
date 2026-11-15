/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::params;

use super::{Calendars, Subscribed, Write};
use crate::archive::{LocalId, Store};
use crate::types::ObjectType;

#[derive(Debug, Clone, Copy)]
pub struct CalendarRecord<'r> {
    pub name: &'r str,
    pub description: Option<&'r str>,
    pub color: Option<&'r str>,
    pub sort_order: i64,
    pub subscribed: Subscribed,
    pub is_visible: bool,
    pub is_default: bool,
    pub include_in_availability: &'r str,
    pub default_alerts_with_time: Option<&'r str>,
    pub default_alerts_without_time: Option<&'r str>,
    pub time_zone: Option<&'r str>,
}

impl Calendars<'_> {
    pub fn insert(&self, c: &CalendarRecord<'_>) -> rusqlite::Result<LocalId> {
        let is_default =
            Store::new(self.conn).unique_default(ObjectType::Calendar, c.is_default, None)?;
        self.conn.execute(
            "INSERT INTO calendars (name, description, color, sort_order, is_subscribed, is_visible,
                is_default, include_in_availability, default_alerts_with_time,
                default_alerts_without_time, time_zone)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                c.name,
                c.description,
                c.color,
                c.sort_order,
                c.subscribed,
                c.is_visible,
                is_default,
                c.include_in_availability,
                c.default_alerts_with_time,
                c.default_alerts_without_time,
                c.time_zone
            ],
        )?;
        Ok(LocalId(self.conn.last_insert_rowid()))
    }

    pub fn update(&self, id: LocalId, c: &CalendarRecord<'_>) -> rusqlite::Result<Write> {
        let is_default =
            Store::new(self.conn).unique_default(ObjectType::Calendar, c.is_default, Some(id))?;
        self.conn
            .execute(
                "UPDATE calendars SET name = ?1, description = ?2, color = ?3, sort_order = ?4,
                    is_subscribed = ?5, is_visible = ?6, is_default = ?7,
                    include_in_availability = ?8, default_alerts_with_time = ?9,
                    default_alerts_without_time = ?10, time_zone = ?11
                 WHERE id = ?12 AND (name IS NOT ?1 OR description IS NOT ?2 OR color IS NOT ?3
                    OR sort_order IS NOT ?4 OR is_subscribed IS NOT ?5 OR is_visible IS NOT ?6
                    OR is_default IS NOT ?7 OR include_in_availability IS NOT ?8
                    OR default_alerts_with_time IS NOT ?9 OR default_alerts_without_time IS NOT ?10
                    OR time_zone IS NOT ?11)",
                params![
                    c.name,
                    c.description,
                    c.color,
                    c.sort_order,
                    c.subscribed,
                    c.is_visible,
                    is_default,
                    c.include_in_availability,
                    c.default_alerts_with_time,
                    c.default_alerts_without_time,
                    c.time_zone,
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
        let calendars = archive.store().calendars();
        let mut record = CalendarRecord {
            name: "Work",
            description: None,
            color: Some("#ff0000"),
            sort_order: 1,
            subscribed: Subscribed::Yes,
            is_visible: true,
            is_default: false,
            include_in_availability: "all",
            default_alerts_with_time: None,
            default_alerts_without_time: None,
            time_zone: Some("Europe/Paris"),
        };
        let id = calendars.insert(&record).unwrap();
        assert_eq!(calendars.update(id, &record).unwrap(), Write::Unchanged);
        record.subscribed = Subscribed::No;
        assert_eq!(calendars.update(id, &record).unwrap(), Write::Changed);
        calendars.delete(id).unwrap();
    }
}
