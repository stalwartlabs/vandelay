/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use crate::archive::ids::takeout::TakeoutIds;
use crate::archive::rows::{CalendarEventRecord, CalendarRecord, Subscribed, Write};
use crate::archive::{LocalId, Store};
use crate::error::Error;
use crate::logging::Logger;
use crate::sync::TypeCounts;
use crate::sync::calcard::{CalcardError, ExtractedFields, JsCalendarEntry, SyntheticUid};
use crate::types::ObjectType;

const DEFAULT_CALENDAR: &str = "Imported";

pub struct CalendarFile {
    entries: Vec<JsCalendarEntry>,
    metadata: CalendarMetadata,
}

impl CalendarFile {
    pub fn load(
        path: &Path,
        event_counts: &mut TypeCounts,
        logger: Logger,
    ) -> Option<CalendarFile> {
        let text = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                logger.warn(format_args!("cannot read {}: {e}", path.display()));
                event_counts.failed += 1;
                return None;
            }
        };
        let entries = match JsCalendarEntry::parse_all(&text) {
            Ok(v) => v,
            Err(CalcardError::NoEntries) => {
                logger.warn(format_args!("{} has no events", path.display()));
                return None;
            }
            Err(e) => {
                logger.warn(format_args!("cannot parse {}: {e}", path.display()));
                event_counts.failed += 1;
                return None;
            }
        };
        Some(CalendarFile {
            entries,
            metadata: CalendarMetadata::parse(&text),
        })
    }
}

pub struct CalendarWriter<'a> {
    pub store: Store<'a>,
    pub ids: TakeoutIds<'a>,
    pub calendars: &'a mut HashMap<String, LocalId>,
    pub calendar_counts: &'a mut TypeCounts,
    pub event_counts: &'a mut TypeCounts,
    pub logger: Logger,
}

impl CalendarWriter<'_> {
    pub fn write(&mut self, path: &Path, file: CalendarFile) -> Result<(), Error> {
        let CalendarFile { entries, metadata } = file;
        let calendar = self.ensure_calendar(&metadata)?;
        let mut seed = String::new();
        for (ordinal, mut entry) in entries.into_iter().enumerate() {
            if let Err(e) = self.write_event(calendar, path, ordinal, &mut entry, &mut seed) {
                self.logger.warn(format_args!(
                    "cannot store event {ordinal} of {}: {e}",
                    path.display()
                ));
                self.event_counts.failed += 1;
            }
        }
        Ok(())
    }

    fn ensure_calendar(&mut self, metadata: &CalendarMetadata) -> Result<LocalId, Error> {
        let name = metadata.x_wr_calname.as_deref().unwrap_or(DEFAULT_CALENDAR);
        if let Some(&id) = self.calendars.get(name) {
            return Ok(id);
        }
        if let Some(id) = self.ids.local_id(ObjectType::Calendar, name)? {
            self.calendars.insert(name.to_owned(), id);
            return Ok(id);
        }
        let id = self.store.calendars().insert(&CalendarRecord {
            name,
            description: metadata.x_wr_caldesc.as_deref(),
            color: None,
            sort_order: 0,
            subscribed: Subscribed::Yes,
            is_visible: true,
            is_default: false,
            include_in_availability: "all",
            default_alerts_with_time: None,
            default_alerts_without_time: None,
            time_zone: metadata.x_wr_timezone.as_deref(),
        })?;
        self.ids.insert(ObjectType::Calendar, name, id)?;
        self.calendars.insert(name.to_owned(), id);
        self.calendar_counts.created += 1;
        self.calendar_counts.fetched += 1;
        Ok(id)
    }

    fn write_event(
        &mut self,
        calendar: LocalId,
        path: &Path,
        ordinal: usize,
        entry: &mut JsCalendarEntry,
        seed: &mut String,
    ) -> Result<(), Error> {
        let ExtractedFields {
            is_draft,
            use_default_alerts,
            uid,
        } = entry.take_extracted();
        let uid = match uid.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(u) => u.to_owned(),
            None => {
                seed.clear();
                let _ = write!(seed, "{}#{ordinal}", path.to_string_lossy());
                SyntheticUid::derive(seed)
            }
        };
        let data = entry.data.to_string();
        let events = self.store.calendar_events();

        if let Some(local) = self.ids.local_id(ObjectType::CalendarEvent, &uid)? {
            let calendars = StoredCalendars::merged(self.store, local, calendar)?;
            let record = CalendarEventRecord {
                calendars: &calendars,
                is_draft,
                use_default_alerts,
                data_type: entry.data_type,
                data: &data,
            };
            match events.update(local, &record)? {
                Write::Changed => self.event_counts.fetched += 1,
                Write::Unchanged => self.event_counts.skipped += 1,
            }
            return Ok(());
        }

        let local = events.insert(&CalendarEventRecord {
            calendars: &[calendar],
            is_draft,
            use_default_alerts,
            data_type: entry.data_type,
            data: &data,
        })?;
        self.ids.insert(ObjectType::CalendarEvent, &uid, local)?;
        self.event_counts.created += 1;
        self.event_counts.fetched += 1;
        Ok(())
    }
}

struct StoredCalendars;

impl StoredCalendars {
    fn merged(store: Store<'_>, event: LocalId, calendar: LocalId) -> Result<Vec<LocalId>, Error> {
        let mut stmt = store.connection().prepare_cached(
            "SELECT j.value FROM calendar_events e, json_each(e.calendar_ids) j
             WHERE e.id = ?1 AND json_type(e.calendar_ids) = 'array' AND j.type = 'integer'",
        )?;
        let mut ids = stmt
            .query_map([event], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<LocalId>>>()?;
        ids.push(calendar);
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }
}

#[derive(Debug, Default, Clone)]
struct CalendarMetadata {
    x_wr_calname: Option<String>,
    x_wr_caldesc: Option<String>,
    x_wr_timezone: Option<String>,
}

impl CalendarMetadata {
    fn parse(text: &str) -> CalendarMetadata {
        let mut out = CalendarMetadata::default();
        let mut inside_vevent = false;
        for raw_line in text.lines() {
            let line = raw_line.trim_end_matches('\r');
            if line.starts_with("BEGIN:VEVENT")
                || line.starts_with("BEGIN:VTODO")
                || line.starts_with("BEGIN:VJOURNAL")
            {
                inside_vevent = true;
                continue;
            }
            if line.starts_with("END:VEVENT")
                || line.starts_with("END:VTODO")
                || line.starts_with("END:VJOURNAL")
            {
                inside_vevent = false;
                continue;
            }
            if inside_vevent {
                continue;
            }
            if let Some(rest) = line.strip_prefix("X-WR-CALNAME:") {
                out.x_wr_calname = Some(rest.trim().to_owned());
            } else if let Some(rest) = line.strip_prefix("X-WR-CALDESC:") {
                out.x_wr_caldesc = Some(rest.trim().to_owned());
            } else if let Some(rest) = line.strip_prefix("X-WR-TIMEZONE:") {
                out.x_wr_timezone = Some(rest.trim().to_owned());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_metadata_finds_xwr_fields() {
        let ical = "BEGIN:VCALENDAR\r\n\
            PRODID:-//Google Inc//Google Calendar 70.9054//EN\r\n\
            X-WR-CALNAME:My Calendar\r\n\
            X-WR-CALDESC:Some description\r\n\
            X-WR-TIMEZONE:Europe/Madrid\r\n\
            BEGIN:VEVENT\r\nUID:e1@google.com\r\nDTSTAMP:20250101T000000Z\r\n\
            END:VEVENT\r\nEND:VCALENDAR\r\n";
        let m = CalendarMetadata::parse(ical);
        assert_eq!(m.x_wr_calname.as_deref(), Some("My Calendar"));
        assert_eq!(m.x_wr_caldesc.as_deref(), Some("Some description"));
        assert_eq!(m.x_wr_timezone.as_deref(), Some("Europe/Madrid"));
    }

    #[test]
    fn parse_metadata_ignores_xwr_inside_vevent() {
        let ical = "BEGIN:VCALENDAR\r\n\
            BEGIN:VEVENT\r\nUID:e1\r\nX-WR-CALNAME:NOT THIS\r\nEND:VEVENT\r\n\
            END:VCALENDAR\r\n";
        let m = CalendarMetadata::parse(ical);
        assert!(m.x_wr_calname.is_none());
    }

    #[test]
    fn parse_metadata_empty_when_missing() {
        let ical = "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n";
        let m = CalendarMetadata::parse(ical);
        assert!(m.x_wr_calname.is_none());
        assert!(m.x_wr_caldesc.is_none());
        assert!(m.x_wr_timezone.is_none());
    }

    #[test]
    fn parse_metadata_handles_lf_only_line_endings() {
        let ical = "BEGIN:VCALENDAR\nX-WR-CALNAME:Foo\nEND:VCALENDAR\n";
        let m = CalendarMetadata::parse(ical);
        assert_eq!(m.x_wr_calname.as_deref(), Some("Foo"));
    }
}
