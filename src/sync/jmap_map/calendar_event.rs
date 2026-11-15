/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use serde_json::Value;

use super::{IdRefs, JsData, LocalIds};
use crate::archive::rows::{CalendarEventRecord, EventDataType, Write};
use crate::archive::{BlobId, LocalId, Store};
use crate::jmap::blob::InlineShape;
use crate::jmap::error::JmapError;
use crate::jmap::wire::JmapId;
use crate::jmap::wire::calendar_event::CalendarEvent;
use crate::types::ObjectType;

const SERVER_SET: [&str; 5] = ["method", "utcStart", "utcEnd", "isOrigin", "baseEventId"];

impl CalendarEvent {
    pub(crate) const SELECT: &str = "SELECT id, calendar_ids, is_draft, use_default_alerts, data FROM calendar_events \
         WHERE data_type = 'Event'";

    fn with_record<R>(
        self,
        resolve: &impl Fn(ObjectType, &str) -> Option<LocalId>,
        intern: &mut impl FnMut(&str) -> Result<BlobId, JmapError>,
        write: impl FnOnce(&CalendarEventRecord<'_>) -> rusqlite::Result<R>,
    ) -> Result<R, JmapError> {
        let calendars = IdRefs(&self.calendar_ids).to_local(ObjectType::Calendar, resolve)?;
        let mut data = JsData::from_rest(self.rest);
        data.remove_all(&SERVER_SET);
        data.intern_blobs(intern)?;
        Ok(write(&CalendarEventRecord {
            calendars: &calendars,
            is_draft: self.is_draft,
            use_default_alerts: self.use_default_alerts,
            data_type: EventDataType::Event,
            data: &data.text(),
        })?)
    }

    pub(crate) fn insert(
        self,
        store: Store<'_>,
        resolve: &impl Fn(ObjectType, &str) -> Option<LocalId>,
        intern: &mut impl FnMut(&str) -> Result<BlobId, JmapError>,
    ) -> Result<LocalId, JmapError> {
        self.with_record(resolve, intern, |r| store.calendar_events().insert(r))
    }

    pub(crate) fn update(
        self,
        store: Store<'_>,
        id: LocalId,
        resolve: &impl Fn(ObjectType, &str) -> Option<LocalId>,
        intern: &mut impl FnMut(&str) -> Result<BlobId, JmapError>,
    ) -> Result<Write, JmapError> {
        self.with_record(resolve, intern, |r| store.calendar_events().update(id, r))
    }

    pub(crate) fn export_json(
        calendar_ids: &str,
        is_draft: bool,
        use_default_alerts: bool,
        data: &str,
        resolve: &impl Fn(ObjectType, LocalId) -> Option<JmapId>,
        bytes: &impl Fn(BlobId) -> Result<Vec<u8>, JmapError>,
    ) -> Result<Value, JmapError> {
        let mut value = JsData::parse(data)?;
        value.inline_blobs(InlineShape::JsCalendarLink, bytes)?;
        let calendars = LocalIds::parse(calendar_ids)?.to_target(ObjectType::Calendar, resolve)?;
        value.insert("calendarIds", calendars);
        value.insert("isDraft", Value::Bool(is_draft));
        value.insert("useDefaultAlerts", Value::Bool(use_default_alerts));
        Ok(value.0)
    }
}
