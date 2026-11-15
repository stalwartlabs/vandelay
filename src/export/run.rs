/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;
use std::fmt;
use std::io::{self, IsTerminal, Write as _};
use std::mem;

use serde_json::{Value, json};

use super::target::Maps;
use super::{Confirm, Export, Exporter, Plan, Prune};
use crate::console::{Column, Counted, Initial, Table, Tone};
use crate::error::Error;
use crate::jmap::request::{Request, SetRequest, URN_VACATION_RESPONSE};
use crate::net::http::RetryStats;
use crate::sync::{PlanCounts, RunOutcome, Summary, SummaryType, TypeCounts};
use crate::types::ObjectType;

const PRUNE_SAMPLE: usize = 5;
const PRUNE_COLUMNS: [Column; 3] = [
    Column::left("Type"),
    Column::right("Candidates"),
    Column::left("Sample"),
];

struct PruneSample<'p>(&'p [String]);

impl fmt::Display for PruneSample<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (n, id) in self.0.iter().take(PRUNE_SAMPLE).enumerate() {
            if n > 0 {
                f.write_str(", ")?;
            }
            f.write_str(id)?;
        }
        if self.0.len() > PRUNE_SAMPLE {
            f.write_str(", ...")?;
        }
        Ok(())
    }
}

pub(super) struct ExportRun<'e, 'a> {
    exporter: &'e Exporter<'a>,
    config: &'e Export,
    work: Vec<ObjectType>,
    counts: HashMap<ObjectType, TypeCounts>,
    vacation: Option<TypeCounts>,
    plan: Vec<(SummaryType, PlanCounts)>,
}

impl<'e, 'a> ExportRun<'e, 'a> {
    pub(super) fn execute(exporter: &'e Exporter<'a>, config: &'e Export) -> RunOutcome {
        let mut run = ExportRun {
            exporter,
            config,
            work: exporter.work_list(config),
            counts: HashMap::new(),
            vacation: None,
            plan: Vec::new(),
        };
        let result = run.phases();
        let stats = exporter.account.client().stats();
        match result {
            Ok(()) if exporter.is_dry_run() => {
                let plan = mem::take(&mut run.plan);
                RunOutcome {
                    summary: Summary {
                        plan,
                        ..run.summary(RetryStats::default())
                    },
                    error: None,
                }
            }
            Ok(()) => RunOutcome {
                summary: run.summary(stats),
                error: None,
            },
            Err(e) => RunOutcome {
                summary: run.summary(stats),
                error: Some(e),
            },
        }
    }

    fn phases(&mut self) -> Result<(), Error> {
        let exporter = self.exporter;
        let logger = exporter.logger;
        let mut maps = Maps::default();
        let mut plans: HashMap<ObjectType, Plan> = HashMap::with_capacity(self.work.len());

        for &ty in &self.work {
            logger.info(format_args!("Exporting {}", Initial::Lower(ty.label())));
            let counts = self.counts.entry(ty).or_default();
            let plan = match exporter.reconcile(ty, &mut maps, counts) {
                Ok(plan) => {
                    if exporter.is_dry_run() {
                        self.plan.push((
                            ty.into(),
                            PlanCounts {
                                new: counts.created,
                                vanished: plan.prune_candidates.len() as u64,
                                present: counts.skipped,
                            },
                        ));
                    }
                    plan
                }
                Err(e) if e.aborts_run() => return Err(e),
                Err(e) => {
                    logger.warn(format_args!("{} aborted: {e}", Initial::Lower(ty.label())));
                    counts.failed += 1;
                    Plan::default()
                }
            };
            plans.insert(ty, plan);
        }

        if self.config.prune == Prune::Destroy {
            self.prune_phase(&plans)?;
        }

        if self.config.selects(ObjectType::SieveScript)
            && exporter
                .account
                .session()
                .may_support(exporter.account.account_id(), URN_VACATION_RESPONSE)
        {
            let mut counts = TypeCounts::default();
            let present = match exporter.reconcile_vacation(&mut counts) {
                Ok(present) => present,
                Err(e) if e.aborts_run() => {
                    self.vacation = Some(counts);
                    return Err(e);
                }
                Err(e) => {
                    logger.warn(format_args!(
                        "{} aborted: {e}",
                        SummaryType::VacationResponse.noun()
                    ));
                    counts.failed += 1;
                    true
                }
            };
            if present {
                if exporter.is_dry_run() {
                    self.plan.push((
                        SummaryType::VacationResponse,
                        PlanCounts {
                            new: counts.updated,
                            ..PlanCounts::default()
                        },
                    ));
                }
                self.vacation = Some(counts);
            }
        }
        Ok(())
    }

    fn summary(mut self, stats: RetryStats) -> Summary {
        let mut summary = Summary::default();
        for ty in &self.work {
            if let Some(counts) = self.counts.remove(ty) {
                summary.per_type.push(((*ty).into(), counts));
            }
        }
        if let Some(counts) = self.vacation {
            summary
                .per_type
                .push((SummaryType::VacationResponse, counts));
        }
        summary.set_retry_stats(stats);
        summary
    }

    fn prune_phase(&mut self, plans: &HashMap<ObjectType, Plan>) -> Result<(), Error> {
        let pending = || {
            self.work.iter().filter_map(|ty| {
                plans
                    .get(ty)
                    .filter(|p| !p.prune_candidates.is_empty())
                    .map(|p| (*ty, p))
            })
        };
        if pending().next().is_none() {
            return Ok(());
        }
        let styled = self.exporter.logger.console().err;
        let mut table = Table::new(&PRUNE_COLUMNS);
        let mut total = 0;
        for (ty, p) in pending() {
            total += p.prune_candidates.len() as u64;
            table
                .cell(ty.label(), None)
                .count(p.prune_candidates.len() as u64, Some(Tone::Bad))
                .cell(PruneSample(&p.prune_candidates), Some(Tone::Dim));
        }
        let total = Counted::new(total, "object", "objects");
        let mut err = io::stderr().lock();
        let _ = writeln!(
            err,
            "\n{}\n",
            styled.paint(
                Tone::Title,
                format_args!("Prune plan: {total} on the target with no match in the archive")
            )
        );
        let _ = table.write(styled, &mut err);
        drop(err);
        if self.exporter.is_dry_run() {
            return Ok(());
        }
        if self.config.confirm == Confirm::Prompt && io::stdin().is_terminal() {
            eprint!("Destroy {total} on the target? [y/N] ");
            let _ = io::stderr().flush();
            let mut line = String::new();
            io::stdin().read_line(&mut line).map_err(Error::partial)?;
            if !hashify::set!(line.trim().as_bytes(), "y", "Y", "yes") {
                return Err(Error::PruneAborted);
            }
        }
        for ty in self.work.iter().rev() {
            if let Some(plan) = plans.get(ty)
                && !plan.prune_candidates.is_empty()
                && let Some(counts) = self.counts.get_mut(ty)
            {
                self.exporter.destroy(*ty, plan, counts);
            }
        }
        Ok(())
    }
}

impl Exporter<'_> {
    fn destroy(&self, ty: ObjectType, plan: &Plan, counts: &mut TypeCounts) {
        if ty == ObjectType::SieveScript && plan.active_sieve_target.is_none() {
            self.deactivate_sieve_scripts();
        }
        let destroy = Value::Array(
            plan.prune_candidates
                .iter()
                .map(|s| Value::String(s.clone()))
                .collect(),
        );
        let extra = ty.destroy_contents_arg();
        let request = SetRequest {
            destroy: Some(destroy),
            extra_args: extra.as_slice(),
            ..Default::default()
        };
        match self.account.set(ty.jmap_name(), request) {
            Ok(outcome) => {
                counts.deleted += outcome.destroyed.len() as u64;
                for (id, err) in &outcome.not_destroyed {
                    self.logger.warn(format_args!(
                        "prune: {} {id} not destroyed: {err}",
                        ty.noun()
                    ));
                    counts.skipped += 1;
                }
            }
            Err(e) => {
                self.logger.warn(format_args!(
                    "prune: destroying {} failed: {e}",
                    Initial::Lower(ty.label())
                ));
                counts.failed += plan.prune_candidates.len() as u64;
            }
        }
    }

    fn deactivate_sieve_scripts(&self) {
        let mut req = Request::new();
        req.call(
            "SieveScript/set",
            json!({ "accountId": self.account.account_id(), "onSuccessDeactivateScript": true }),
            "d",
        );
        let outcome = self
            .account
            .send(&req)
            .and_then(|resp| resp.first()?.check_error());
        if let Err(e) = outcome {
            self.logger.warn(format_args!(
                "prune: deactivating sieve scripts before destroy failed: {e}"
            ));
        }
    }
}

impl ObjectType {
    fn destroy_contents_arg(self) -> Option<(&'static str, Value)> {
        let name = match self {
            ObjectType::AddressBook => "onDestroyRemoveContents",
            ObjectType::Calendar => "onDestroyRemoveEvents",
            ObjectType::FileNode => "onDestroyRemoveChildren",
            _ => return None,
        };
        Some((name, Value::Bool(false)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The sample shows the first five ids and marks the rest with an ellipsis.
    #[test]
    fn prune_sample_truncates_after_five() {
        let ids: Vec<String> = (1..=7).map(|n| format!("m{n}")).collect();
        assert_eq!(PruneSample(&ids).to_string(), "m1, m2, m3, m4, m5, ...");
        assert_eq!(PruneSample(&ids[..2]).to_string(), "m1, m2");
    }
}
