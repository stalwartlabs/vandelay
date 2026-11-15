/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use chrono::{Datelike, Duration, NaiveDate, Weekday};
use serde_json::{Map, Value, json};

use crate::exchange::graph::recurrence::{GraphRecurrence, PatternKind};
use crate::exchange::vocab::{DayToken, Ordinal, WeekdayName};

const MAX_STEPS: usize = 200_000;
const DATE_FORMAT: &str = "%Y-%m-%d";
const DAYS_PER_WEEK: i64 = 7;
const MONTHS_PER_YEAR: i64 = 12;

struct Series {
    start: NaiveDate,
    limit: Option<NaiveDate>,
    window_start: NaiveDate,
    window_end: NaiveDate,
    emitted: usize,
    out: Vec<NaiveDate>,
}

impl Series {
    fn push(&mut self, date: NaiveDate) {
        if date < self.start || self.limit.is_some_and(|end| date > end) {
            return;
        }
        self.emitted += 1;
        if date >= self.window_start && date <= self.window_end {
            self.out.push(date);
        }
    }
}

impl GraphRecurrence<'_> {
    pub fn expected_dates(
        self,
        window_start: NaiveDate,
        window_end: NaiveDate,
    ) -> Option<Vec<NaiveDate>> {
        let pattern = self.0.get("pattern")?;
        let range = self.0.get("range")?;
        let start = GraphRecurrence::date_of(range.get("startDate"))?;
        let count = range
            .get("numberOfOccurrences")
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
            .and_then(|n| usize::try_from(n).ok());
        let interval = pattern
            .get("interval")
            .and_then(Value::as_u64)
            .filter(|i| *i > 0)
            .map_or(Some(1), |i| i64::try_from(i).ok())?;
        let kind = pattern
            .get("type")
            .and_then(Value::as_str)
            .and_then(PatternKind::parse)?;
        let days = GraphRecurrence::weekdays(pattern);
        let ordinal = pattern
            .get("index")
            .and_then(Value::as_str)
            .and_then(Ordinal::parse)
            .unwrap_or(Ordinal::First);
        let day_of_month = || {
            pattern
                .get("dayOfMonth")
                .and_then(Value::as_u64)
                .and_then(|d| u32::try_from(d).ok())
        };
        let month = || {
            pattern
                .get("month")
                .and_then(Value::as_u64)
                .and_then(|m| u32::try_from(m).ok())
        };
        let years = i32::try_from(interval).ok()?;

        let mut series = Series {
            start,
            limit: GraphRecurrence::date_of(range.get("endDate")),
            window_start,
            window_end,
            emitted: 0,
            out: Vec::new(),
        };
        let mut cursor = start;
        for _ in 0..MAX_STEPS {
            match kind {
                PatternKind::Daily => {
                    series.push(cursor);
                    cursor = cursor.checked_add_signed(Duration::days(interval))?;
                }
                PatternKind::Weekly => {
                    if days.is_empty() {
                        return None;
                    }
                    let first = GraphRecurrence::first_day_of_week(pattern);
                    let week_start = GraphRecurrence::start_of_week(cursor, first);
                    for day in &days {
                        series.push(
                            week_start
                                + Duration::days(GraphRecurrence::weekday_offset(first, *day)),
                        );
                    }
                    cursor = week_start.checked_add_signed(Duration::weeks(interval))?;
                }
                PatternKind::AbsoluteMonthly => {
                    let dom = day_of_month()?;
                    if let Some(date) =
                        GraphRecurrence::clamped_day(cursor.year(), cursor.month(), dom)
                    {
                        series.push(date);
                    }
                    cursor = GraphRecurrence::add_months(cursor.with_day(1)?, interval)?;
                }
                PatternKind::RelativeMonthly => {
                    if let Some(date) = GraphRecurrence::nth_weekday_of_month(
                        cursor.year(),
                        cursor.month(),
                        &days,
                        ordinal,
                    ) {
                        series.push(date);
                    }
                    cursor = GraphRecurrence::add_months(cursor.with_day(1)?, interval)?;
                }
                PatternKind::AbsoluteYearly => {
                    let month = month()?;
                    let dom = day_of_month()?;
                    if let Some(date) = GraphRecurrence::clamped_day(cursor.year(), month, dom) {
                        series.push(date);
                    }
                    cursor = NaiveDate::from_ymd_opt(cursor.year().checked_add(years)?, 1, 1)?;
                }
                PatternKind::RelativeYearly => {
                    let month = month()?;
                    if let Some(date) =
                        GraphRecurrence::nth_weekday_of_month(cursor.year(), month, &days, ordinal)
                    {
                        series.push(date);
                    }
                    cursor = NaiveDate::from_ymd_opt(cursor.year().checked_add(years)?, 1, 1)?;
                }
            }
            if count.is_some_and(|n| series.emitted >= n)
                || series.limit.is_some_and(|end| cursor > end)
                || cursor > window_end
            {
                break;
            }
        }

        series.out.sort_unstable();
        series.out.dedup();
        Some(series.out)
    }

    fn date_of(value: Option<&Value>) -> Option<NaiveDate> {
        let raw = value?.as_str()?;
        NaiveDate::parse_from_str(raw.split('T').next().unwrap_or(raw), DATE_FORMAT).ok()
    }

    fn weekdays(pattern: &Value) -> Vec<Weekday> {
        pattern
            .get("daysOfWeek")
            .and_then(Value::as_array)
            .map(|days| {
                days.iter()
                    .filter_map(Value::as_str)
                    .filter_map(|d| WeekdayName(d).parse())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn first_day_of_week(pattern: &Value) -> Weekday {
        pattern
            .get("firstDayOfWeek")
            .and_then(Value::as_str)
            .and_then(|d| WeekdayName(d).parse())
            .unwrap_or(Weekday::Sun)
    }

    fn weekday_offset(first: Weekday, day: Weekday) -> i64 {
        let from = i64::from(first.num_days_from_sunday());
        let to = i64::from(day.num_days_from_sunday());
        (to - from).rem_euclid(DAYS_PER_WEEK)
    }

    fn start_of_week(date: NaiveDate, first: Weekday) -> NaiveDate {
        date - Duration::days(GraphRecurrence::weekday_offset(first, date.weekday()))
    }

    fn clamped_day(year: i32, month: u32, day: u32) -> Option<NaiveDate> {
        if !(1..=12).contains(&month) {
            return None;
        }
        let last = GraphRecurrence::last_day_of_month(year, month)?;
        NaiveDate::from_ymd_opt(year, month, day.min(last))
    }

    fn last_day_of_month(year: i32, month: u32) -> Option<u32> {
        let first_next = if month == 12 {
            NaiveDate::from_ymd_opt(year.checked_add(1)?, 1, 1)?
        } else {
            NaiveDate::from_ymd_opt(year, month + 1, 1)?
        };
        first_next.pred_opt().map(|d| d.day())
    }

    fn add_months(date: NaiveDate, months: i64) -> Option<NaiveDate> {
        let total = i64::from(date.year()) * MONTHS_PER_YEAR + i64::from(date.month0()) + months;
        let year = i32::try_from(total.div_euclid(MONTHS_PER_YEAR)).ok()?;
        let month = u32::try_from(total.rem_euclid(MONTHS_PER_YEAR) + 1).ok()?;
        NaiveDate::from_ymd_opt(year, month, 1)
    }

    fn nth_weekday_of_month(
        year: i32,
        month: u32,
        days: &[Weekday],
        ordinal: Ordinal,
    ) -> Option<NaiveDate> {
        if days.is_empty() || !(1..=12).contains(&month) {
            return None;
        }
        let last = GraphRecurrence::last_day_of_month(year, month)?;
        let mut matches: Vec<NaiveDate> = Vec::new();
        for day in 1..=last {
            let date = NaiveDate::from_ymd_opt(year, month, day)?;
            if days.contains(&date.weekday()) {
                matches.push(date);
            }
        }
        ordinal.pick(&matches)
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
        GraphRecurrence(&recurrence)
            .expected_dates(day(from), day(to))
            .expect("pattern is supported")
            .into_iter()
            .map(|d| d.to_string())
            .collect()
    }

    // An interval too large for a week offset yields no dates instead of panicking.
    #[test]
    fn huge_weekly_interval_is_rejected_without_panicking() {
        let recurrence = json!({
            "pattern": {"type": "weekly", "interval": 20000000000u64, "daysOfWeek": ["monday"]},
            "range": {"type": "noEnd", "startDate": "2024-01-01"}
        });
        assert_eq!(
            GraphRecurrence(&recurrence).expected_dates(day("2024-01-01"), day("2026-01-01")),
            None
        );
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
            GraphRecurrence(&json!({
                "pattern": {"type": "somethingNew", "interval": 1},
                "range": {"type": "noEnd", "startDate": "2026-01-01"}
            }))
            .expected_dates(day("2026-01-01"), day("2027-01-01"))
            .is_none()
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Frequency {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

#[derive(Debug, Clone, Copy)]
pub struct JsRule<'a>(pub &'a Value);

impl JsRule<'_> {
    pub fn to_graph_pattern(self, series_start: NaiveDate) -> Option<Value> {
        let rule = self.0;
        let frequency = rule.get("frequency").and_then(Value::as_str)?;
        let interval = rule.get("interval").and_then(Value::as_u64).unwrap_or(1);
        let by_day: Vec<&'static str> = rule
            .get("byDay")
            .and_then(Value::as_array)
            .map(|days| {
                days.iter()
                    .filter_map(|d| d.get("day").and_then(Value::as_str))
                    .filter_map(DayToken::parse)
                    .map(|d| WeekdayName::long(d.0))
                    .collect()
            })
            .unwrap_or_default();
        let first_of = |key: &str| {
            rule.get(key)
                .and_then(Value::as_array)
                .and_then(|a| a.first())
        };
        let by_month_day = first_of("byMonthDay").and_then(Value::as_i64);
        let by_month = first_of("byMonth").and_then(|m| {
            m.as_str()
                .and_then(|s| s.parse::<i64>().ok())
                .or_else(|| m.as_i64())
        });
        let set_position = first_of("bySetPosition").and_then(Value::as_i64);
        let start_day = i64::from(series_start.day());
        let start_month = i64::from(series_start.month());
        let relative = match set_position {
            Some(position) if !by_day.is_empty() => Some(Ordinal::from_position(position)),
            _ => None,
        };

        let mut pattern = Map::new();
        pattern.insert("interval".to_owned(), Value::from(interval));
        let frequency = hashify::map!(
            frequency.as_bytes(),
            Frequency,
            "daily" => Frequency::Daily,
            "weekly" => Frequency::Weekly,
            "monthly" => Frequency::Monthly,
            "yearly" => Frequency::Yearly,
        )
        .copied()?;
        let kind = match (frequency, relative) {
            (Frequency::Daily, _) => PatternKind::Daily,
            (Frequency::Weekly, _) => {
                pattern.insert("daysOfWeek".to_owned(), json!(by_day));
                PatternKind::Weekly
            }
            (Frequency::Monthly, Some(ordinal)) => {
                pattern.insert("daysOfWeek".to_owned(), json!(by_day));
                pattern.insert("index".to_owned(), Value::from(ordinal.name()));
                PatternKind::RelativeMonthly
            }
            (Frequency::Monthly, None) => {
                pattern.insert(
                    "dayOfMonth".to_owned(),
                    Value::from(by_month_day.unwrap_or(start_day)),
                );
                PatternKind::AbsoluteMonthly
            }
            (Frequency::Yearly, Some(ordinal)) => {
                pattern.insert("daysOfWeek".to_owned(), json!(by_day));
                pattern.insert("index".to_owned(), Value::from(ordinal.name()));
                pattern.insert(
                    "month".to_owned(),
                    Value::from(by_month.unwrap_or(start_month)),
                );
                PatternKind::RelativeYearly
            }
            (Frequency::Yearly, None) => {
                pattern.insert(
                    "month".to_owned(),
                    Value::from(by_month.unwrap_or(start_month)),
                );
                pattern.insert(
                    "dayOfMonth".to_owned(),
                    Value::from(by_month_day.unwrap_or(start_day)),
                );
                PatternKind::AbsoluteYearly
            }
        };
        pattern.insert("type".to_owned(), Value::from(kind.as_str()));
        if let Some(first) = rule
            .get("firstDayOfWeek")
            .and_then(Value::as_str)
            .and_then(DayToken::parse)
        {
            pattern.insert(
                "firstDayOfWeek".to_owned(),
                Value::from(WeekdayName::long(first.0)),
            );
        }

        let mut range = Map::new();
        range.insert(
            "startDate".to_owned(),
            Value::from(series_start.format(DATE_FORMAT).to_string()),
        );
        if let Some(count) = rule.get("count").and_then(Value::as_u64) {
            range.insert("type".to_owned(), Value::from("numbered"));
            range.insert("numberOfOccurrences".to_owned(), Value::from(count));
        } else if let Some(until) = rule.get("until").and_then(Value::as_str) {
            range.insert("type".to_owned(), Value::from("endDate"));
            range.insert(
                "endDate".to_owned(),
                Value::from(until.split('T').next().unwrap_or(until)),
            );
        } else {
            range.insert("type".to_owned(), Value::from("noEnd"));
        }

        Some(json!({"pattern": Value::Object(pattern), "range": Value::Object(range)}))
    }
}

#[cfg(test)]
mod roundtrip_tests {
    use super::*;
    use serde_json::json;

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn roundtrip(graph: Value, start: &str, from: &str, to: &str) {
        let direct = GraphRecurrence(&graph)
            .expected_dates(day(from), day(to))
            .expect("graph pattern expands");
        let rule = GraphRecurrence(&graph)
            .to_rule()
            .expect("converts to jscalendar");
        let back = JsRule(&rule)
            .to_graph_pattern(day(start))
            .expect("jscalendar rule maps back to a graph pattern");
        let viarule = GraphRecurrence(&back)
            .expected_dates(day(from), day(to))
            .expect("mapped pattern expands");
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
