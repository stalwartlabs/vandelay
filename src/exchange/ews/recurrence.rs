/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use serde_json::{Map, Value, json};

use crate::exchange::date::WireDate;
use crate::exchange::ews::parse::{RawRecurrence, RecurrencePattern, RecurrenceRange};
use crate::exchange::vocab::{MonthName, Ordinal};

type DaySet = &'static [&'static str];

impl RawRecurrence {
    pub fn to_jscalendar(&self) -> Option<Value> {
        let mut rule: Map<String, Value> = Map::new();
        match self.pattern.as_ref()? {
            RecurrencePattern::Daily { interval } => {
                RawRecurrence::frequency(&mut rule, "daily", Some(*interval));
            }
            RecurrencePattern::Weekly {
                interval,
                days_of_week,
            } => {
                RawRecurrence::frequency(&mut rule, "weekly", Some(*interval));
                let days: Vec<Value> = days_of_week
                    .iter()
                    .filter_map(|d| RawRecurrence::expand_day(d))
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
                RawRecurrence::frequency(&mut rule, "monthly", Some(*interval));
                rule.insert("byMonthDay".to_owned(), json!([*day_of_month]));
            }
            RecurrencePattern::RelativeMonthly {
                interval,
                day_of_week_index,
                days_of_week,
            } => {
                RawRecurrence::frequency(&mut rule, "monthly", Some(*interval));
                RawRecurrence::insert_relative_by_day(&mut rule, days_of_week, day_of_week_index);
            }
            RecurrencePattern::AbsoluteYearly {
                month,
                day_of_month,
            } => {
                RawRecurrence::frequency(&mut rule, "yearly", None);
                RawRecurrence::insert_month(&mut rule, month);
                rule.insert("byMonthDay".to_owned(), json!([*day_of_month]));
            }
            RecurrencePattern::RelativeYearly {
                month,
                day_of_week_index,
                days_of_week,
            } => {
                RawRecurrence::frequency(&mut rule, "yearly", None);
                RawRecurrence::insert_month(&mut rule, month);
                RawRecurrence::insert_relative_by_day(&mut rule, days_of_week, day_of_week_index);
            }
        }
        match self.range.as_ref() {
            Some(RecurrenceRange::NoEnd { .. }) | None => {}
            Some(RecurrenceRange::EndDate { end_date, .. }) => {
                if let Some(local) = WireDate(end_date).recurrence_until() {
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

    fn frequency(rule: &mut Map<String, Value>, frequency: &str, interval: Option<u32>) {
        rule.insert("frequency".to_owned(), Value::from(frequency));
        if let Some(interval) = interval.filter(|i| *i > 1) {
            rule.insert("interval".to_owned(), Value::from(interval));
        }
    }

    fn insert_month(rule: &mut Map<String, Value>, month: &str) {
        if let Some(n) = MonthName(month).number() {
            rule.insert("byMonth".to_owned(), json!([n.to_string()]));
        }
    }

    fn expand_day(day: &str) -> Option<DaySet> {
        hashify::map_ignore_case!(
            day.as_bytes(),
            DaySet,
            "monday" => &["mo"],
            "mo" => &["mo"],
            "tuesday" => &["tu"],
            "tu" => &["tu"],
            "wednesday" => &["we"],
            "we" => &["we"],
            "thursday" => &["th"],
            "th" => &["th"],
            "friday" => &["fr"],
            "fr" => &["fr"],
            "saturday" => &["sa"],
            "sa" => &["sa"],
            "sunday" => &["su"],
            "su" => &["su"],
            "day" => &["mo", "tu", "we", "th", "fr", "sa", "su"],
            "weekday" => &["mo", "tu", "we", "th", "fr"],
            "weekendday" => &["sa", "su"],
        )
        .copied()
    }

    fn insert_relative_by_day(rule: &mut Map<String, Value>, days_of_week: &[String], index: &str) {
        let nth = Ordinal::parse_exact(index).map(Ordinal::position);
        let mut days: Vec<&'static str> = Vec::new();
        let mut is_set = false;
        for slice in days_of_week
            .iter()
            .filter_map(|d| RawRecurrence::expand_day(d))
        {
            is_set |= slice.len() > 1;
            for &token in slice {
                if !days.contains(&token) {
                    days.push(token);
                }
            }
        }
        if days.is_empty() {
            return;
        }
        if is_set {
            let by_day = days.iter().map(|t| json!({"day": t})).collect();
            rule.insert("byDay".to_owned(), Value::Array(by_day));
            if let Some(n) = nth {
                rule.insert("bySetPosition".to_owned(), json!([n]));
            }
            return;
        }
        let by_day = {
            days.iter()
                .map(|t| {
                    let mut o = Map::new();
                    o.insert("day".to_owned(), Value::from(*t));
                    if let Some(n) = nth {
                        o.insert("nthOfPeriod".to_owned(), Value::from(n));
                    }
                    Value::Object(o)
                })
                .collect()
        };
        rule.insert("byDay".to_owned(), Value::Array(by_day));
    }
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
        let rule = raw.to_jscalendar().unwrap();
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
        let rule = raw.to_jscalendar().unwrap();
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
        let rule = raw.to_jscalendar().unwrap();
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
        let rule = raw.to_jscalendar().unwrap();
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
        let rule = raw.to_jscalendar().unwrap();
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
        let rule = raw.to_jscalendar().expect("rule");
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
        let rule = raw.to_jscalendar().expect("rule");
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
        let rule = raw.to_jscalendar().unwrap();
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
        let rule = raw.to_jscalendar().unwrap();
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
        let rule = raw.to_jscalendar().unwrap();
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
        let rule = raw.to_jscalendar().unwrap();
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
        let rule = raw.to_jscalendar().unwrap();
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
        let rule = raw.to_jscalendar().unwrap();
        assert_eq!(rule["frequency"], "weekly");
        assert_eq!(rule["until"], "2025-06-30T23:59:59");
        let by_day = rule["byDay"].as_array().unwrap();
        assert_eq!(by_day.len(), 2);
        assert_eq!(by_day[0]["day"], "mo");
        assert_eq!(by_day[1]["day"], "we");
    }
}
