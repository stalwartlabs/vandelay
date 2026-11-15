/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt::Write;

use chrono::Weekday;

const SECONDS_PER_HOUR: i64 = 3600;
const SECONDS_PER_MINUTE: i64 = 60;

#[derive(Debug, Clone, Copy)]
pub struct WeekdayName<'a>(pub &'a str);

impl WeekdayName<'_> {
    pub fn parse(self) -> Option<Weekday> {
        hashify::map_ignore_case!(
            self.0.as_bytes(),
            Weekday,
            "monday" => Weekday::Mon,
            "tuesday" => Weekday::Tue,
            "wednesday" => Weekday::Wed,
            "thursday" => Weekday::Thu,
            "friday" => Weekday::Fri,
            "saturday" => Weekday::Sat,
            "sunday" => Weekday::Sun,
        )
        .copied()
    }

    pub fn long(day: Weekday) -> &'static str {
        match day {
            Weekday::Mon => "monday",
            Weekday::Tue => "tuesday",
            Weekday::Wed => "wednesday",
            Weekday::Thu => "thursday",
            Weekday::Fri => "friday",
            Weekday::Sat => "saturday",
            Weekday::Sun => "sunday",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DayToken(pub Weekday);

impl DayToken {
    pub fn parse(token: &str) -> Option<DayToken> {
        hashify::map_ignore_case!(
            token.as_bytes(),
            Weekday,
            "mo" => Weekday::Mon,
            "tu" => Weekday::Tue,
            "we" => Weekday::Wed,
            "th" => Weekday::Thu,
            "fr" => Weekday::Fri,
            "sa" => Weekday::Sat,
            "su" => Weekday::Sun,
        )
        .copied()
        .map(DayToken)
    }

    pub fn as_str(self) -> &'static str {
        match self.0 {
            Weekday::Mon => "mo",
            Weekday::Tue => "tu",
            Weekday::Wed => "we",
            Weekday::Thu => "th",
            Weekday::Fri => "fr",
            Weekday::Sat => "sa",
            Weekday::Sun => "su",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ordinal {
    First,
    Second,
    Third,
    Fourth,
    Last,
}

impl Ordinal {
    pub fn parse(name: &str) -> Option<Ordinal> {
        hashify::map_ignore_case!(
            name.as_bytes(),
            Ordinal,
            "first" => Ordinal::First,
            "second" => Ordinal::Second,
            "third" => Ordinal::Third,
            "fourth" => Ordinal::Fourth,
            "last" => Ordinal::Last,
        )
        .copied()
    }

    pub fn parse_exact(name: &str) -> Option<Ordinal> {
        hashify::map!(
            name.as_bytes(),
            Ordinal,
            "First" => Ordinal::First,
            "Second" => Ordinal::Second,
            "Third" => Ordinal::Third,
            "Fourth" => Ordinal::Fourth,
            "Last" => Ordinal::Last,
        )
        .copied()
    }

    pub fn position(self) -> i32 {
        match self {
            Ordinal::First => 1,
            Ordinal::Second => 2,
            Ordinal::Third => 3,
            Ordinal::Fourth => 4,
            Ordinal::Last => -1,
        }
    }

    pub fn from_position(position: i64) -> Ordinal {
        match position {
            2 => Ordinal::Second,
            3 => Ordinal::Third,
            4 => Ordinal::Fourth,
            -1 => Ordinal::Last,
            _ => Ordinal::First,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Ordinal::First => "first",
            Ordinal::Second => "second",
            Ordinal::Third => "third",
            Ordinal::Fourth => "fourth",
            Ordinal::Last => "last",
        }
    }

    pub fn pick<T: Copy>(self, matches: &[T]) -> Option<T> {
        match self {
            Ordinal::First => matches.first(),
            Ordinal::Second => matches.get(1),
            Ordinal::Third => matches.get(2),
            Ordinal::Fourth => matches.get(3),
            Ordinal::Last => matches.last(),
        }
        .copied()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MonthName<'a>(pub &'a str);

impl MonthName<'_> {
    pub fn number(self) -> Option<u32> {
        hashify::map!(
            self.0.as_bytes(),
            u32,
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
        )
        .copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privacy {
    Public,
    Private,
    Secret,
}

impl Privacy {
    pub fn from_sensitivity(value: &str) -> Privacy {
        hashify::map_ignore_case!(
            value.as_bytes(),
            Privacy,
            "private" => Privacy::Private,
            "confidential" => Privacy::Secret,
        )
        .copied()
        .unwrap_or(Privacy::Public)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Privacy::Public => "public",
            Privacy::Private => "private",
            Privacy::Secret => "secret",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Participation {
    Accepted,
    Tentative,
    Declined,
    NeedsAction,
}

impl Participation {
    pub fn from_ews(response_type: &str) -> Option<Participation> {
        hashify::map!(
            response_type.as_bytes(),
            Participation,
            "Accept" => Participation::Accepted,
            "Tentative" => Participation::Tentative,
            "Decline" => Participation::Declined,
            "Organizer" => Participation::Accepted,
        )
        .copied()
    }

    pub fn from_graph(response: &str) -> Option<Participation> {
        hashify::map!(
            response.as_bytes(),
            Participation,
            "accepted" => Participation::Accepted,
            "organizer" => Participation::Accepted,
            "declined" => Participation::Declined,
            "tentativelyAccepted" => Participation::Tentative,
            "notResponded" => Participation::NeedsAction,
            "none" => Participation::NeedsAction,
        )
        .copied()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Participation::Accepted => "accepted",
            Participation::Tentative => "tentative",
            Participation::Declined => "declined",
            Participation::NeedsAction => "needs-action",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationStyle {
    Ews,
    Graph,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsoDuration(pub i64);

impl IsoDuration {
    pub fn format(self, style: DurationStyle) -> Option<String> {
        let total = self.0;
        match style {
            DurationStyle::Ews if total == 0 => return Some("PT0S".to_owned()),
            DurationStyle::Ews if total < 0 => return None,
            DurationStyle::Graph if total <= 0 => return None,
            _ => {}
        }
        let hours = total / SECONDS_PER_HOUR;
        let minutes = (total % SECONDS_PER_HOUR) / SECONDS_PER_MINUTE;
        let seconds = total % SECONDS_PER_MINUTE;
        let mut out = String::from("PT");
        if hours > 0 {
            let _ = write!(out, "{hours}H");
        }
        let bridge_minutes = style == DurationStyle::Graph && hours > 0 && seconds > 0;
        if minutes > 0 || bridge_minutes {
            let _ = write!(out, "{minutes}M");
        }
        if seconds > 0 || (hours == 0 && minutes == 0) {
            let _ = write!(out, "{seconds}S");
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekday_names_parse_in_any_case_and_round_trip_to_tokens() {
        for (name, token) in [
            ("Monday", "mo"),
            ("TUESDAY", "tu"),
            ("wednesday", "we"),
            ("Thursday", "th"),
            ("friday", "fr"),
            ("Saturday", "sa"),
            ("sunday", "su"),
        ] {
            let day = WeekdayName(name).parse().expect(name);
            assert_eq!(DayToken(day).as_str(), token);
        }
        assert_eq!(WeekdayName("mo").parse(), None);
        assert_eq!(
            DayToken::parse("MO").map(|d| WeekdayName::long(d.0)),
            Some("monday")
        );
    }

    #[test]
    fn ordinals_keep_their_case_rules() {
        assert_eq!(Ordinal::parse("LAST"), Some(Ordinal::Last));
        assert_eq!(Ordinal::parse_exact("Last"), Some(Ordinal::Last));
        assert_eq!(Ordinal::parse_exact("last"), None);
        assert_eq!(Ordinal::Fourth.position(), 4);
        assert_eq!(Ordinal::Last.pick(&[1, 2, 3]), Some(3));
        assert_eq!(Ordinal::Fourth.pick(&[1, 2, 3]), None);
        assert_eq!(Ordinal::from_position(-1).name(), "last");
        assert_eq!(Ordinal::from_position(9), Ordinal::First);
    }

    #[test]
    fn month_names_are_case_sensitive() {
        assert_eq!(MonthName("June").number(), Some(6));
        assert_eq!(MonthName("june").number(), None);
    }

    #[test]
    fn privacy_is_case_insensitive_for_both_servers() {
        for (value, expected) in [
            ("Private", "private"),
            ("private", "private"),
            ("Confidential", "secret"),
            ("confidential", "secret"),
            ("Normal", "public"),
            ("personal", "public"),
        ] {
            assert_eq!(Privacy::from_sensitivity(value).as_str(), expected);
        }
    }

    #[test]
    fn duration_styles_keep_their_zero_and_minute_rules() {
        assert_eq!(
            IsoDuration(0).format(DurationStyle::Ews).as_deref(),
            Some("PT0S")
        );
        assert_eq!(IsoDuration(0).format(DurationStyle::Graph), None);
        assert_eq!(IsoDuration(-5).format(DurationStyle::Ews), None);
        assert_eq!(
            IsoDuration(3605).format(DurationStyle::Ews).as_deref(),
            Some("PT1H5S")
        );
        assert_eq!(
            IsoDuration(3605).format(DurationStyle::Graph).as_deref(),
            Some("PT1H0M5S")
        );
        assert_eq!(
            IsoDuration(5400).format(DurationStyle::Graph).as_deref(),
            Some("PT1H30M")
        );
        assert_eq!(
            IsoDuration(45).format(DurationStyle::Ews).as_deref(),
            Some("PT45S")
        );
    }

    #[test]
    fn participation_vocabularies_differ_per_server() {
        assert_eq!(
            Participation::from_ews("Organizer"),
            Some(Participation::Accepted)
        );
        assert_eq!(Participation::from_ews("accepted"), None);
        assert_eq!(
            Participation::from_graph("tentativelyAccepted"),
            Some(Participation::Tentative)
        );
        assert_eq!(Participation::NeedsAction.as_str(), "needs-action");
    }
}
