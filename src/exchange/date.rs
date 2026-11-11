/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, TimeZone};
use chrono_tz::Tz;

const END_OF_DAY: &str = "23:59:59";
const HALF_DAY: TimeDelta = TimeDelta::hours(12);
const OUTLOOK_NO_DATE_YEAR: i32 = 4500;

pub fn nearest_midnight(value: &str, iana: Option<&str>) -> Option<NaiveDate> {
    let trimmed = value.trim();
    let local = match DateTime::parse_from_rfc3339(trimmed) {
        Ok(instant) => match iana.and_then(|tz| tz.parse::<Tz>().ok()) {
            Some(tz) => instant.with_timezone(&tz).naive_local(),
            None => instant.naive_local(),
        },
        Err(_) => NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S%.f")
            .or_else(|_| {
                NaiveDate::parse_from_str(trimmed, "%Y-%m-%d").map(|d| d.and_time(NaiveTime::MIN))
            })
            .ok()?,
    };
    local.checked_add_signed(HALF_DAY).map(|dt| dt.date())
}

pub fn all_day_date(utc: &str, iana: Option<&str>) -> Option<NaiveDate> {
    let trimmed = utc.trim();
    let instant = DateTime::parse_from_rfc3339(trimmed)
        .map(|dt| dt.naive_utc())
        .or_else(|_| NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S%.f"))
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

pub fn is_outlook_no_date(value: &str) -> bool {
    value
        .trim()
        .split_once('-')
        .and_then(|(year, _)| year.parse::<i32>().ok())
        .is_some_and(|year| year >= OUTLOOK_NO_DATE_YEAR)
}

pub fn recurrence_until(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let date = trimmed.get(..10).filter(|d| is_calendar_date(d))?;
    let time = match trimmed.get(10..) {
        Some(rest) if rest.starts_with('T') || rest.starts_with('t') => {
            clock_time(&rest[1..]).unwrap_or_else(|| END_OF_DAY.to_owned())
        }
        _ => END_OF_DAY.to_owned(),
    };
    Some(format!("{date}T{time}"))
}

fn is_calendar_date(value: &str) -> bool {
    let b = value.as_bytes();
    b.len() == 10
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit)
}

fn clock_time(rest: &str) -> Option<String> {
    let end = rest
        .find(['Z', 'z', '+'])
        .or_else(|| rest.find('-'))
        .unwrap_or(rest.len());
    let raw = &rest[..end];
    let mut parts = raw.split(':');
    let hour = two_digits(parts.next()?)?;
    let minute = two_digits(parts.next().unwrap_or("00"))?;
    let second = match parts.next() {
        Some(s) => two_digits(s.split('.').next().unwrap_or("00"))?,
        None => "00".to_owned(),
    };
    Some(format!("{hour}:{minute}:{second}"))
}

fn two_digits(value: &str) -> Option<String> {
    let v = value.trim();
    if v.len() == 2 && v.bytes().all(|b| b.is_ascii_digit()) {
        Some(v.to_owned())
    } else {
        None
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
            nearest_midnight("2026-11-11T23:00:00Z", Some("Europe/Paris")),
            date(2026, 11, 12)
        );
        assert_eq!(
            nearest_midnight("2026-07-11T22:00:00Z", Some("Europe/Paris")),
            date(2026, 7, 12)
        );
    }

    #[test]
    fn utc_instant_rounds_to_local_midnight_west_of_utc() {
        assert_eq!(
            nearest_midnight("2026-11-12T05:00:00Z", Some("America/New_York")),
            date(2026, 11, 12)
        );
    }

    #[test]
    fn zone_off_by_an_hour_still_gives_the_same_day() {
        assert_eq!(
            nearest_midnight("2026-11-11T23:00:00Z", Some("Europe/London")),
            date(2026, 11, 12)
        );
        assert_eq!(
            nearest_midnight("2026-11-11T23:00:00Z", Some("Europe/Helsinki")),
            date(2026, 11, 12)
        );
    }

    #[test]
    fn utc_instant_without_zone_rounds_in_utc() {
        assert_eq!(
            nearest_midnight("1990-04-14T22:00:00Z", None),
            date(1990, 4, 15)
        );
        assert_eq!(
            nearest_midnight("1990-04-15T05:00:00Z", None),
            date(1990, 4, 15)
        );
    }

    #[test]
    fn floating_and_bare_dates_are_kept() {
        assert_eq!(
            nearest_midnight("2025-12-25T00:00:00", Some("Europe/Paris")),
            date(2025, 12, 25)
        );
        assert_eq!(nearest_midnight("2025-12-25", None), date(2025, 12, 25));
        assert_eq!(nearest_midnight("garbage", None), None);
    }

    #[test]
    fn all_day_midnight_is_taken_as_the_date() {
        assert_eq!(
            all_day_date("2026-11-12T00:00:00.0000000", Some("Pacific/Auckland")),
            date(2026, 11, 12)
        );
        assert_eq!(
            all_day_date("2026-11-12T00:00:00Z", Some("America/New_York")),
            date(2026, 11, 12)
        );
    }

    #[test]
    fn all_day_utc_instant_moves_to_the_local_date() {
        assert_eq!(
            all_day_date("2026-11-11T23:00:00.0000000", Some("Europe/Paris")),
            date(2026, 11, 12)
        );
        assert_eq!(
            all_day_date("2026-11-11T11:00:00Z", Some("Pacific/Auckland")),
            date(2026, 11, 12)
        );
        assert_eq!(
            all_day_date("2026-11-12T05:00:00.0000000", Some("America/New_York")),
            date(2026, 11, 12)
        );
        assert_eq!(
            all_day_date("2026-11-11T23:00:00", None),
            date(2026, 11, 12)
        );
        assert_eq!(all_day_date("garbage", None), None);
    }

    #[test]
    fn outlook_no_date_sentinel() {
        assert!(is_outlook_no_date("4500-08-31T23:00:00Z"));
        assert!(is_outlook_no_date("4501-01-01T00:00:00Z"));
        assert!(!is_outlook_no_date("2026-11-12T00:00:00Z"));
        assert!(!is_outlook_no_date("garbage"));
    }

    #[test]
    fn bare_date_gets_end_of_day() {
        assert_eq!(
            recurrence_until("2026-08-01").as_deref(),
            Some("2026-08-01T23:59:59")
        );
    }

    #[test]
    fn utc_designator_is_dropped() {
        assert_eq!(
            recurrence_until("2026-08-01Z").as_deref(),
            Some("2026-08-01T23:59:59")
        );
    }

    #[test]
    fn numeric_offset_is_dropped_not_embedded() {
        assert_eq!(
            recurrence_until("2021-09-30-06:00").as_deref(),
            Some("2021-09-30T23:59:59")
        );
        assert_eq!(
            recurrence_until("2021-09-30+02:00").as_deref(),
            Some("2021-09-30T23:59:59")
        );
    }

    #[test]
    fn date_time_keeps_its_clock_time() {
        assert_eq!(
            recurrence_until("2021-09-30T15:15:00").as_deref(),
            Some("2021-09-30T15:15:00")
        );
        assert_eq!(
            recurrence_until("2021-09-30T15:15:00Z").as_deref(),
            Some("2021-09-30T15:15:00")
        );
        assert_eq!(
            recurrence_until("2021-09-30T15:15:00-06:00").as_deref(),
            Some("2021-09-30T15:15:00")
        );
        assert_eq!(
            recurrence_until("2021-09-30T15:15:00.123Z").as_deref(),
            Some("2021-09-30T15:15:00")
        );
    }

    #[test]
    fn short_clock_time_is_padded_to_seconds() {
        assert_eq!(
            recurrence_until("2021-09-30T15:15").as_deref(),
            Some("2021-09-30T15:15:00")
        );
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        assert_eq!(
            recurrence_until("  2026-08-01Z \n").as_deref(),
            Some("2026-08-01T23:59:59")
        );
    }

    #[test]
    fn unusable_input_yields_none() {
        assert_eq!(recurrence_until(""), None);
        assert_eq!(recurrence_until("not-a-date"), None);
        assert_eq!(recurrence_until("2026-8-1"), None);
    }
}
