/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashSet;
use std::mem;

use encodify::base64::STANDARD;
use serde_json::{Map, Value, json};

use crate::archive::Store;
use crate::archive::rows::{CalendarEventRecord, EventDataType};
use crate::console::{Counted, Grouped};
use crate::error::Error;
use crate::exchange::graph::api::{
    Endpoints, PREFER_BODY_HTML, PREFER_BODY_TEXT, PREFER_TIMEZONE_UTC,
};
use crate::exchange::graph::calendar_map::{ConvertedEvent, EventType, GraphEvent};
use crate::exchange::graph::client::GraphClient;
use crate::exchange::graph::error::GraphError;
use crate::exchange::graph::types::EventBodyFormat;
use crate::sync::TypeCounts;
use crate::sync::diff::IdDiff;
use crate::sync::pool::Pool;
use crate::types::ObjectType;

use super::coordinator::{CHUNK_SIZE, EventAttachments, GraphCoordinator};
use super::folders::GraphFolder;
use super::series::ExceptionWindow;

const FILE_ATTACHMENT: &str = "#microsoft.graph.fileAttachment";
const DEFAULT_ATTACHMENT_NAME: &str = "attachment";
const OCTET_STREAM: &str = "application/octet-stream";

struct EventAttachment {
    name: String,
    content_type: String,
    bytes: Vec<u8>,
}

impl EventAttachment {
    fn fetch_all(client: &GraphClient, endpoints: &Endpoints, event_id: &str) -> Vec<Self> {
        let Ok(body) = client.get_json_with_prefer(&endpoints.event_attachments(event_id), &[])
        else {
            return Vec::new();
        };
        let Some(items) = body.get("value").and_then(Value::as_array) else {
            return Vec::new();
        };
        items
            .iter()
            .filter_map(|item| {
                let text = |key: &str| item.get(key).and_then(Value::as_str);
                if text("@odata.type") != Some(FILE_ATTACHMENT) {
                    return None;
                }
                let bytes = STANDARD.decode(text("contentBytes")?).ok()?;
                (!bytes.is_empty()).then(|| EventAttachment {
                    name: text("name").unwrap_or(DEFAULT_ATTACHMENT_NAME).to_owned(),
                    content_type: text("contentType").unwrap_or(OCTET_STREAM).to_owned(),
                    bytes,
                })
            })
            .collect()
    }

    fn link_all(
        attachments: &[EventAttachment],
        store: Store<'_>,
        card: &mut Value,
    ) -> rusqlite::Result<()> {
        if attachments.is_empty() {
            return Ok(());
        }
        let blobs = store.blobs();
        let mut links = Map::with_capacity(attachments.len());
        for (index, attachment) in (1u32..).zip(attachments) {
            links.insert(
                index.to_string(),
                json!({
                    "@type": "Link",
                    "@blob": blobs.intern(&attachment.bytes)?.get(),
                    "contentType": attachment.content_type,
                    "title": attachment.name,
                    "rel": "enclosure",
                }),
            );
        }
        if let Some(map) = card.as_object_mut() {
            match map.get_mut("links").and_then(Value::as_object_mut) {
                Some(existing) => existing.extend(links),
                None => {
                    map.insert("links".to_owned(), Value::Object(links));
                }
            }
        }
        Ok(())
    }
}

struct FetchedEvent {
    graph_id: String,
    event: Result<Value, GraphError>,
    attachments: Vec<EventAttachment>,
}

struct NewEvent {
    graph_id: String,
    converted: ConvertedEvent,
    attachments: Vec<EventAttachment>,
}

impl GraphCoordinator<'_> {
    pub fn reconcile_events(
        &self,
        calendars: &[GraphFolder],
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        let local = self.known(ObjectType::CalendarEvent)?;
        let windows = ExceptionWindow::around_today(self.exception_window_years);
        let mut server_total: HashSet<String> = HashSet::new();
        let mut any_failure = false;

        for calendar in calendars {
            let url = self
                .endpoints
                .calendar_events_ids(&calendar.graph_id, self.top);
            let stubs = match self.client.collect_all_values(&url, &[PREFER_TIMEZONE_UTC]) {
                Ok(stubs) => stubs,
                Err(e) => {
                    self.logger.warn(format_args!(
                        "calendar {} stub fetch failed: {e}",
                        calendar.graph_id
                    ));
                    counts.failed += 1;
                    any_failure = true;
                    continue;
                }
            };
            let mut wanted = Vec::new();
            let mut occurrences = 0usize;
            let mut series_masters = Vec::new();
            for stub in &stubs {
                let event_type = GraphEvent(stub).event_type();
                if event_type == EventType::Occurrence {
                    occurrences += 1;
                    continue;
                }
                let Some(id) = stub.get("id").and_then(Value::as_str) else {
                    continue;
                };
                if event_type == EventType::SeriesMaster {
                    series_masters.push(id.to_owned());
                }
                if local.contains_key(id) {
                    counts.fetched += 1;
                    server_total.insert(id.to_owned());
                } else if server_total.insert(id.to_owned()) {
                    wanted.push(id.to_owned());
                }
            }
            self.logger.progress(format_args!(
                "Calendar {}: {} listed, {} new, {} skipped",
                calendar.graph_id,
                Counted::new(stubs.len() as u64, "event", "events"),
                Grouped(wanted.len() as u64),
                Counted::new(occurrences as u64, "occurrence", "occurrences"),
            ));

            let mut converted = Vec::with_capacity(wanted.len());
            for fetched in self.fetch_events(wanted) {
                let graph_id = fetched.graph_id;
                match fetched.event {
                    Ok(raw) if GraphEvent(&raw).starts_on_outlook_no_date() => counts.skipped += 1,
                    Ok(raw) => match GraphEvent(&raw).convert(None) {
                        Ok(event) => converted.push(NewEvent {
                            graph_id,
                            converted: event,
                            attachments: fetched.attachments,
                        }),
                        Err(e) => {
                            counts.failed += 1;
                            self.logger.warn(format_args!(
                                "calendar event {graph_id} could not be converted: {e}"
                            ));
                        }
                    },
                    Err(GraphError::Vanished) => counts.skipped += 1,
                    Err(e) => {
                        counts.failed += 1;
                        self.logger
                            .warn(format_args!("calendar event {graph_id} fetch failed: {e}"));
                    }
                }
            }
            self.insert_events(calendar, converted, counts)?;

            if !series_masters.is_empty() {
                self.expand_series(&calendar.graph_id, &series_masters, &windows, counts)?;
            }
        }

        if any_failure {
            self.logger.warn(format_args!(
                "vanished calendar events not removed: one or more calendars failed to enumerate; \
                 a clean re-run will reconcile deletions"
            ));
            return Ok(());
        }
        let vanished = IdDiff::compute(server_total, &local).vanished;
        self.sweep(ObjectType::CalendarEvent, &vanished, counts)
    }

    fn fetch_events(&self, ids: Vec<String>) -> Vec<FetchedEvent> {
        let client = self.client.clone();
        let endpoints = self.endpoints.clone();
        let prefer = [
            PREFER_TIMEZONE_UTC,
            match self.event_body_format {
                EventBodyFormat::Text => PREFER_BODY_TEXT,
                EventBodyFormat::Html => PREFER_BODY_HTML,
            },
        ];
        let want_attachments = self.event_attachments == EventAttachments::Fetch;
        let pool: Pool<String, FetchedEvent> = Pool::new(self.workers, move |id: String| {
            let event = client.get_json_with_prefer(&endpoints.event(&id), &prefer);
            let attachments = match &event {
                Ok(raw)
                    if want_attachments
                        && raw.get("hasAttachments").and_then(Value::as_bool) == Some(true) =>
                {
                    EventAttachment::fetch_all(&client, &endpoints, &id)
                }
                _ => Vec::new(),
            };
            FetchedEvent {
                graph_id: id,
                event,
                attachments,
            }
        });
        let total = ids.len();
        for id in ids {
            pool.submit(id);
        }
        pool.results().iter().take(total).collect()
    }

    fn insert_events(
        &self,
        calendar: &GraphFolder,
        mut events: Vec<NewEvent>,
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        let calendars = [calendar.local_id];
        for chunk in events.chunks_mut(CHUNK_SIZE) {
            let tx = self.store.begin()?;
            let store = tx.store();
            for event in chunk {
                let mut card = mem::take(&mut event.converted.data);
                EventAttachment::link_all(&event.attachments, store, &mut card)?;
                let id = store.calendar_events().insert(&CalendarEventRecord {
                    calendars: &calendars,
                    is_draft: event.converted.is_draft,
                    use_default_alerts: event.converted.use_default_alerts,
                    data_type: EventDataType::Event,
                    data: &card.to_string(),
                })?;
                self.ids_in(store)
                    .insert(ObjectType::CalendarEvent, &event.graph_id, id)?;
                counts.created += 1;
            }
            tx.commit()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_occurrence_keys_drop_the_oid_prefix() {
        let raw = json!({
            "id": "M",
            "start": {"dateTime": "2026-09-21T11:00:00.0000000", "timeZone": "UTC"},
            "end": {"dateTime": "2026-09-21T11:30:00.0000000", "timeZone": "UTC"},
            "recurrence": {
                "pattern": {"type": "daily", "interval": 1},
                "range": {"type": "numbered", "startDate": "2026-09-21",
                          "numberOfOccurrences": 6}
            },
            "cancelledOccurrences": ["OID.AAMkAGI2TGuLAAA=.2026-09-24"]
        });
        let converted = GraphEvent(&raw).convert(None).unwrap();
        let overrides = converted.data["recurrenceOverrides"].as_object().unwrap();
        assert!(
            overrides.contains_key("2026-09-24T11:00:00"),
            "beta cancelledOccurrences are OID.<id>.<date>; the key must be a LocalDateTime, \
             got {:?}",
            overrides.keys().collect::<Vec<_>>()
        );
        assert_eq!(overrides["2026-09-24T11:00:00"]["excluded"], true);
    }
}
