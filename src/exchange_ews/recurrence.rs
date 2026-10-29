/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use serde_json::{Map, Value, json};

use crate::exchange::date::recurrence_until;
use crate::exchange_ews::parse::{RawRecurrence, RecurrencePattern, RecurrenceRange};

pub fn to_jscalendar_rule(raw: &RawRecurrence) -> Option<Value> {
    let mut rule: Map<String, Value> = Map::new();
    match raw.pattern.as_ref()? {
        RecurrencePattern::Daily { interval } => {
            rule.insert("frequency".to_owned(), Value::String("daily".to_owned()));
            if *interval > 1 {
                rule.insert("interval".to_owned(), Value::from(*interval));
            }
        }
        RecurrencePattern::Weekly {
            interval,
            days_of_week,
        } => {
            rule.insert("frequency".to_owned(), Value::String("weekly".to_owned()));
            if *interval > 1 {
                rule.insert("interval".to_owned(), Value::from(*interval));
            }
            let days: Vec<Value> = days_of_week
                .iter()
                .filter_map(|d| expand_day(d))
                .flat_map(|slice| slice.iter().map(|t| json!({"day": t})))
                .collect();
            if !days.is_empty() {
                rule.insert("byDay".to_owned(), Value::Array(days));
            }
        }
        RecurrencePattern::AbsoluteMonthly {
            interval,
            day_of_month,
        } => {
            rule.insert("frequency".to_owned(), Value::String("monthly".to_owned()));
            if *interval > 1 {
                rule.insert("interval".to_owned(), Value::from(*interval));
            }
            rule.insert(
                "byMonthDay".to_owned(),
                Value::Array(vec![Value::from(*day_of_month)]),
            );
        }
        RecurrencePattern::RelativeMonthly {
            interval,
            day_of_week_index,
            days_of_week,
        } => {
            rule.insert("frequency".to_owned(), Value::String("monthly".to_owned()));
            if *interval > 1 {
                rule.insert("interval".to_owned(), Value::from(*interval));
            }
            insert_relative_by_day(&mut rule, days_of_week, day_of_week_index);
        }
        RecurrencePattern::AbsoluteYearly {
            month,
            day_of_month,
        } => {
            rule.insert("frequency".to_owned(), Value::String("yearly".to_owned()));
            if let Some(n) = month_number(month) {
                rule.insert(
                    "byMonth".to_owned(),
                    Value::Array(vec![Value::String(n.to_string())]),
                );
            }
            rule.insert(
                "byMonthDay".to_owned(),
                Value::Array(vec![Value::from(*day_of_month)]),
            );
        }
        RecurrencePattern::RelativeYearly {
            month,
            day_of_week_index,
            days_of_week,
        } => {
            rule.insert("frequency".to_owned(), Value::String("yearly".to_owned()));
            if let Some(n) = month_number(month) {
                rule.insert(
                    "byMonth".to_owned(),
                    Value::Array(vec![Value::String(n.to_string())]),
                );
            }
            insert_relative_by_day(&mut rule, days_of_week, day_of_week_index);
        }
    }
    match raw.range.as_ref() {
        Some(RecurrenceRange::NoEnd { .. }) | None => {}
        Some(RecurrenceRange::EndDate { end_date, .. }) => {
            if let Some(local) = recurrence_until(end_date) {
                rule.insert("until".to_owned(), Value::String(local));
            }
        }
        Some(RecurrenceRange::Numbered {
            number_of_occurrences,
            ..
        }) => {
            rule.insert("count".to_owned(), Value::from(*number_of_occurrences));
        }
    }
    Some(Value::Object(rule))
}

fn expand_day(d: &str) -> Option<&'static [&'static str]> {
    Some(match d.to_ascii_lowercase().as_str() {
        "monday" | "mo" => &["mo"],
        "tuesday" | "tu" => &["tu"],
        "wednesday" | "we" => &["we"],
        "thursday" | "th" => &["th"],
        "friday" | "fr" => &["fr"],
        "saturday" | "sa" => &["sa"],
        "sunday" | "su" => &["su"],
        "day" => &["mo", "tu", "we", "th", "fr", "sa", "su"],
        "weekday" => &["mo", "tu", "we", "th", "fr"],
        "weekendday" => &["sa", "su"],
        _ => return None,
    })
}

fn insert_relative_by_day(rule: &mut Map<String, Value>, days_of_week: &[String], index: &str) {
    let nth = nth_of_period(index);
    let mut days: Vec<&'static str> = Vec::new();
    let mut is_set = false;
    for d in days_of_week {
        if let Some(slice) = expand_day(d) {
            if slice.len() > 1 {
                is_set = true;
            }
            for &t in slice {
                if !days.contains(&t) {
                    days.push(t);
                }
            }
        }
    }
    if days.is_empty() {
        return;
    }
    if is_set {
        let by_day: Vec<Value> = days.iter().map(|t| json!({"day": t})).collect();
        rule.insert("byDay".to_owned(), Value::Array(by_day));
        if let Some(n) = nth {
            rule.insert("bySetPosition".to_owned(), json!([n]));
        }
    } else {
        let by_day: Vec<Value> = days
            .iter()
            .map(|t| {
                let mut o = Map::new();
                o.insert("day".to_owned(), Value::String((*t).to_owned()));
                if let Some(n) = nth {
                    o.insert("nthOfPeriod".to_owned(), Value::from(n));
                }
                Value::Object(o)
            })
            .collect();
        rule.insert("byDay".to_owned(), Value::Array(by_day));
    }
}

fn nth_of_period(index: &str) -> Option<i32> {
    match index {
        "First" => Some(1),
        "Second" => Some(2),
        "Third" => Some(3),
        "Fourth" => Some(4),
        "Last" => Some(-1),
        _ => None,
    }
}

fn month_number(m: &str) -> Option<u32> {
    Some(match m {
        "January" => 1,
        "February" => 2,
        "March" => 3,
        "April" => 4,
        "May" => 5,
        "June" => 6,
        "July" => 7,
        "August" => 8,
        "September" => 9,
        "October" => 10,
        "November" => 11,
        "December" => 12,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_interval_round_trips() {
        let raw = RawRecurrence {
            pattern: Some(RecurrencePattern::Daily { interval: 3 }),
            range: Some(RecurrenceRange::Numbered {
                start_date: "2025-01-01".to_owned(),
                number_of_occurrences: 5,
            }),
        };
        let rule = to_jscalendar_rule(&raw).unwrap();
        assert_eq!(rule["frequency"], "daily");
        assert_eq!(rule["interval"], 3);
        assert_eq!(rule["count"], 5);
    }

    #[test]
    fn relative_monthly_nth_of_period() {
        let raw = RawRecurrence {
            pattern: Some(RecurrencePattern::RelativeMonthly {
                interval: 1,
                day_of_week_index: "First".to_owned(),
                days_of_week: vec!["Monday".to_owned()],
            }),
            range: Some(RecurrenceRange::NoEnd {
                start_date: "2025-01-01".to_owned(),
            }),
        };
        let rule = to_jscalendar_rule(&raw).unwrap();
        assert_eq!(rule["frequency"], "monthly");
        let by_day = rule["byDay"].as_array().unwrap();
        assert_eq!(by_day[0]["day"], "mo");
        assert_eq!(by_day[0]["nthOfPeriod"], 1);
    }

    #[test]
    fn relative_yearly_last_friday_in_june() {
        let raw = RawRecurrence {
            pattern: Some(RecurrencePattern::RelativeYearly {
                month: "June".to_owned(),
                day_of_week_index: "Last".to_owned(),
                days_of_week: vec!["Friday".to_owned()],
            }),
            range: Some(RecurrenceRange::NoEnd {
                start_date: "2020-01-01".to_owned(),
            }),
        };
        let rule = to_jscalendar_rule(&raw).unwrap();
        assert_eq!(rule["frequency"], "yearly");
        assert_eq!(rule["byMonth"][0], "6");
        assert_eq!(rule["byDay"][0]["day"], "fr");
        assert_eq!(rule["byDay"][0]["nthOfPeriod"], -1);
    }

    #[test]
    fn absolute_monthly_translates_to_by_month_day() {
        let raw = RawRecurrence {
            pattern: Some(RecurrencePattern::AbsoluteMonthly {
                interval: 2,
                day_of_month: 15,
            }),
            range: Some(RecurrenceRange::NoEnd {
                start_date: "2025-01-15".to_owned(),
            }),
        };
        let rule = to_jscalendar_rule(&raw).unwrap();
        assert_eq!(rule["frequency"], "monthly");
        assert_eq!(rule["interval"], 2);
        assert_eq!(rule["byMonthDay"][0], 15);
    }

    #[test]
    fn absolute_yearly_translates_to_by_month_and_day() {
        let raw = RawRecurrence {
            pattern: Some(RecurrencePattern::AbsoluteYearly {
                month: "January".to_owned(),
                day_of_month: 1,
            }),
            range: Some(RecurrenceRange::NoEnd {
                start_date: "2025-01-01".to_owned(),
            }),
        };
        let rule = to_jscalendar_rule(&raw).unwrap();
        assert_eq!(rule["frequency"], "yearly");
        assert_eq!(rule["byMonth"][0], "1");
        assert_eq!(rule["byMonthDay"][0], 1);
    }

    #[test]
    fn end_date_with_numeric_offset_yields_valid_local_until() {
        let raw = RawRecurrence {
            pattern: Some(RecurrencePattern::Weekly {
                interval: 2,
                days_of_week: vec!["Wednesday".to_owned()],
            }),
            range: Some(RecurrenceRange::EndDate {
                start_date: "2021-08-04-06:00".to_owned(),
                end_date: "2021-09-30-06:00".to_owned(),
            }),
        };
        let rule = to_jscalendar_rule(&raw).expect("rule");
        assert_eq!(rule["until"], "2021-09-30T23:59:59");
    }

    #[test]
    fn unusable_end_date_omits_until_rather_than_emitting_garbage() {
        let raw = RawRecurrence {
            pattern: Some(RecurrencePattern::Daily { interval: 1 }),
            range: Some(RecurrenceRange::EndDate {
                start_date: String::new(),
                end_date: "not-a-date".to_owned(),
            }),
        };
        let rule = to_jscalendar_rule(&raw).expect("rule");
        assert!(rule.get("until").is_none());
    }

    #[test]
    fn no_end_range_emits_no_until_or_count() {
        let raw = RawRecurrence {
            pattern: Some(RecurrencePattern::Daily { interval: 1 }),
            range: Some(RecurrenceRange::NoEnd {
                start_date: "2025-01-01".to_owned(),
            }),
        };
        let rule = to_jscalendar_rule(&raw).unwrap();
        assert!(rule.get("until").is_none());
        assert!(rule.get("count").is_none());
    }

    #[test]
    fn relative_monthly_first_weekday_expands_with_set_position() {
        let raw = RawRecurrence {
            pattern: Some(RecurrencePattern::RelativeMonthly {
                interval: 1,
                day_of_week_index: "First".to_owned(),
                days_of_week: vec!["Weekday".to_owned()],
            }),
            range: Some(RecurrenceRange::NoEnd {
                start_date: "2025-01-01".to_owned(),
            }),
        };
        let rule = to_jscalendar_rule(&raw).unwrap();
        let by_day = rule["byDay"].as_array().unwrap();
        let days: Vec<&str> = by_day.iter().map(|d| d["day"].as_str().unwrap()).collect();
        assert_eq!(days, ["mo", "tu", "we", "th", "fr"]);
        assert!(
            by_day.iter().all(|d| d.get("nthOfPeriod").is_none()),
            "set-based byDay must not carry per-day nthOfPeriod"
        );
        assert_eq!(rule["bySetPosition"], json!([1]));
    }

    #[test]
    fn relative_monthly_last_weekendday_uses_negative_set_position() {
        let raw = RawRecurrence {
            pattern: Some(RecurrencePattern::RelativeMonthly {
                interval: 1,
                day_of_week_index: "Last".to_owned(),
                days_of_week: vec!["WeekendDay".to_owned()],
            }),
            range: Some(RecurrenceRange::NoEnd {
                start_date: "2025-01-01".to_owned(),
            }),
        };
        let rule = to_jscalendar_rule(&raw).unwrap();
        let days: Vec<&str> = rule["byDay"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["day"].as_str().unwrap())
            .collect();
        assert_eq!(days, ["sa", "su"]);
        assert_eq!(rule["bySetPosition"], json!([-1]));
    }

    #[test]
    fn end_date_with_trailing_z_yields_valid_local_until() {
        let raw = RawRecurrence {
            pattern: Some(RecurrencePattern::Daily { interval: 2 }),
            range: Some(RecurrenceRange::EndDate {
                start_date: "2026-07-01Z".to_owned(),
                end_date: "2026-08-01Z".to_owned(),
            }),
        };
        let rule = to_jscalendar_rule(&raw).unwrap();
        assert_eq!(rule["until"], "2026-08-01T23:59:59");
    }

    #[test]
    fn weekly_every_weekday_expands() {
        let raw = RawRecurrence {
            pattern: Some(RecurrencePattern::Weekly {
                interval: 1,
                days_of_week: vec!["Weekday".to_owned()],
            }),
            range: Some(RecurrenceRange::NoEnd {
                start_date: "2025-01-06".to_owned(),
            }),
        };
        let rule = to_jscalendar_rule(&raw).unwrap();
        let days: Vec<&str> = rule["byDay"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["day"].as_str().unwrap())
            .collect();
        assert_eq!(days, ["mo", "tu", "we", "th", "fr"]);
        assert!(rule.get("bySetPosition").is_none());
    }

    #[test]
    fn weekly_two_days() {
        let raw = RawRecurrence {
            pattern: Some(RecurrencePattern::Weekly {
                interval: 1,
                days_of_week: vec!["Monday".to_owned(), "Wednesday".to_owned()],
            }),
            range: Some(RecurrenceRange::EndDate {
                start_date: "2025-01-06".to_owned(),
                end_date: "2025-06-30".to_owned(),
            }),
        };
        let rule = to_jscalendar_rule(&raw).unwrap();
        assert_eq!(rule["frequency"], "weekly");
        assert_eq!(rule["until"], "2025-06-30T23:59:59");
        let by_day = rule["byDay"].as_array().unwrap();
        assert_eq!(by_day.len(), 2);
        assert_eq!(by_day[0]["day"], "mo");
        assert_eq!(by_day[1]["day"], "we");
    }
}
