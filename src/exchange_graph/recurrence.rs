/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use serde_json::{Value, json};

use crate::exchange::date::recurrence_until;
use crate::exchange_graph::error::GraphError;

pub fn convert_patterned_recurrence(pr: &Value) -> Result<Value, GraphError> {
    Ok(Value::Object(convert_patterned_recurrence_rule(pr)?))
}

pub fn convert_patterned_recurrence_rule(
    pr: &Value,
) -> Result<serde_json::Map<String, Value>, GraphError> {
    let pattern = pr
        .get("pattern")
        .ok_or_else(|| GraphError::Malformed("recurrence.pattern missing".to_owned()))?;
    let range = pr
        .get("range")
        .ok_or_else(|| GraphError::Malformed("recurrence.range missing".to_owned()))?;

    let mut rule = serde_json::Map::new();
    rule.insert("@type".to_owned(), Value::from("RecurrenceRule"));
    rule.insert("frequency".to_owned(), Value::from(frequency_for(pattern)));

    if let Some(interval) = pattern.get("interval").and_then(Value::as_u64)
        && interval != 1
    {
        rule.insert("interval".to_owned(), Value::from(interval));
    }

    if let Some(first_day) = pattern.get("firstDayOfWeek").and_then(Value::as_str)
        && let Some(short) = day_short(first_day)
    {
        rule.insert("firstDayOfWeek".to_owned(), Value::from(short));
    }

    if let Some(days) = pattern.get("daysOfWeek").and_then(Value::as_array) {
        let by_day: Vec<Value> = days
            .iter()
            .filter_map(Value::as_str)
            .filter_map(day_short)
            .map(|d| json!({"@type": "NDay", "day": d}))
            .collect();
        if !by_day.is_empty() {
            rule.insert("byDay".to_owned(), Value::Array(by_day));
        }
    }

    if let Some(dom) = pattern.get("dayOfMonth").and_then(Value::as_i64)
        && dom != 0
    {
        rule.insert(
            "byMonthDay".to_owned(),
            Value::Array(vec![Value::from(dom)]),
        );
    }

    if let Some(month) = pattern.get("month").and_then(Value::as_i64)
        && month != 0
    {
        rule.insert(
            "byMonth".to_owned(),
            Value::Array(vec![Value::String(month.to_string())]),
        );
    }

    if let Some(index) = pattern.get("index").and_then(Value::as_str)
        && let Some(setpos) = set_position_for(index)
    {
        rule.insert(
            "bySetPosition".to_owned(),
            Value::Array(vec![Value::from(setpos)]),
        );
    }

    let range_type = range.get("type").and_then(Value::as_str).unwrap_or("noEnd");
    match range_type {
        "endDate" => {
            if let Some(local) = range
                .get("endDate")
                .and_then(Value::as_str)
                .and_then(recurrence_until)
            {
                rule.insert("until".to_owned(), Value::from(local));
            }
        }
        "numbered" => {
            if let Some(count) = range.get("numberOfOccurrences").and_then(Value::as_u64) {
                rule.insert("count".to_owned(), Value::from(count));
            }
        }
        _ => {}
    }

    Ok(rule)
}

fn frequency_for(pattern: &Value) -> &'static str {
    match pattern.get("type").and_then(Value::as_str).unwrap_or("") {
        "daily" => "daily",
        "weekly" => "weekly",
        "absoluteMonthly" | "relativeMonthly" => "monthly",
        "absoluteYearly" | "relativeYearly" => "yearly",
        _ => "daily",
    }
}

fn day_short(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "monday" => Some("mo"),
        "tuesday" => Some("tu"),
        "wednesday" => Some("we"),
        "thursday" => Some("th"),
        "friday" => Some("fr"),
        "saturday" => Some("sa"),
        "sunday" => Some("su"),
        _ => None,
    }
}

fn set_position_for(index: &str) -> Option<i64> {
    match index.to_ascii_lowercase().as_str() {
        "first" => Some(1),
        "second" => Some(2),
        "third" => Some(3),
        "fourth" => Some(4),
        "last" => Some(-1),
        _ => None,
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
        let out = convert_patterned_recurrence(&pr).unwrap();
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
        let out = convert_patterned_recurrence(&pr).unwrap();
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
        let out = convert_patterned_recurrence(&pr).unwrap();
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
        let out = convert_patterned_recurrence(&pr).unwrap();
        let rule = &out;
        assert_eq!(rule["frequency"], "monthly");
        assert_eq!(rule["bySetPosition"][0], 3);
        assert_eq!(rule["byDay"][0]["day"], "th");
    }

    #[test]
    fn absolute_yearly_with_month_and_day() {
        let pr = json!({
            "pattern": {"type": "absoluteYearly", "interval": 1, "month": 7, "dayOfMonth": 4},
            "range": {"type": "noEnd"}
        });
        let out = convert_patterned_recurrence(&pr).unwrap();
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
        let out = convert_patterned_recurrence(&pr).unwrap();
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
        let out = convert_patterned_recurrence(&pr).unwrap();
        assert_eq!(out["until"], "2026-12-31T23:59:59");
    }

    #[test]
    fn range_end_datetime_preserved() {
        let pr = json!({
            "pattern": {"type": "daily", "interval": 1},
            "range": {"type": "endDate", "endDate": "2026-12-31T23:59:59"}
        });
        let out = convert_patterned_recurrence(&pr).unwrap();
        assert_eq!(out["until"], "2026-12-31T23:59:59");
    }

    #[test]
    fn range_numbered_maps_to_count() {
        let pr = json!({
            "pattern": {"type": "weekly", "interval": 1, "daysOfWeek": ["monday"]},
            "range": {"type": "numbered", "numberOfOccurrences": 10}
        });
        let out = convert_patterned_recurrence(&pr).unwrap();
        assert_eq!(out["count"], 10);
    }

    #[test]
    fn no_end_range_omits_until_and_count() {
        let pr = json!({
            "pattern": {"type": "daily", "interval": 1},
            "range": {"type": "noEnd"}
        });
        let out = convert_patterned_recurrence(&pr).unwrap();
        assert!(out.get("until").is_none());
        assert!(out.get("count").is_none());
    }
}
