/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::Row;

use super::JsonColumn;
use crate::archive::rows::{CalendarRecord, Subscribed, Write};
use crate::archive::{LocalId, Store};
use crate::jmap::error::JmapError;
use crate::jmap::wire::calendar::Calendar;

impl Calendar {
    pub(crate) const SELECT: &str = "SELECT id, name, description, color, sort_order, is_subscribed, \
         is_visible, is_default, include_in_availability, default_alerts_with_time, \
         default_alerts_without_time, time_zone FROM calendars";

    fn with_record<R>(
        &self,
        write: impl FnOnce(&CalendarRecord<'_>) -> rusqlite::Result<R>,
    ) -> Result<R, JmapError> {
        let with_time = JsonColumn::encode(self.default_alerts_with_time.as_ref())?;
        let without_time = JsonColumn::encode(self.default_alerts_without_time.as_ref())?;
        Ok(write(&CalendarRecord {
            name: &self.name,
            description: self.description.as_deref(),
            color: self.color.as_deref(),
            sort_order: i64::from(self.sort_order),
            subscribed: Subscribed::from(self.is_subscribed),
            is_visible: self.is_visible,
            is_default: self.is_default,
            include_in_availability: &self.include_in_availability,
            default_alerts_with_time: with_time.as_deref(),
            default_alerts_without_time: without_time.as_deref(),
            time_zone: self.time_zone.as_deref(),
        })?)
    }

    pub(crate) fn insert(&self, store: Store<'_>) -> Result<LocalId, JmapError> {
        self.with_record(|r| store.calendars().insert(r))
    }

    pub(crate) fn update(&self, store: Store<'_>, id: LocalId) -> Result<Write, JmapError> {
        self.with_record(|r| store.calendars().update(id, r))
    }

    pub(crate) fn from_row(row: &Row<'_>) -> Result<Calendar, JmapError> {
        Ok(Calendar {
            id: None,
            name: row.get(1)?,
            description: row.get(2)?,
            color: row.get(3)?,
            sort_order: row.get::<_, i64>(4)? as u32,
            is_subscribed: row.get::<_, i64>(5)? != 0,
            is_visible: row.get::<_, i64>(6)? != 0,
            is_default: row.get::<_, i64>(7)? != 0,
            include_in_availability: row.get(8)?,
            default_alerts_with_time: JsonColumn::decode(row.get(9)?)?,
            default_alerts_without_time: JsonColumn::decode(row.get(10)?)?,
            time_zone: row.get(11)?,
        })
    }
}
