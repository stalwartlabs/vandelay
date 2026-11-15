/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use serde_json::{Map, Value};

use crate::archive::rows::{CalendarEventRecord, EventDataType};
use crate::error::Error;
use crate::exchange::date::WireDate;
use crate::exchange::ews::parse::CalendarItemRaw;
use crate::exchange::ews::types::{CalendarItemType, ItemId};
use crate::exchange::ews::xml::ItemShape;
use crate::sync::TypeCounts;
use crate::types::ObjectType;

use super::coordinator::EwsCoordinator;
use super::folder_sync::Placement;

impl EwsCoordinator<'_> {
    pub fn import_event(
        &self,
        xml: &str,
        at: &Placement<'_>,
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        let raw = CalendarItemRaw::parse(xml)
            .map_err(|e| Error::Partial(format!("unparsable item: {e}")))?;
        if raw.id.id.is_empty() {
            counts.failed += 1;
            return Ok(());
        }
        if matches!(
            raw.calendar_item_type,
            Some(CalendarItemType::Occurrence | CalendarItemType::Exception)
        ) || raw
            .start
            .as_deref()
            .is_some_and(|s| WireDate(s).is_outlook_no_date())
        {
            counts.skipped += 1;
            return Ok(());
        }
        let modified = self.fetch_modified_occurrences(&raw);
        let mut built = raw.to_jscalendar_with_exceptions(&modified, self.items.owner_smtp);
        self.attach_links(&raw, &mut built.data)?;
        let calendars = [at.folder];
        let data = built.data.to_string();
        let record = CalendarEventRecord {
            calendars: &calendars,
            is_draft: built.is_draft,
            use_default_alerts: built.use_default_alerts,
            data_type: EventDataType::Event,
            data: &data,
        };

        let tx = self.store.begin().map_err(Error::partial)?;
        let store = tx.store();
        let ids = self.ids_in(store);
        match at.existing.get(&raw.id.id) {
            Some(&id) => {
                store
                    .calendar_events()
                    .update(id, &record)
                    .map_err(Error::partial)?;
                ids.update_change_key(ObjectType::CalendarEvent, &raw.id.id, &raw.id.change_key)
                    .map_err(Error::partial)?;
                counts.fetched += 1;
            }
            None => {
                let id = store
                    .calendar_events()
                    .insert(&record)
                    .map_err(Error::partial)?;
                ids.insert(
                    ObjectType::CalendarEvent,
                    at.folder_id,
                    &raw.id.id,
                    &raw.id.change_key,
                    id,
                )
                .map_err(Error::partial)?;
                counts.created += 1;
            }
        }
        tx.commit().map_err(Error::partial)
    }

    fn fetch_modified_occurrences(&self, raw: &CalendarItemRaw) -> Vec<CalendarItemRaw> {
        let ids: Vec<ItemId> = raw
            .modified_occurrences
            .iter()
            .filter(|o| !o.item_id.id.is_empty())
            .map(|o| o.item_id.clone())
            .collect();
        if ids.is_empty() {
            return Vec::new();
        }
        match self.items.get_items(ItemShape::CalendarItem, &ids) {
            Ok(outcome) => outcome
                .messages
                .iter()
                .filter(|m| m.success)
                .filter_map(|m| CalendarItemRaw::parse(&m.inner_xml).ok())
                .filter(|occ| !occ.id.id.is_empty())
                .collect(),
            Err(e) => {
                self.items.logger.warn(format_args!(
                    "GetItem (calendar exceptions) failed: {e}; overrides keep time-only changes"
                ));
                Vec::new()
            }
        }
    }

    fn attach_links(&self, raw: &CalendarItemRaw, data: &mut Value) -> Result<(), Error> {
        let refs: Vec<_> = raw
            .attachments
            .iter()
            .filter(|a| !a.is_item_attachment && !a.attachment_id.is_empty())
            .collect();
        if refs.is_empty() {
            return Ok(());
        }
        let ids: Vec<&str> = refs.iter().map(|a| a.attachment_id.as_str()).collect();
        let fetched = match self.items.fetch_attachments(&ids) {
            Ok(fetched) => fetched,
            Err(e) => {
                self.items.logger.warn(format_args!(
                    "calendar attachments fetch failed: {e}; continuing without enclosures"
                ));
                return Ok(());
            }
        };
        let mut links = Map::new();
        for (idx, att) in (1u32..).zip(fetched) {
            let blob = self
                .store
                .blobs()
                .intern(&att.bytes)
                .map_err(Error::partial)?;
            let declared = refs.iter().find(|r| r.attachment_id == att.attachment_id);
            let content_type = declared
                .and_then(|r| r.content_type.as_deref())
                .filter(|s| !s.is_empty())
                .map_or(att.media_type, str::to_owned);
            let mut entry = Map::new();
            entry.insert("@type".to_owned(), Value::String("Link".to_owned()));
            entry.insert("@blob".to_owned(), Value::from(blob.get()));
            entry.insert("contentType".to_owned(), Value::String(content_type));
            if let Some(name) = declared.and_then(|r| r.name.clone()) {
                entry.insert("title".to_owned(), Value::String(name));
            }
            entry.insert("rel".to_owned(), Value::String("enclosure".to_owned()));
            links.insert(idx.to_string(), Value::Object(entry));
        }
        if links.is_empty() {
            return Ok(());
        }
        if let Some(obj) = data.as_object_mut() {
            match obj.get_mut("links").and_then(Value::as_object_mut) {
                Some(existing) => existing.extend(links),
                None => {
                    obj.insert("links".to_owned(), Value::Object(links));
                }
            }
        }
        Ok(())
    }
}
