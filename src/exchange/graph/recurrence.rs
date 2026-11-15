/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use serde_json::{Map, Value, json};

use crate::exchange::date::WireDate;
use crate::exchange::graph::error::GraphError;
use crate::exchange::vocab::{DayToken, Ordinal, WeekdayName};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternKind {
    Daily,
    Weekly,
    AbsoluteMonthly,
    RelativeMonthly,
    AbsoluteYearly,
    RelativeYearly,
}

impl PatternKind {
    pub fn parse(value: &str) -> Option<PatternKind> {
        hashify::map!(
            value.as_bytes(),
            PatternKind,
            "daily" => PatternKind::Daily,
            "weekly" => PatternKind::Weekly,
            "absoluteMonthly" => PatternKind::AbsoluteMonthly,
            "relativeMonthly" => PatternKind::RelativeMonthly,
            "absoluteYearly" => PatternKind::AbsoluteYearly,
            "relativeYearly" => PatternKind::RelativeYearly,
        )
        .copied()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            PatternKind::Daily => "daily",
            PatternKind::Weekly => "weekly",
            PatternKind::AbsoluteMonthly => "absoluteMonthly",
            PatternKind::RelativeMonthly => "relativeMonthly",
            PatternKind::AbsoluteYearly => "absoluteYearly",
            PatternKind::RelativeYearly => "relativeYearly",
        }
    }

    fn frequency(self) -> &'static str {
        match self {
            PatternKind::Daily => "daily",
            PatternKind::Weekly => "weekly",
            PatternKind::AbsoluteMonthly | PatternKind::RelativeMonthly => "monthly",
            PatternKind::AbsoluteYearly | PatternKind::RelativeYearly => "yearly",
        }
    }

    fn is_relative(self) -> bool {
        matches!(
            self,
            PatternKind::RelativeMonthly | PatternKind::RelativeYearly
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeKind {
    EndDate,
    Numbered,
    NoEnd,
}

impl RangeKind {
    pub fn parse(value: &str) -> RangeKind {
        hashify::map!(
            value.as_bytes(),
            RangeKind,
            "endDate" => RangeKind::EndDate,
            "numbered" => RangeKind::Numbered,
        )
        .copied()
        .unwrap_or(RangeKind::NoEnd)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct GraphRecurrence<'a>(pub &'a Value);

impl GraphRecurrence<'_> {
    pub fn to_rule(self) -> Result<Value, GraphError> {
        self.to_rule_map().map(Value::Object)
    }

    pub fn to_rule_map(self) -> Result<Map<String, Value>, GraphError> {
        let pattern = self
            .0
            .get("pattern")
            .ok_or_else(|| GraphError::Malformed("recurrence.pattern missing".to_owned()))?;
        let range = self
            .0
            .get("range")
            .ok_or_else(|| GraphError::Malformed("recurrence.range missing".to_owned()))?;
        let kind = pattern
            .get("type")
            .and_then(Value::as_str)
            .and_then(PatternKind::parse);

        let mut rule = Map::new();
        rule.insert("@type".to_owned(), Value::from("RecurrenceRule"));
        rule.insert(
            "frequency".to_owned(),
            Value::from(kind.map_or("daily", PatternKind::frequency)),
        );

        if let Some(interval) = pattern.get("interval").and_then(Value::as_u64)
            && interval != 1
        {
            rule.insert("interval".to_owned(), Value::from(interval));
        }

        if let Some(day) = GraphRecurrence::weekday(pattern.get("firstDayOfWeek")) {
            rule.insert("firstDayOfWeek".to_owned(), Value::from(day.as_str()));
        }

        if let Some(days) = pattern.get("daysOfWeek").and_then(Value::as_array) {
            let by_day: Vec<Value> = days
                .iter()
                .filter_map(|d| GraphRecurrence::weekday(Some(d)))
                .map(|d| json!({"@type": "NDay", "day": d.as_str()}))
                .collect();
            if !by_day.is_empty() {
                rule.insert("byDay".to_owned(), Value::Array(by_day));
            }
        }

        if let Some(dom) = pattern.get("dayOfMonth").and_then(Value::as_i64)
            && dom != 0
        {
            rule.insert("byMonthDay".to_owned(), json!([dom]));
        }

        if let Some(month) = pattern.get("month").and_then(Value::as_i64)
            && month != 0
        {
            rule.insert("byMonth".to_owned(), json!([month.to_string()]));
        }

        if kind.is_some_and(PatternKind::is_relative)
            && let Some(ordinal) = pattern
                .get("index")
                .and_then(Value::as_str)
                .and_then(Ordinal::parse)
        {
            rule.insert(
                "bySetPosition".to_owned(),
                json!([i64::from(ordinal.position())]),
            );
        }

        let range_kind = range
            .get("type")
            .and_then(Value::as_str)
            .map_or(RangeKind::NoEnd, RangeKind::parse);
        match range_kind {
            RangeKind::EndDate => {
                if let Some(local) = range
                    .get("endDate")
                    .and_then(Value::as_str)
                    .map(WireDate)
                    .filter(|end| !end.is_outlook_no_date())
                    .and_then(WireDate::recurrence_until)
                {
                    rule.insert("until".to_owned(), Value::from(local));
                }
            }
            RangeKind::Numbered => {
                if let Some(count) = range.get("numberOfOccurrences").and_then(Value::as_u64) {
                    rule.insert("count".to_owned(), Value::from(count));
                }
            }
            RangeKind::NoEnd => {}
        }

        Ok(rule)
    }

    fn weekday(name: Option<&Value>) -> Option<DayToken> {
        name.and_then(Value::as_str)
            .and_then(|n| WeekdayName(n).parse())
            .map(DayToken)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn daily_with_interval() {
        let pr = json!({
            "pattern": {"type": "daily", "interval": 3},
            "range": {"type": "noEnd"}
        });
        let out = GraphRecurrence(&pr).to_rule().unwrap();
        let rule = &out;
        assert_eq!(rule["frequency"], "daily");
        assert_eq!(rule["interval"], 3);
        assert!(rule.get("until").is_none());
        assert!(rule.get("count").is_none());
    }

    #[test]
    fn weekly_with_days_of_week() {
        let pr = json!({
            "pattern": {
                "type": "weekly",
                "interval": 1,
                "daysOfWeek": ["monday", "wednesday", "friday"],
                "firstDayOfWeek": "monday"
            },
            "range": {"type": "noEnd"}
        });
        let out = GraphRecurrence(&pr).to_rule().unwrap();
        let rule = &out;
        assert_eq!(rule["frequency"], "weekly");
        assert_eq!(rule["firstDayOfWeek"], "mo");
        let by_day = rule["byDay"].as_array().unwrap();
        assert_eq!(by_day.len(), 3);
        assert_eq!(by_day[0]["day"], "mo");
        assert_eq!(by_day[1]["day"], "we");
        assert_eq!(by_day[2]["day"], "fr");
    }

    #[test]
    fn absolute_monthly_uses_by_month_day() {
        let pr = json!({
            "pattern": {"type": "absoluteMonthly", "interval": 1, "dayOfMonth": 15},
            "range": {"type": "noEnd"}
        });
        let out = GraphRecurrence(&pr).to_rule().unwrap();
        let rule = &out;
        assert_eq!(rule["frequency"], "monthly");
        assert_eq!(rule["byMonthDay"][0], 15);
    }

    #[test]
    fn relative_monthly_emits_by_set_position() {
        let pr = json!({
            "pattern": {
                "type": "relativeMonthly",
                "interval": 1,
                "daysOfWeek": ["thursday"],
                "index": "third"
            },
            "range": {"type": "noEnd"}
        });
        let out = GraphRecurrence(&pr).to_rule().unwrap();
        let rule = &out;
        assert_eq!(rule["frequency"], "monthly");
        assert_eq!(rule["bySetPosition"][0], 3);
        assert_eq!(rule["byDay"][0]["day"], "th");
    }

    #[test]
    fn index_is_ignored_unless_the_pattern_is_relative() {
        for kind in ["daily", "weekly", "absoluteMonthly", "absoluteYearly"] {
            let pr = json!({
                "pattern": {"type": kind, "interval": 1, "index": "first",
                            "daysOfWeek": ["monday"], "dayOfMonth": 1, "month": 1},
                "range": {"type": "noEnd"}
            });
            let out = GraphRecurrence(&pr).to_rule().unwrap();
            assert!(
                out.get("bySetPosition").is_none(),
                "{kind} carries Graph's default index=first, which must not become bySetPosition"
            );
        }
    }

    #[test]
    fn index_survives_for_relative_patterns() {
        for kind in ["relativeMonthly", "relativeYearly"] {
            let pr = json!({
                "pattern": {"type": kind, "interval": 1, "index": "third",
                            "daysOfWeek": ["thursday"]},
                "range": {"type": "noEnd"}
            });
            let out = GraphRecurrence(&pr).to_rule().unwrap();
            assert_eq!(out["bySetPosition"][0], 3, "{kind} keeps its index");
        }
    }

    #[test]
    fn absolute_yearly_with_month_and_day() {
        let pr = json!({
            "pattern": {"type": "absoluteYearly", "interval": 1, "month": 7, "dayOfMonth": 4},
            "range": {"type": "noEnd"}
        });
        let out = GraphRecurrence(&pr).to_rule().unwrap();
        let rule = &out;
        assert_eq!(rule["frequency"], "yearly");
        assert_eq!(rule["byMonth"][0], "7");
        assert_eq!(rule["byMonthDay"][0], 4);
    }

    #[test]
    fn relative_yearly_uses_setpos_and_byday() {
        let pr = json!({
            "pattern": {
                "type": "relativeYearly", "interval": 1, "daysOfWeek": ["monday"],
                "index": "last", "month": 5
            },
            "range": {"type": "noEnd"}
        });
        let out = GraphRecurrence(&pr).to_rule().unwrap();
        let rule = &out;
        assert_eq!(rule["frequency"], "yearly");
        assert_eq!(rule["bySetPosition"][0], -1);
        assert_eq!(rule["byMonth"][0], "5");
    }

    #[test]
    fn range_end_date_maps_to_until_local_datetime() {
        let pr = json!({
            "pattern": {"type": "daily", "interval": 1},
            "range": {"type": "endDate", "endDate": "2026-12-31"}
        });
        let out = GraphRecurrence(&pr).to_rule().unwrap();
        assert_eq!(out["until"], "2026-12-31T23:59:59");
    }

    #[test]
    fn range_end_on_outlook_no_date_has_no_until() {
        let rule = GraphRecurrence(&json!({
            "pattern": {"type": "daily", "interval": 1},
            "range": {"type": "endDate", "startDate": "2026-01-01", "endDate": "4500-12-31"}
        }))
        .to_rule()
        .unwrap();
        assert!(rule.get("until").is_none());
    }

    #[test]
    fn range_end_datetime_preserved() {
        let pr = json!({
            "pattern": {"type": "daily", "interval": 1},
            "range": {"type": "endDate", "endDate": "2026-12-31T23:59:59"}
        });
        let out = GraphRecurrence(&pr).to_rule().unwrap();
        assert_eq!(out["until"], "2026-12-31T23:59:59");
    }

    #[test]
    fn range_numbered_maps_to_count() {
        let pr = json!({
            "pattern": {"type": "weekly", "interval": 1, "daysOfWeek": ["monday"]},
            "range": {"type": "numbered", "numberOfOccurrences": 10}
        });
        let out = GraphRecurrence(&pr).to_rule().unwrap();
        assert_eq!(out["count"], 10);
    }

    #[test]
    fn no_end_range_omits_until_and_count() {
        let pr = json!({
            "pattern": {"type": "daily", "interval": 1},
            "range": {"type": "noEnd"}
        });
        let out = GraphRecurrence(&pr).to_rule().unwrap();
        assert!(out.get("until").is_none());
        assert!(out.get("count").is_none());
    }
}
