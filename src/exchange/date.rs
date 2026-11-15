/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt::Write;

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, TimeZone};
use chrono_tz::Tz;
use serde_json::{Map, Value};

const END_OF_DAY: &str = "23:59:59";
const MIDNIGHT_FIELD: &str = "00";
const HALF_DAY: TimeDelta = TimeDelta::hours(12);
const OUTLOOK_NO_DATE_YEAR: i32 = 4500;
const LOCAL_DATE_TIME: &str = "%Y-%m-%dT%H:%M:%S%.f";
const DATE_LEN: usize = 10;

#[derive(Debug, Clone, Copy)]
pub struct WireDate<'a>(pub &'a str);

impl WireDate<'_> {
    pub fn nearest_midnight(self, iana: Option<&str>) -> Option<NaiveDate> {
        let trimmed = self.0.trim();
        let local = match DateTime::parse_from_rfc3339(trimmed) {
            Ok(instant) => match iana.and_then(|tz| tz.parse::<Tz>().ok()) {
                Some(tz) => instant.with_timezone(&tz).naive_local(),
                None => instant.naive_local(),
            },
            Err(_) => NaiveDateTime::parse_from_str(trimmed, LOCAL_DATE_TIME)
                .or_else(|_| {
                    NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
                        .map(|d| d.and_time(NaiveTime::MIN))
                })
                .ok()?,
        };
        local.checked_add_signed(HALF_DAY).map(|dt| dt.date())
    }

    pub fn all_day(self, iana: Option<&str>) -> Option<NaiveDate> {
        let trimmed = self.0.trim();
        let instant = DateTime::parse_from_rfc3339(trimmed)
            .map(|dt| dt.naive_utc())
            .or_else(|_| NaiveDateTime::parse_from_str(trimmed, LOCAL_DATE_TIME))
            .ok()?;
        if instant.time() == NaiveTime::MIN {
            return Some(instant.date());
        }
        let local = match iana.and_then(|tz| tz.parse::<Tz>().ok()) {
            Some(tz) => tz.from_utc_datetime(&instant).naive_local(),
            None => instant,
        };
        local.checked_add_signed(HALF_DAY).map(|dt| dt.date())
    }

    pub fn is_outlook_no_date(self) -> bool {
        self.0
            .trim()
            .split_once('-')
            .and_then(|(year, _)| year.parse::<i32>().ok())
            .is_some_and(|year| year >= OUTLOOK_NO_DATE_YEAR)
    }

    pub fn partial_date(self) -> Option<Value> {
        if self.is_outlook_no_date() {
            return None;
        }
        let date = self.nearest_midnight(None)?;
        let mut o = Map::new();
        o.insert("@type".to_owned(), Value::from("PartialDate"));
        if date.year() > 0 {
            o.insert("year".to_owned(), Value::from(date.year()));
        }
        o.insert("month".to_owned(), Value::from(date.month()));
        o.insert("day".to_owned(), Value::from(date.day()));
        Some(Value::Object(o))
    }

    pub fn without_fraction(self) -> String {
        let raw = self.0;
        let trimmed = raw.trim_end_matches('Z');
        let base = trimmed.split_once('.').map_or(trimmed, |(base, _)| base);
        let mut out = String::with_capacity(base.len() + 1);
        out.push_str(base);
        if raw.ends_with('Z') {
            out.push('Z');
        }
        out
    }

    pub fn recurrence_until(self) -> Option<String> {
        let trimmed = self.0.trim();
        let date = trimmed
            .get(..DATE_LEN)
            .filter(|d| WireDate::is_calendar_date(d))?;
        let mut out = String::with_capacity(DATE_LEN + 1 + END_OF_DAY.len());
        out.push_str(date);
        out.push('T');
        let clock = trimmed
            .get(DATE_LEN..)
            .and_then(|rest| rest.strip_prefix(['T', 't']))
            .and_then(WireDate::clock_time);
        match clock {
            Some((hour, minute, second)) => {
                let _ = write!(out, "{hour}:{minute}:{second}");
            }
            None => out.push_str(END_OF_DAY),
        }
        Some(out)
    }

    fn is_calendar_date(value: &str) -> bool {
        let [y0, y1, y2, y3, b'-', m0, m1, b'-', d0, d1] = value.as_bytes() else {
            return false;
        };
        [y0, y1, y2, y3, m0, m1, d0, d1]
            .into_iter()
            .all(u8::is_ascii_digit)
    }

    fn clock_time(rest: &str) -> Option<(&str, &str, &str)> {
        let raw = rest
            .split_once(['Z', 'z', '+'])
            .or_else(|| rest.split_once('-'))
            .map_or(rest, |(clock, _)| clock);
        let mut parts = raw.split(':');
        let hour = WireDate::two_digits(parts.next()?)?;
        let minute = WireDate::two_digits(parts.next().unwrap_or(MIDNIGHT_FIELD))?;
        let second = match parts.next() {
            Some(s) => WireDate::two_digits(s.split('.').next().unwrap_or(MIDNIGHT_FIELD))?,
            None => MIDNIGHT_FIELD,
        };
        Some((hour, minute, second))
    }

    fn two_digits(value: &str) -> Option<&str> {
        let v = value.trim();
        (v.len() == 2 && v.bytes().all(|b| b.is_ascii_digit())).then_some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> Option<NaiveDate> {
        NaiveDate::from_ymd_opt(y, m, d)
    }

    #[test]
    fn utc_instant_rounds_to_local_midnight_east_of_utc() {
        assert_eq!(
            WireDate("2026-11-11T23:00:00Z").nearest_midnight(Some("Europe/Paris")),
            date(2026, 11, 12)
        );
        assert_eq!(
            WireDate("2026-07-11T22:00:00Z").nearest_midnight(Some("Europe/Paris")),
            date(2026, 7, 12)
        );
    }

    #[test]
    fn utc_instant_rounds_to_local_midnight_west_of_utc() {
        assert_eq!(
            WireDate("2026-11-12T05:00:00Z").nearest_midnight(Some("America/New_York")),
            date(2026, 11, 12)
        );
    }

    #[test]
    fn zone_off_by_an_hour_still_gives_the_same_day() {
        assert_eq!(
            WireDate("2026-11-11T23:00:00Z").nearest_midnight(Some("Europe/London")),
            date(2026, 11, 12)
        );
        assert_eq!(
            WireDate("2026-11-11T23:00:00Z").nearest_midnight(Some("Europe/Helsinki")),
            date(2026, 11, 12)
        );
    }

    #[test]
    fn utc_instant_without_zone_rounds_in_utc() {
        assert_eq!(
            WireDate("1990-04-14T22:00:00Z").nearest_midnight(None),
            date(1990, 4, 15)
        );
        assert_eq!(
            WireDate("1990-04-15T05:00:00Z").nearest_midnight(None),
            date(1990, 4, 15)
        );
    }

    #[test]
    fn floating_and_bare_dates_are_kept() {
        assert_eq!(
            WireDate("2025-12-25T00:00:00").nearest_midnight(Some("Europe/Paris")),
            date(2025, 12, 25)
        );
        assert_eq!(
            WireDate("2025-12-25").nearest_midnight(None),
            date(2025, 12, 25)
        );
        assert_eq!(WireDate("garbage").nearest_midnight(None), None);
    }

    #[test]
    fn all_day_midnight_is_taken_as_the_date() {
        assert_eq!(
            WireDate("2026-11-12T00:00:00.0000000").all_day(Some("Pacific/Auckland")),
            date(2026, 11, 12)
        );
        assert_eq!(
            WireDate("2026-11-12T00:00:00Z").all_day(Some("America/New_York")),
            date(2026, 11, 12)
        );
    }

    #[test]
    fn all_day_utc_instant_moves_to_the_local_date() {
        assert_eq!(
            WireDate("2026-11-11T23:00:00.0000000").all_day(Some("Europe/Paris")),
            date(2026, 11, 12)
        );
        assert_eq!(
            WireDate("2026-11-11T11:00:00Z").all_day(Some("Pacific/Auckland")),
            date(2026, 11, 12)
        );
        assert_eq!(
            WireDate("2026-11-12T05:00:00.0000000").all_day(Some("America/New_York")),
            date(2026, 11, 12)
        );
        assert_eq!(
            WireDate("2026-11-11T23:00:00").all_day(None),
            date(2026, 11, 12)
        );
        assert_eq!(WireDate("garbage").all_day(None), None);
    }

    #[test]
    fn outlook_no_date_sentinel() {
        assert!(WireDate("4500-08-31T23:00:00Z").is_outlook_no_date());
        assert!(WireDate("4501-01-01T00:00:00Z").is_outlook_no_date());
        assert!(!WireDate("2026-11-12T00:00:00Z").is_outlook_no_date());
        assert!(!WireDate("garbage").is_outlook_no_date());
    }

    #[test]
    fn bare_date_gets_end_of_day() {
        assert_eq!(
            WireDate("2026-08-01").recurrence_until().as_deref(),
            Some("2026-08-01T23:59:59")
        );
    }

    #[test]
    fn utc_designator_is_dropped() {
        assert_eq!(
            WireDate("2026-08-01Z").recurrence_until().as_deref(),
            Some("2026-08-01T23:59:59")
        );
    }

    #[test]
    fn numeric_offset_is_dropped_not_embedded() {
        assert_eq!(
            WireDate("2021-09-30-06:00").recurrence_until().as_deref(),
            Some("2021-09-30T23:59:59")
        );
        assert_eq!(
            WireDate("2021-09-30+02:00").recurrence_until().as_deref(),
            Some("2021-09-30T23:59:59")
        );
    }

    #[test]
    fn date_time_keeps_its_clock_time() {
        assert_eq!(
            WireDate("2021-09-30T15:15:00")
                .recurrence_until()
                .as_deref(),
            Some("2021-09-30T15:15:00")
        );
        assert_eq!(
            WireDate("2021-09-30T15:15:00Z")
                .recurrence_until()
                .as_deref(),
            Some("2021-09-30T15:15:00")
        );
        assert_eq!(
            WireDate("2021-09-30T15:15:00-06:00")
                .recurrence_until()
                .as_deref(),
            Some("2021-09-30T15:15:00")
        );
        assert_eq!(
            WireDate("2021-09-30T15:15:00.123Z")
                .recurrence_until()
                .as_deref(),
            Some("2021-09-30T15:15:00")
        );
    }

    #[test]
    fn short_clock_time_is_padded_to_seconds() {
        assert_eq!(
            WireDate("2021-09-30T15:15").recurrence_until().as_deref(),
            Some("2021-09-30T15:15:00")
        );
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        assert_eq!(
            WireDate("  2026-08-01Z \n").recurrence_until().as_deref(),
            Some("2026-08-01T23:59:59")
        );
    }

    #[test]
    fn unusable_input_yields_none() {
        assert_eq!(WireDate("").recurrence_until(), None);
        assert_eq!(WireDate("not-a-date").recurrence_until(), None);
        assert_eq!(WireDate("2026-8-1").recurrence_until(), None);
    }
}
