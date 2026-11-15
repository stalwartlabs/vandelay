/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

pub mod calcard;
pub mod dates;
pub mod diff;
pub mod emailmeta;
pub(crate) mod jmap_map;
pub mod keys;
pub mod keywords;
pub mod pool;
pub mod run;
pub mod tree;
pub mod vanish;

use std::path::PathBuf;

use crate::error::Error;
use crate::logging::Logger;
use crate::net::http::{ClientOptions, RetryStats, TlsPolicy};
use crate::net::retry::RetryPolicy;
use crate::types::ObjectType;

pub struct RunOptions {
    pub archive: PathBuf,
    pub threads: usize,
    pub mode: RunMode,
    pub max_retries: u32,
    pub tls: TlsPolicy,
    pub logger: Logger,
}

impl RunOptions {
    pub fn client_options(&self) -> ClientOptions {
        ClientOptions {
            retry: RetryPolicy::new(self.max_retries),
            tls: self.tls,
            logger: self.logger,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    Apply,
    DryRun,
}

#[derive(Debug, Default, Clone)]
pub struct TypeCounts {
    pub created: u64,
    pub fetched: u64,
    pub updated: u64,
    pub deleted: u64,
    pub skipped: u64,
    pub failed: u64,
}

impl TypeCounts {
    pub fn add(&mut self, other: &TypeCounts) {
        self.created += other.created;
        self.fetched += other.fetched;
        self.updated += other.updated;
        self.deleted += other.deleted;
        self.skipped += other.skipped;
        self.failed += other.failed;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SummaryType {
    Object(ObjectType),
    VacationResponse,
}

impl SummaryType {
    pub const fn label(self) -> &'static str {
        match self {
            SummaryType::Object(ty) => ty.label(),
            SummaryType::VacationResponse => "Vacation response",
        }
    }

    pub const fn noun(self) -> &'static str {
        match self {
            SummaryType::Object(ty) => ty.noun(),
            SummaryType::VacationResponse => "vacation response",
        }
    }
}

impl From<ObjectType> for SummaryType {
    fn from(ty: ObjectType) -> SummaryType {
        SummaryType::Object(ty)
    }
}

impl PartialEq<ObjectType> for SummaryType {
    fn eq(&self, other: &ObjectType) -> bool {
        *self == SummaryType::Object(*other)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PlanCounts {
    pub new: u64,
    pub vanished: u64,
    pub present: u64,
}

#[derive(Debug, Default, Clone)]
pub struct Summary {
    pub per_type: Vec<(SummaryType, TypeCounts)>,
    pub plan: Vec<(SummaryType, PlanCounts)>,
    pub retries_observed: u64,
    pub retry_after_sleeps: u64,
}

impl Summary {
    pub fn counts(&mut self, ty: impl Into<SummaryType>) -> &mut TypeCounts {
        Summary::slot(&mut self.per_type, ty.into())
    }

    pub fn get(&self, ty: impl Into<SummaryType>) -> Option<&TypeCounts> {
        let ty = ty.into();
        self.per_type
            .iter()
            .find_map(|(t, counts)| (*t == ty).then_some(counts))
    }

    pub fn plan(&mut self, ty: impl Into<SummaryType>) -> &mut PlanCounts {
        Summary::slot(&mut self.plan, ty.into())
    }

    fn slot<T: Default>(rows: &mut Vec<(SummaryType, T)>, ty: SummaryType) -> &mut T {
        let index = rows.iter().position(|(t, _)| *t == ty);
        let (_, value) = match index {
            Some(index) => &mut rows[index],
            None => rows.push_mut((ty, T::default())),
        };
        value
    }

    pub fn set_retry_stats(&mut self, stats: RetryStats) {
        self.retries_observed = stats.retries_observed;
        self.retry_after_sleeps = stats.retry_after_sleeps;
    }

    pub fn any_failed(&self) -> bool {
        self.per_type.iter().any(|(_, c)| c.failed > 0)
    }

    pub fn failed(&self) -> u64 {
        self.per_type.iter().map(|(_, c)| c.failed).sum()
    }
}

#[derive(Debug)]
pub struct RunOutcome {
    pub summary: Summary,
    pub error: Option<Error>,
}

impl RunOutcome {
    pub fn failed(error: Error) -> RunOutcome {
        RunOutcome {
            summary: Summary::default(),
            error: Some(error),
        }
    }

    pub fn into_result(self) -> Result<Summary, Error> {
        match self.error {
            Some(error) => Err(error),
            None => Ok(self.summary),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ExitStatus;

    #[test]
    fn an_aborted_run_keeps_its_error_and_its_partial_counts() {
        let mut summary = Summary::default();
        summary.counts(ObjectType::Mailbox);
        let outcome = RunOutcome {
            summary,
            error: Some(Error::Connection("http status 404".to_owned())),
        };
        assert_eq!(outcome.summary.per_type.len(), 1);
        let err = outcome.into_result().expect_err("aborted");
        assert_eq!(err.exit_status(), ExitStatus::Connection);
    }

    #[test]
    fn counts_are_insertion_ordered_and_reused() {
        let mut summary = Summary::default();
        summary.counts(ObjectType::Mailbox).created += 1;
        summary.counts(ObjectType::Email).failed += 2;
        summary.counts(SummaryType::VacationResponse).fetched += 1;
        summary.counts(ObjectType::Mailbox).created += 1;
        let types: Vec<SummaryType> = summary.per_type.iter().map(|(t, _)| *t).collect();
        assert_eq!(
            types,
            [
                SummaryType::Object(ObjectType::Mailbox),
                SummaryType::Object(ObjectType::Email),
                SummaryType::VacationResponse
            ]
        );
        assert_eq!(summary.get(ObjectType::Mailbox).map(|c| c.created), Some(2));
        assert!(summary.get(ObjectType::Calendar).is_none());
        assert!(summary.any_failed());
        assert_eq!(summary.failed(), 2);
        summary.set_retry_stats(RetryStats {
            retries_observed: 4,
            retry_after_sleeps: 1,
        });
        assert_eq!(
            (summary.retries_observed, summary.retry_after_sleeps),
            (4, 1)
        );
    }

    // Plan rows are kept apart from the run counts, one row per type.
    #[test]
    fn plan_rows_are_separate_from_counts() {
        let mut summary = Summary::default();
        summary.plan(ObjectType::Email).new = 3;
        summary.plan(ObjectType::Email).present += 2;
        assert!(summary.per_type.is_empty());
        assert_eq!(
            summary.plan,
            [(
                SummaryType::Object(ObjectType::Email),
                PlanCounts {
                    new: 3,
                    vanished: 0,
                    present: 2
                }
            )]
        );
    }

    // Labels and nouns come from the object type, with the vacation response added.
    #[test]
    fn summary_type_vocabulary() {
        let event = SummaryType::from(ObjectType::CalendarEvent);
        assert_eq!(event.label(), "Calendar events");
        assert_eq!(event.noun(), "calendar event");
        assert_eq!(SummaryType::VacationResponse.label(), "Vacation response");
        assert_eq!(SummaryType::VacationResponse.noun(), "vacation response");
        assert!(event == ObjectType::CalendarEvent);
        assert!(SummaryType::VacationResponse != ObjectType::SieveScript);
    }

    #[test]
    fn a_result_without_a_native_outcome_reports_nothing_on_abort() {
        let outcome = RunOutcome::failed(Error::Partial("one object".to_owned()));
        assert!(outcome.summary.per_type.is_empty());
        assert!(outcome.error.is_some());
    }
}
