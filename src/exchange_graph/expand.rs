/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use chrono::{Datelike, Duration, NaiveDate, Weekday};
use serde_json::Value;

const MAX_STEPS: usize = 200_000;

pub fn expected_dates(
    recurrence: &Value,
    window_start: NaiveDate,
    window_end: NaiveDate,
) -> Option<Vec<NaiveDate>> {
    let pattern = recurrence.get("pattern")?;
    let range = recurrence.get("range")?;
    let start = date_of(range.get("startDate"))?;
    let limit = date_of(range.get("endDate"));
    let count = range
        .get("numberOfOccurrences")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .map(|n| n as usize);
    let interval = pattern
        .get("interval")
        .and_then(Value::as_u64)
        .filter(|i| *i > 0)
        .unwrap_or(1) as i64;
    let kind = pattern.get("type").and_then(Value::as_str)?;

    let mut out: Vec<NaiveDate> = Vec::new();
    let mut emitted = 0usize;
    let mut steps = 0usize;
    let mut cursor = start;

    let push = |date: NaiveDate, out: &mut Vec<NaiveDate>, emitted: &mut usize| {
        if date < start {
            return;
        }
        if let Some(end) = limit
            && date > end
        {
            return;
        }
        *emitted += 1;
        if date >= window_start && date <= window_end {
            out.push(date);
        }
    };

    loop {
        steps += 1;
        if steps > MAX_STEPS {
            break;
        }
        let done = match kind {
            "daily" => {
                push(cursor, &mut out, &mut emitted);
                cursor = cursor.checked_add_signed(Duration::days(interval))?;
                false
            }
            "weekly" => {
                let days = weekdays(pattern);
                if days.is_empty() {
                    return None;
                }
                let first = first_day_of_week(pattern);
                let week_start = start_of_week(cursor, first);
                for day in &days {
                    let offset = weekday_offset(first, *day);
                    let date = week_start + Duration::days(offset);
                    push(date, &mut out, &mut emitted);
                }
                cursor = week_start.checked_add_signed(Duration::weeks(interval))?;
                false
            }
            "absoluteMonthly" => {
                let dom = pattern.get("dayOfMonth").and_then(Value::as_u64)? as u32;
                if let Some(date) = clamped_day(cursor.year(), cursor.month(), dom) {
                    push(date, &mut out, &mut emitted);
                }
                cursor = add_months(cursor.with_day(1)?, interval)?;
                false
            }
            "relativeMonthly" => {
                if let Some(date) = nth_weekday_of_month(
                    cursor.year(),
                    cursor.month(),
                    &weekdays(pattern),
                    pattern
                        .get("index")
                        .and_then(Value::as_str)
                        .unwrap_or("first"),
                ) {
                    push(date, &mut out, &mut emitted);
                }
                cursor = add_months(cursor.with_day(1)?, interval)?;
                false
            }
            "absoluteYearly" => {
                let month = pattern.get("month").and_then(Value::as_u64)? as u32;
                let dom = pattern.get("dayOfMonth").and_then(Value::as_u64)? as u32;
                if let Some(date) = clamped_day(cursor.year(), month, dom) {
                    push(date, &mut out, &mut emitted);
                }
                cursor = NaiveDate::from_ymd_opt(cursor.year() + interval as i32, 1, 1)?;
                false
            }
            "relativeYearly" => {
                let month = pattern.get("month").and_then(Value::as_u64)? as u32;
                if let Some(date) = nth_weekday_of_month(
                    cursor.year(),
                    month,
                    &weekdays(pattern),
                    pattern
                        .get("index")
                        .and_then(Value::as_str)
                        .unwrap_or("first"),
                ) {
                    push(date, &mut out, &mut emitted);
                }
                cursor = NaiveDate::from_ymd_opt(cursor.year() + interval as i32, 1, 1)?;
                false
            }
            _ => true,
        };
        if done {
            return None;
        }
        if let Some(n) = count
            && emitted >= n
        {
            break;
        }
        if let Some(end) = limit
            && cursor > end
        {
            break;
        }
        if cursor > window_end {
            break;
        }
    }

    out.sort_unstable();
    out.dedup();
    Some(out)
}

fn date_of(value: Option<&Value>) -> Option<NaiveDate> {
    let raw = value?.as_str()?;
    NaiveDate::parse_from_str(raw.split('T').next().unwrap_or(raw), "%Y-%m-%d").ok()
}

fn weekdays(pattern: &Value) -> Vec<Weekday> {
    pattern
        .get("daysOfWeek")
        .and_then(Value::as_array)
        .map(|days| {
            days.iter()
                .filter_map(Value::as_str)
                .filter_map(parse_weekday)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_weekday(name: &str) -> Option<Weekday> {
    match name.to_ascii_lowercase().as_str() {
        "monday" => Some(Weekday::Mon),
        "tuesday" => Some(Weekday::Tue),
        "wednesday" => Some(Weekday::Wed),
        "thursday" => Some(Weekday::Thu),
        "friday" => Some(Weekday::Fri),
        "saturday" => Some(Weekday::Sat),
        "sunday" => Some(Weekday::Sun),
        _ => None,
    }
}

fn first_day_of_week(pattern: &Value) -> Weekday {
    pattern
        .get("firstDayOfWeek")
        .and_then(Value::as_str)
        .and_then(parse_weekday)
        .unwrap_or(Weekday::Sun)
}

fn weekday_offset(first: Weekday, day: Weekday) -> i64 {
    let from = first.num_days_from_sunday() as i64;
    let to = day.num_days_from_sunday() as i64;
    (to - from).rem_euclid(7)
}

fn start_of_week(date: NaiveDate, first: Weekday) -> NaiveDate {
    let offset = weekday_offset(first, date.weekday());
    date - Duration::days(offset)
}

fn clamped_day(year: i32, month: u32, day: u32) -> Option<NaiveDate> {
    if !(1..=12).contains(&month) {
        return None;
    }
    let last = last_day_of_month(year, month)?;
    NaiveDate::from_ymd_opt(year, month, day.min(last))
}

fn last_day_of_month(year: i32, month: u32) -> Option<u32> {
    let first_next = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)?
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)?
    };
    Some((first_next - Duration::days(1)).day())
}

fn add_months(date: NaiveDate, months: i64) -> Option<NaiveDate> {
    let total = date.year() as i64 * 12 + (date.month() as i64 - 1) + months;
    let year = (total.div_euclid(12)) as i32;
    let month = (total.rem_euclid(12) + 1) as u32;
    NaiveDate::from_ymd_opt(year, month, 1)
}

fn nth_weekday_of_month(year: i32, month: u32, days: &[Weekday], index: &str) -> Option<NaiveDate> {
    if days.is_empty() || !(1..=12).contains(&month) {
        return None;
    }
    let last = last_day_of_month(year, month)?;
    let mut matches: Vec<NaiveDate> = Vec::new();
    for day in 1..=last {
        let date = NaiveDate::from_ymd_opt(year, month, day)?;
        if days.contains(&date.weekday()) {
            matches.push(date);
        }
    }
    if matches.is_empty() {
        return None;
    }
    match index.to_ascii_lowercase().as_str() {
        "first" => matches.first().copied(),
        "second" => matches.get(1).copied(),
        "third" => matches.get(2).copied(),
        "fourth" => matches.get(3).copied(),
        "last" => matches.last().copied(),
        _ => matches.first().copied(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn expand(recurrence: Value, from: &str, to: &str) -> Vec<String> {
        expected_dates(&recurrence, day(from), day(to))
            .expect("pattern is supported")
            .into_iter()
            .map(|d| d.to_string())
            .collect()
    }

    #[test]
    fn daily_numbered_stops_at_the_count() {
        let dates = expand(
            json!({
                "pattern": {"type": "daily", "interval": 1},
                "range": {"type": "numbered", "startDate": "2026-09-21",
                          "numberOfOccurrences": 6}
            }),
            "2026-01-01",
            "2027-01-01",
        );
        assert_eq!(
            dates,
            [
                "2026-09-21",
                "2026-09-22",
                "2026-09-23",
                "2026-09-24",
                "2026-09-25",
                "2026-09-26"
            ]
        );
    }

    #[test]
    fn weekly_interval_two_keeps_all_three_weekdays() {
        let dates = expand(
            json!({
                "pattern": {"type": "weekly", "interval": 2,
                            "daysOfWeek": ["monday", "wednesday", "friday"],
                            "firstDayOfWeek": "monday"},
                "range": {"type": "endDate", "startDate": "2026-09-16",
                          "endDate": "2026-10-03"}
            }),
            "2026-01-01",
            "2027-01-01",
        );
        assert_eq!(
            dates,
            [
                "2026-09-16",
                "2026-09-18",
                "2026-09-28",
                "2026-09-30",
                "2026-10-02"
            ],
            "interval 2 skips the week of the 21st entirely"
        );
    }

    #[test]
    fn relative_monthly_third_thursday() {
        let dates = expand(
            json!({
                "pattern": {"type": "relativeMonthly", "interval": 1,
                            "daysOfWeek": ["thursday"], "index": "third"},
                "range": {"type": "numbered", "startDate": "2026-09-17",
                          "numberOfOccurrences": 3}
            }),
            "2026-01-01",
            "2027-06-01",
        );
        assert_eq!(dates, ["2026-09-17", "2026-10-15", "2026-11-19"]);
    }

    #[test]
    fn relative_monthly_last_weekday() {
        let dates = expand(
            json!({
                "pattern": {"type": "relativeMonthly", "interval": 1,
                            "daysOfWeek": ["friday"], "index": "last"},
                "range": {"type": "numbered", "startDate": "2026-01-30",
                          "numberOfOccurrences": 3}
            }),
            "2026-01-01",
            "2027-01-01",
        );
        assert_eq!(dates, ["2026-01-30", "2026-02-27", "2026-03-27"]);
    }

    #[test]
    fn absolute_monthly_clamps_to_a_short_month() {
        let dates = expand(
            json!({
                "pattern": {"type": "absoluteMonthly", "interval": 1, "dayOfMonth": 31},
                "range": {"type": "numbered", "startDate": "2026-01-31",
                          "numberOfOccurrences": 3}
            }),
            "2026-01-01",
            "2027-01-01",
        );
        assert_eq!(
            dates,
            ["2026-01-31", "2026-02-28", "2026-03-31"],
            "day 31 clamps to the last day of February"
        );
    }

    #[test]
    fn absolute_yearly_keeps_a_leap_day_series_on_the_28th_in_common_years() {
        let dates = expand(
            json!({
                "pattern": {"type": "absoluteYearly", "interval": 1,
                            "month": 2, "dayOfMonth": 29},
                "range": {"type": "numbered", "startDate": "2024-02-29",
                          "numberOfOccurrences": 3}
            }),
            "2020-01-01",
            "2030-01-01",
        );
        assert_eq!(dates, ["2024-02-29", "2025-02-28", "2026-02-28"]);
    }

    #[test]
    fn window_clips_output_but_the_count_still_tracks_the_whole_series() {
        let dates = expand(
            json!({
                "pattern": {"type": "daily", "interval": 1},
                "range": {"type": "numbered", "startDate": "2026-09-21",
                          "numberOfOccurrences": 6}
            }),
            "2026-09-24",
            "2026-09-25",
        );
        assert_eq!(
            dates,
            ["2026-09-24", "2026-09-25"],
            "only in-window dates are returned, but occurrences 1-3 still counted \
             toward the numbered range"
        );
    }

    #[test]
    fn no_end_series_stops_at_the_window() {
        let dates = expand(
            json!({
                "pattern": {"type": "daily", "interval": 1},
                "range": {"type": "noEnd", "startDate": "2026-09-21"}
            }),
            "2026-09-21",
            "2026-09-24",
        );
        assert_eq!(
            dates,
            ["2026-09-21", "2026-09-22", "2026-09-23", "2026-09-24"]
        );
    }

    #[test]
    fn unknown_pattern_is_reported_rather_than_guessed() {
        assert!(
            expected_dates(
                &json!({
                    "pattern": {"type": "somethingNew", "interval": 1},
                    "range": {"type": "noEnd", "startDate": "2026-01-01"}
                }),
                day("2026-01-01"),
                day("2027-01-01"),
            )
            .is_none()
        );
    }
}

pub fn graph_pattern_from_jscalendar(rule: &Value, series_start: NaiveDate) -> Option<Value> {
    use serde_json::json;
    let frequency = rule.get("frequency").and_then(Value::as_str)?;
    let interval = rule.get("interval").and_then(Value::as_u64).unwrap_or(1);
    let by_day: Vec<String> = rule
        .get("byDay")
        .and_then(Value::as_array)
        .map(|days| {
            days.iter()
                .filter_map(|d| d.get("day").and_then(Value::as_str))
                .filter_map(long_weekday)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let by_month_day = rule
        .get("byMonthDay")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_i64);
    let by_month = rule
        .get("byMonth")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|m| {
            m.as_str()
                .and_then(|s| s.parse::<i64>().ok())
                .or_else(|| m.as_i64())
        });
    let set_position = rule
        .get("bySetPosition")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_i64);

    let mut pattern = serde_json::Map::new();
    pattern.insert("interval".to_owned(), Value::from(interval));
    let kind = match frequency {
        "daily" => "daily",
        "weekly" => {
            pattern.insert("daysOfWeek".to_owned(), json!(by_day));
            "weekly"
        }
        "monthly" if set_position.is_some() && !by_day.is_empty() => {
            pattern.insert("daysOfWeek".to_owned(), json!(by_day));
            pattern.insert("index".to_owned(), Value::from(index_name(set_position?)));
            "relativeMonthly"
        }
        "monthly" => {
            pattern.insert(
                "dayOfMonth".to_owned(),
                Value::from(by_month_day.unwrap_or(series_start.day() as i64)),
            );
            "absoluteMonthly"
        }
        "yearly" if set_position.is_some() && !by_day.is_empty() => {
            pattern.insert("daysOfWeek".to_owned(), json!(by_day));
            pattern.insert("index".to_owned(), Value::from(index_name(set_position?)));
            pattern.insert(
                "month".to_owned(),
                Value::from(by_month.unwrap_or(series_start.month() as i64)),
            );
            "relativeYearly"
        }
        "yearly" => {
            pattern.insert(
                "month".to_owned(),
                Value::from(by_month.unwrap_or(series_start.month() as i64)),
            );
            pattern.insert(
                "dayOfMonth".to_owned(),
                Value::from(by_month_day.unwrap_or(series_start.day() as i64)),
            );
            "absoluteYearly"
        }
        _ => return None,
    };
    pattern.insert("type".to_owned(), Value::from(kind));
    if let Some(first) = rule.get("firstDayOfWeek").and_then(Value::as_str)
        && let Some(long) = long_weekday(first)
    {
        pattern.insert("firstDayOfWeek".to_owned(), Value::from(long));
    }

    let mut range = serde_json::Map::new();
    range.insert(
        "startDate".to_owned(),
        Value::from(series_start.format("%Y-%m-%d").to_string()),
    );
    if let Some(count) = rule.get("count").and_then(Value::as_u64) {
        range.insert("type".to_owned(), Value::from("numbered"));
        range.insert("numberOfOccurrences".to_owned(), Value::from(count));
    } else if let Some(until) = rule.get("until").and_then(Value::as_str) {
        range.insert("type".to_owned(), Value::from("endDate"));
        range.insert(
            "endDate".to_owned(),
            Value::from(until.split('T').next().unwrap_or(until).to_owned()),
        );
    } else {
        range.insert("type".to_owned(), Value::from("noEnd"));
    }

    Some(json!({"pattern": Value::Object(pattern), "range": Value::Object(range)}))
}

fn long_weekday(short: &str) -> Option<&'static str> {
    match short.to_ascii_lowercase().as_str() {
        "mo" => Some("monday"),
        "tu" => Some("tuesday"),
        "we" => Some("wednesday"),
        "th" => Some("thursday"),
        "fr" => Some("friday"),
        "sa" => Some("saturday"),
        "su" => Some("sunday"),
        _ => None,
    }
}

fn index_name(set_position: i64) -> &'static str {
    match set_position {
        1 => "first",
        2 => "second",
        3 => "third",
        4 => "fourth",
        -1 => "last",
        _ => "first",
    }
}

#[cfg(test)]
mod roundtrip_tests {
    use super::*;
    use crate::exchange_graph::recurrence::convert_patterned_recurrence;
    use serde_json::json;

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn roundtrip(graph: Value, start: &str, from: &str, to: &str) {
        let direct = expected_dates(&graph, day(from), day(to)).expect("graph pattern expands");
        let rule = convert_patterned_recurrence(&graph).expect("converts to jscalendar");
        let back = graph_pattern_from_jscalendar(&rule, day(start))
            .expect("jscalendar rule maps back to a graph pattern");
        let viarule = expected_dates(&back, day(from), day(to)).expect("mapped pattern expands");
        assert_eq!(
            direct, viarule,
            "a stored rule must expand to the same dates as the Graph pattern it came from"
        );
    }

    #[test]
    fn stored_rules_expand_identically_to_their_graph_patterns() {
        roundtrip(
            json!({"pattern": {"type": "daily", "interval": 2},
                   "range": {"type": "numbered", "startDate": "2026-09-21",
                             "numberOfOccurrences": 5}}),
            "2026-09-21",
            "2026-01-01",
            "2027-01-01",
        );
        roundtrip(
            json!({"pattern": {"type": "weekly", "interval": 2,
                               "daysOfWeek": ["monday", "wednesday", "friday"],
                               "firstDayOfWeek": "monday"},
                   "range": {"type": "endDate", "startDate": "2026-09-16",
                             "endDate": "2026-12-31"}}),
            "2026-09-16",
            "2026-01-01",
            "2027-01-01",
        );
        roundtrip(
            json!({"pattern": {"type": "relativeMonthly", "interval": 1,
                               "daysOfWeek": ["thursday"], "index": "third"},
                   "range": {"type": "numbered", "startDate": "2026-09-17",
                             "numberOfOccurrences": 6}}),
            "2026-09-17",
            "2026-01-01",
            "2028-01-01",
        );
        roundtrip(
            json!({"pattern": {"type": "absoluteMonthly", "interval": 1, "dayOfMonth": 31},
                   "range": {"type": "numbered", "startDate": "2026-01-31",
                             "numberOfOccurrences": 4}}),
            "2026-01-31",
            "2026-01-01",
            "2027-01-01",
        );
        roundtrip(
            json!({"pattern": {"type": "absoluteYearly", "interval": 1,
                               "month": 11, "dayOfMonth": 29},
                   "range": {"type": "noEnd", "startDate": "2026-11-29"}}),
            "2026-11-29",
            "2026-01-01",
            "2031-01-01",
        );
        roundtrip(
            json!({"pattern": {"type": "relativeYearly", "interval": 1,
                               "daysOfWeek": ["monday"], "index": "last", "month": 5},
                   "range": {"type": "noEnd", "startDate": "2026-05-25"}}),
            "2026-05-25",
            "2026-01-01",
            "2031-01-01",
        );
    }
}
