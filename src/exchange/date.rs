/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

const END_OF_DAY: &str = "23:59:59";

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
