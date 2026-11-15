/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

#[derive(Debug, Clone, Default)]
pub struct RawRecurrence {
    pub pattern: Option<RecurrencePattern>,
    pub range: Option<RecurrenceRange>,
}

#[derive(Debug, Clone)]
pub enum RecurrencePattern {
    Daily {
        interval: u32,
    },
    Weekly {
        interval: u32,
        days_of_week: Vec<String>,
    },
    AbsoluteMonthly {
        interval: u32,
        day_of_month: u32,
    },
    RelativeMonthly {
        interval: u32,
        day_of_week_index: String,
        days_of_week: Vec<String>,
    },
    AbsoluteYearly {
        month: String,
        day_of_month: u32,
    },
    RelativeYearly {
        month: String,
        day_of_week_index: String,
        days_of_week: Vec<String>,
    },
}

#[derive(Debug, Clone)]
pub enum RecurrenceRange {
    NoEnd {
        start_date: String,
    },
    EndDate {
        start_date: String,
        end_date: String,
    },
    Numbered {
        start_date: String,
        number_of_occurrences: u32,
    },
}

#[derive(Clone, Copy)]
pub(super) enum PatternKind {
    Daily,
    Weekly,
    AbsoluteMonthly,
    RelativeMonthly,
    AbsoluteYearly,
    RelativeYearly,
}

#[derive(Clone, Copy)]
pub(super) enum RangeKind {
    NoEnd,
    EndDate,
    Numbered,
}

#[derive(Clone, Copy)]
pub(super) enum RecurrenceText {
    Interval,
    DaysOfWeek,
    DayOfMonth,
    DayOfWeekIndex,
    Month,
    StartDate,
    EndDate,
    NumberOfOccurrences,
}

#[derive(Clone, Copy)]
pub(super) enum RecurrenceElement {
    Text(RecurrenceText),
    Pattern(PatternKind),
    Range(RangeKind),
}

impl RecurrenceElement {
    pub(super) fn parse(local: &str) -> Option<RecurrenceElement> {
        hashify::map_ignore_case!(
            local.as_bytes(),
            RecurrenceElement,
            "Interval" => RecurrenceElement::Text(RecurrenceText::Interval),
            "DaysOfWeek" => RecurrenceElement::Text(RecurrenceText::DaysOfWeek),
            "DayOfMonth" => RecurrenceElement::Text(RecurrenceText::DayOfMonth),
            "DayOfWeekIndex" => RecurrenceElement::Text(RecurrenceText::DayOfWeekIndex),
            "Month" => RecurrenceElement::Text(RecurrenceText::Month),
            "StartDate" => RecurrenceElement::Text(RecurrenceText::StartDate),
            "EndDate" => RecurrenceElement::Text(RecurrenceText::EndDate),
            "NumberOfOccurrences" => RecurrenceElement::Text(RecurrenceText::NumberOfOccurrences),
            "DailyRecurrence" => RecurrenceElement::Pattern(PatternKind::Daily),
            "WeeklyRecurrence" => RecurrenceElement::Pattern(PatternKind::Weekly),
            "AbsoluteMonthlyRecurrence" => RecurrenceElement::Pattern(PatternKind::AbsoluteMonthly),
            "RelativeMonthlyRecurrence" => RecurrenceElement::Pattern(PatternKind::RelativeMonthly),
            "AbsoluteYearlyRecurrence" => RecurrenceElement::Pattern(PatternKind::AbsoluteYearly),
            "RelativeYearlyRecurrence" => RecurrenceElement::Pattern(PatternKind::RelativeYearly),
            "NoEndRecurrence" => RecurrenceElement::Range(RangeKind::NoEnd),
            "EndDateRecurrence" => RecurrenceElement::Range(RangeKind::EndDate),
            "NumberedRecurrence" => RecurrenceElement::Range(RangeKind::Numbered),
        )
        .copied()
    }
}

#[derive(Default)]
pub(super) struct PendingRecurrence {
    pub(super) pattern: Option<PatternKind>,
    pub(super) range: Option<RangeKind>,
    interval: u32,
    days_of_week: Vec<String>,
    day_of_month: u32,
    day_of_week_index: String,
    month: String,
    start_date: String,
    end_date: String,
    number_of_occurrences: u32,
}

impl PendingRecurrence {
    pub(super) fn text(&mut self, target: RecurrenceText, text: &str) {
        let trimmed = text.trim();
        match target {
            RecurrenceText::Interval => self.interval = trimmed.parse().unwrap_or(1),
            RecurrenceText::DaysOfWeek => self
                .days_of_week
                .extend(text.split_whitespace().map(str::to_owned)),
            RecurrenceText::DayOfMonth => self.day_of_month = trimmed.parse().unwrap_or(0),
            RecurrenceText::DayOfWeekIndex => trimmed.clone_into(&mut self.day_of_week_index),
            RecurrenceText::Month => trimmed.clone_into(&mut self.month),
            RecurrenceText::StartDate => trimmed.clone_into(&mut self.start_date),
            RecurrenceText::EndDate => trimmed.clone_into(&mut self.end_date),
            RecurrenceText::NumberOfOccurrences => {
                self.number_of_occurrences = trimmed.parse().unwrap_or(0);
            }
        }
    }

    pub(super) fn finish(self) -> RawRecurrence {
        let interval = if self.interval == 0 { 1 } else { self.interval };
        let pattern = self.pattern.map(|kind| match kind {
            PatternKind::Daily => RecurrencePattern::Daily { interval },
            PatternKind::Weekly => RecurrencePattern::Weekly {
                interval,
                days_of_week: self.days_of_week,
            },
            PatternKind::AbsoluteMonthly => RecurrencePattern::AbsoluteMonthly {
                interval,
                day_of_month: self.day_of_month,
            },
            PatternKind::RelativeMonthly => RecurrencePattern::RelativeMonthly {
                interval,
                day_of_week_index: self.day_of_week_index,
                days_of_week: self.days_of_week,
            },
            PatternKind::AbsoluteYearly => RecurrencePattern::AbsoluteYearly {
                month: self.month,
                day_of_month: self.day_of_month,
            },
            PatternKind::RelativeYearly => RecurrencePattern::RelativeYearly {
                month: self.month,
                day_of_week_index: self.day_of_week_index,
                days_of_week: self.days_of_week,
            },
        });
        let start_date = self.start_date;
        let range = self.range.map(|kind| match kind {
            RangeKind::NoEnd => RecurrenceRange::NoEnd { start_date },
            RangeKind::EndDate => RecurrenceRange::EndDate {
                start_date,
                end_date: self.end_date,
            },
            RangeKind::Numbered => RecurrenceRange::Numbered {
                start_date,
                number_of_occurrences: self.number_of_occurrences,
            },
        });
        RawRecurrence { pattern, range }
    }
}
