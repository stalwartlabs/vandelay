/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use rusqlite::{OptionalExtension, params};
use serde_json::map::Entry;
use serde_json::{Map, Value, json};

use crate::console::Counted;
use crate::error::Error;
use crate::exchange::date::WireDate;
use crate::exchange::graph::api::{EXCEPTION_WINDOW_MAX_DAYS, PREFER_TIMEZONE_UTC};
use crate::exchange::graph::calendar_map::{ConvertedEvent, GraphEvent};
use crate::exchange::graph::expand::JsRule;
use crate::exchange::graph::recurrence::GraphRecurrence;
use crate::exchange::jscalendar::OverridePatch;
use crate::exchange::tz::ZoneId;
use crate::sync::TypeCounts;
use crate::types::ObjectType;

use super::coordinator::GraphCoordinator;

const DAYS_PER_YEAR: i64 = 365;
const DATE_FORMAT: &str = "%Y-%m-%d";
const WINDOW_BOUND_FORMAT: &str = "%Y-%m-%dT00:00:00";
const LOCAL_FORMAT: &str = "%Y-%m-%dT%H:%M:%S";
const LOCAL_FRACTION_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.f";
const OVERRIDES: &str = "recurrenceOverrides";

#[derive(Debug, Clone, Copy)]
pub struct ExceptionWindow {
    from: NaiveDate,
    to: NaiveDate,
}

impl ExceptionWindow {
    pub fn around_today(years: i32) -> [ExceptionWindow; 2] {
        let today = Utc::now().date_naive();
        let span = Duration::days(
            i64::from(years.max(1))
                .saturating_mul(DAYS_PER_YEAR)
                .min(EXCEPTION_WINDOW_MAX_DAYS),
        );
        [
            ExceptionWindow {
                from: today - span,
                to: today,
            },
            ExceptionWindow {
                from: today,
                to: today + span,
            },
        ]
    }

    fn bounds(self) -> (String, String) {
        (
            self.from.format(WINDOW_BOUND_FORMAT).to_string(),
            self.to.format(WINDOW_BOUND_FORMAT).to_string(),
        )
    }
}

struct Occurrence {
    start: String,
    zone: Option<String>,
}

pub struct OverrideKey;

impl OverrideKey {
    pub fn normalise(raw: &str, master_tz: Option<&str>) -> String {
        if let Some(tz_name) = master_tz
            && let Some(local) = OverrideKey::to_local(raw, tz_name)
        {
            return local;
        }
        OverrideKey::strip_offset_and_fraction(raw)
    }

    fn to_local(raw: &str, tz_name: &str) -> Option<String> {
        let tz: Tz = tz_name.parse().ok()?;
        let local = match DateTime::parse_from_rfc3339(raw) {
            Ok(parsed) => tz.from_utc_datetime(&parsed.naive_utc()).naive_local(),
            Err(_) => NaiveDateTime::parse_from_str(raw, LOCAL_FRACTION_FORMAT)
                .or_else(|_| NaiveDateTime::parse_from_str(raw, LOCAL_FORMAT))
                .ok()?,
        };
        Some(local.format(LOCAL_FORMAT).to_string())
    }

    fn strip_offset_and_fraction(raw: &str) -> String {
        let no_offset = if let Some((head, _)) = raw.split_once('Z') {
            head
        } else if let Some((head, _)) = raw.split_once('+') {
            head
        } else if let Some((head, _)) = raw.rsplit_once('-')
            && head.len() > 10
        {
            head
        } else {
            raw
        };
        let trimmed = no_offset
            .split_once('.')
            .map_or(no_offset, |(head, _)| head);
        if trimmed.contains('T') {
            trimmed.to_owned()
        } else {
            format!("{trimmed}T00:00:00")
        }
    }
}

struct MasterCard {
    data: Value,
    changed: bool,
}

impl MasterCard {
    fn new(data: Value) -> MasterCard {
        MasterCard {
            data,
            changed: false,
        }
    }

    fn key(&self, utc: &str, zone: Option<&str>) -> String {
        if self.data.get("showWithoutTime").and_then(Value::as_bool) == Some(true)
            && let Some(date) = WireDate(utc).all_day(zone)
        {
            return GraphEvent::all_day_key(date);
        }
        OverrideKey::normalise(utc, self.data.get("timeZone").and_then(Value::as_str))
    }

    fn local_date(&self, utc: &str, zone: Option<&str>) -> String {
        let mut key = self.key(utc, zone);
        if let Some((date, _)) = key.split_once('T') {
            let len = date.len();
            key.truncate(len);
        }
        key
    }

    fn overrides(&mut self) -> Option<&mut Map<String, Value>> {
        self.data
            .as_object_mut()?
            .entry(OVERRIDES)
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
    }

    fn set_override(&mut self, key: String, patch: Value) {
        let Some(overrides) = self.overrides() else {
            return;
        };
        let changed = match overrides.entry(key) {
            Entry::Vacant(slot) => {
                slot.insert(patch);
                true
            }
            Entry::Occupied(mut slot) if *slot.get() != patch => {
                slot.insert(patch);
                true
            }
            Entry::Occupied(_) => false,
        };
        self.changed |= changed;
    }

    fn exclude(&mut self, keys: Vec<String>) {
        if keys.is_empty() {
            return;
        }
        let Some(overrides) = self.overrides() else {
            return;
        };
        let mut changed = false;
        for key in keys {
            if let Entry::Vacant(slot) = overrides.entry(key) {
                slot.insert(json!({"excluded": true}));
                changed = true;
            }
        }
        self.changed |= changed;
    }

    fn cancelled_keys(
        &self,
        occurrences: &[Occurrence],
        exceptions: &[&ConvertedEvent],
        windows: &[ExceptionWindow],
    ) -> Vec<String> {
        let Some(rule) = self.data.get("recurrenceRule") else {
            return Vec::new();
        };
        let Some(start) = self.data.get("start").and_then(Value::as_str) else {
            return Vec::new();
        };
        let (start_date, start_time) = start.split_once('T').unwrap_or((start, "00:00:00"));
        let Ok(series_start) = NaiveDate::parse_from_str(start_date, DATE_FORMAT) else {
            return Vec::new();
        };
        let Some(pattern) = JsRule(rule).to_graph_pattern(series_start) else {
            return Vec::new();
        };
        let mut actual: HashSet<String> =
            HashSet::with_capacity(occurrences.len() + exceptions.len());
        for occurrence in occurrences {
            actual.insert(self.local_date(&occurrence.start, occurrence.zone.as_deref()));
        }
        for exception in exceptions {
            if let Some(raw) = exception.original_start.as_deref() {
                actual.insert(self.local_date(raw, exception.zone.as_deref()));
            }
        }

        let mut out = Vec::new();
        let mut key = String::new();
        for window in windows {
            let Some(expected) = GraphRecurrence(&pattern).expected_dates(window.from, window.to)
            else {
                return Vec::new();
            };
            for date in expected {
                key.clear();
                let _ = write!(key, "{}", date.format(DATE_FORMAT));
                if !actual.contains(&key) {
                    out.push(format!("{key}T{start_time}"));
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

impl GraphCoordinator<'_> {
    pub fn expand_series(
        &self,
        calendar_id: &str,
        series_masters: &[String],
        windows: &[ExceptionWindow],
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        let exceptions = self.fetch_exceptions(calendar_id, windows, counts);
        let occurrences = self.fetch_occurrences(calendar_id, windows, counts);
        let mut by_master: HashMap<&str, Vec<&ConvertedEvent>> = HashMap::new();
        for exception in &exceptions {
            if let Some(master) = exception.series_master_id.as_deref() {
                by_master.entry(master).or_default().push(exception);
            }
        }

        let conn = self.store.connection();
        let known = self.known(ObjectType::CalendarEvent)?;
        for master_id in series_masters {
            let Some(&local_id) = known.get(master_id) else {
                continue;
            };
            let Ok(Some(stored)) = conn
                .query_row(
                    "SELECT data FROM calendar_events WHERE id = ?1",
                    params![local_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
            else {
                continue;
            };
            let Ok(data) = serde_json::from_str::<Value>(&stored) else {
                continue;
            };
            let mut card = MasterCard::new(data);
            let master_exceptions = by_master
                .get(master_id.as_str())
                .map_or(&[][..], Vec::as_slice);
            for exception in master_exceptions {
                if let Some(raw) = exception.original_start.as_deref() {
                    let key = card.key(raw, exception.zone.as_deref());
                    card.set_override(key, OverridePatch::from_event(&exception.data));
                }
            }
            let master_occurrences = occurrences
                .get(master_id.as_str())
                .map_or(&[][..], Vec::as_slice);
            let cancelled = card.cancelled_keys(master_occurrences, master_exceptions, windows);
            card.exclude(cancelled);

            if card.changed {
                conn.execute(
                    "UPDATE calendar_events SET data = ?1 WHERE id = ?2",
                    params![card.data.to_string(), local_id],
                )?;
                counts.updated += 1;
            }
        }
        Ok(())
    }

    fn fetch_exceptions(
        &self,
        calendar_id: &str,
        windows: &[ExceptionWindow],
        counts: &mut TypeCounts,
    ) -> Vec<ConvertedEvent> {
        let mut out = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for window in windows {
            let (from, to) = window.bounds();
            let url = self
                .endpoints
                .calendar_exceptions(calendar_id, &from, &to, self.top);
            let values = match self.client.collect_all_values(&url, &[PREFER_TIMEZONE_UTC]) {
                Ok(values) => values,
                Err(e) => {
                    counts.failed += 1;
                    self.logger.warn(format_args!(
                        "calendar {calendar_id}: exception lookup {from}..{to} failed: {e}"
                    ));
                    continue;
                }
            };
            self.logger.progress(format_args!(
                "Calendar {calendar_id}: {} in {from}..{to}",
                Counted::new(values.len() as u64, "exception", "exceptions")
            ));
            for raw in values {
                let Some(exception_id) = raw.get("id").and_then(Value::as_str) else {
                    continue;
                };
                if !seen.insert(exception_id.to_owned()) {
                    continue;
                }
                match GraphEvent(&raw).convert(None) {
                    Ok(converted) => out.push(converted),
                    Err(e) => {
                        counts.failed += 1;
                        self.logger.warn(format_args!(
                            "exception {exception_id} could not be converted: {e}"
                        ));
                    }
                }
            }
        }
        out
    }

    fn fetch_occurrences(
        &self,
        calendar_id: &str,
        windows: &[ExceptionWindow],
        counts: &mut TypeCounts,
    ) -> HashMap<String, Vec<Occurrence>> {
        let mut out: HashMap<String, Vec<Occurrence>> = HashMap::new();
        for window in windows {
            let (from, to) = window.bounds();
            let url = self
                .endpoints
                .calendar_occurrences(calendar_id, &from, &to, self.top);
            let values = match self.client.collect_all_values(&url, &[PREFER_TIMEZONE_UTC]) {
                Ok(values) => values,
                Err(e) => {
                    counts.failed += 1;
                    self.logger.warn(format_args!(
                        "calendar {calendar_id}: occurrence sweep {from}..{to} failed: {e}; \
                         deleted occurrences cannot be detected for this calendar"
                    ));
                    continue;
                }
            };
            for raw in values {
                let Some(master) = raw.get("seriesMasterId").and_then(Value::as_str) else {
                    continue;
                };
                let Some(start) = raw
                    .get("start")
                    .and_then(|s| s.get("dateTime"))
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                let zone = raw
                    .get("originalStartTimeZone")
                    .and_then(Value::as_str)
                    .and_then(|z| ZoneId(z).iana());
                out.entry(master.to_owned()).or_default().push(Occurrence {
                    start: start.to_owned(),
                    zone,
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_day_daily_master() -> MasterCard {
        MasterCard::new(json!({
            "@type": "Event",
            "start": "2026-11-12T00:00:00",
            "duration": "P1D",
            "showWithoutTime": true,
            "recurrenceRule": {"@type": "RecurrenceRule", "frequency": "daily", "count": 3}
        }))
    }

    fn date(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, DATE_FORMAT).unwrap()
    }

    #[test]
    fn all_day_override_key_takes_the_date_in_the_event_zone() {
        let master = all_day_daily_master();
        let paris = Some("Europe/Paris");
        assert_eq!(
            master.key("2026-11-12T23:00:00Z", paris),
            "2026-11-13T00:00:00"
        );
        assert_eq!(
            master.key("2026-11-13T00:00:00Z", paris),
            "2026-11-13T00:00:00"
        );
        assert_eq!(
            master.local_date("2026-11-13T23:00:00.0000000", paris),
            "2026-11-14"
        );
    }

    #[test]
    fn all_day_occurrences_east_of_utc_are_not_reported_cancelled() {
        let master = all_day_daily_master();
        let occurrences: Vec<Occurrence> =
            ["2026-11-11T23:00:00.0000000", "2026-11-13T23:00:00.0000000"]
                .into_iter()
                .map(|start| Occurrence {
                    start: start.to_owned(),
                    zone: Some("Europe/Paris".to_owned()),
                })
                .collect();
        let windows = [ExceptionWindow {
            from: date("2026-11-01"),
            to: date("2026-11-30"),
        }];
        let cancelled = master.cancelled_keys(&occurrences, &[], &windows);
        assert_eq!(cancelled, vec!["2026-11-13T00:00:00".to_owned()]);
    }

    #[test]
    fn exception_windows_never_exceed_the_graph_limit() {
        for requested in [1, 5, 50] {
            for window in ExceptionWindow::around_today(requested) {
                let days = (window.to - window.from).num_days();
                assert!(days > 0, "window runs forwards: {window:?}");
                assert!(
                    days <= EXCEPTION_WINDOW_MAX_DAYS,
                    "requested {requested}y produced {days} days, over Graph's cap"
                );
                let (from, to) = window.bounds();
                assert!(from.ends_with("T00:00:00") && to.ends_with("T00:00:00"));
            }
        }
    }

    #[test]
    fn overrides_mark_the_card_changed_only_on_a_real_change() {
        let mut card = all_day_daily_master();
        card.exclude(Vec::new());
        assert!(!card.changed);
        assert!(card.data.get(OVERRIDES).is_none());
        card.set_override("2026-11-13T00:00:00".to_owned(), json!({"title": "x"}));
        assert!(card.changed);
        card.changed = false;
        card.set_override("2026-11-13T00:00:00".to_owned(), json!({"title": "x"}));
        card.exclude(vec!["2026-11-13T00:00:00".to_owned()]);
        assert!(!card.changed);
        card.exclude(vec!["2026-11-14T00:00:00".to_owned()]);
        assert!(card.changed);
        assert_eq!(
            card.data[OVERRIDES]["2026-11-14T00:00:00"]["excluded"],
            true
        );
    }

    #[test]
    fn override_key_utc_converts_to_master_local_time() {
        let local = OverrideKey::normalise("2026-05-11T15:00:00Z", Some("America/New_York"));
        assert_eq!(local, "2026-05-11T11:00:00");
    }

    #[test]
    fn override_key_no_tz_strips_z_only() {
        let local = OverrideKey::normalise("2026-05-11T15:00:00Z", None);
        assert_eq!(local, "2026-05-11T15:00:00");
    }

    #[test]
    fn override_key_with_explicit_offset_converts() {
        let local = OverrideKey::normalise("2026-05-11T15:00:00+00:00", Some("Europe/London"));
        assert_eq!(local, "2026-05-11T16:00:00");
    }

    #[test]
    fn override_key_naive_localdatetime_passes_through() {
        let local = OverrideKey::normalise("2026-05-11T15:00:00", Some("America/New_York"));
        assert_eq!(local, "2026-05-11T15:00:00");
    }

    #[test]
    fn override_key_unknown_timezone_strips_offset_only() {
        let local = OverrideKey::normalise("2026-05-11T15:00:00Z", Some("Not/A_Zone"));
        assert_eq!(local, "2026-05-11T15:00:00");
    }

    #[test]
    fn override_key_fractional_seconds_stripped() {
        let local = OverrideKey::normalise("2026-05-11T15:00:00.1234567Z", None);
        assert_eq!(local, "2026-05-11T15:00:00");
    }

    #[test]
    fn override_key_strips_a_negative_offset_but_not_a_date_hyphen() {
        assert_eq!(
            OverrideKey::normalise("2026-05-11T15:00:00-05:00", None),
            "2026-05-11T15:00:00"
        );
        assert_eq!(
            OverrideKey::normalise("2026-05-11", None),
            "2026-05-11T00:00:00"
        );
    }
}
