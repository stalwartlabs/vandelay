/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt;
use std::io::{self, Write};
use std::path::Path;

use crate::console::{Column, Grouped, Styled, Table, Tone};
use crate::export::Prune;
use crate::sync::{RunMode, Summary, TypeCounts};

const TOTAL: &str = "Total";
const IMPORT_PLAN_TITLE: &str = "Import plan (dry run, nothing written)";
const EXPORT_PLAN_TITLE: &str = "Export plan (dry run, nothing written)";
const IMPORT_COLUMNS: [Column; 7] = [
    Column::left("Type"),
    Column::right("Created"),
    Column::right("Fetched"),
    Column::right("Updated"),
    Column::right("Deleted"),
    Column::right("Skipped"),
    Column::right("Failed"),
];
const EXPORT_COLUMNS: [Column; 6] = [
    Column::left("Type"),
    Column::right("Created"),
    Column::right("Matched"),
    Column::right("Updated"),
    Column::right("Destroyed"),
    Column::right("Failed"),
];
const FAILURE_COLUMNS: [Column; 2] = [Column::left("Type"), Column::right("Failed")];
const IMPORT_PLAN_COLUMNS: [Column; 4] = [
    Column::left("Type"),
    Column::right("New"),
    Column::right("Vanished"),
    Column::right("Present"),
];
const EXPORT_PLAN_COLUMNS: [Column; 4] = [
    Column::left("Type"),
    Column::right("Create"),
    Column::right("Matched"),
    Column::right("Destroy"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RunKind {
    Import,
    Export(Prune),
}

pub(super) struct Report<'a> {
    pub kind: RunKind,
    pub archive: &'a Path,
    pub mode: RunMode,
    pub summary: &'a Summary,
}

impl Report<'_> {
    pub(super) fn write(&self, styled: Styled, out: &mut dyn Write) -> io::Result<()> {
        self.write_plan(styled, out)?;
        self.write_counts(styled, out)
    }

    fn write_plan(&self, styled: Styled, out: &mut dyn Write) -> io::Result<()> {
        let rows = &self.summary.plan;
        if rows.is_empty() {
            return Ok(());
        }
        let (title, columns) = match self.kind {
            RunKind::Import => (IMPORT_PLAN_TITLE, &IMPORT_PLAN_COLUMNS[..]),
            RunKind::Export(Prune::Destroy) => (EXPORT_PLAN_TITLE, &EXPORT_PLAN_COLUMNS[..]),
            RunKind::Export(Prune::Keep) => (
                EXPORT_PLAN_TITLE,
                &EXPORT_PLAN_COLUMNS[..EXPORT_PLAN_COLUMNS.len() - 1],
            ),
        };
        writeln!(out, "\n{}\n", styled.paint(Tone::Title, title))?;
        let mut table = Table::new(columns);
        for (ty, plan) in rows {
            table
                .cell(ty.label(), None)
                .count(plan.new, Some(Tone::Good));
            match self.kind {
                RunKind::Import => {
                    table
                        .count(plan.vanished, Some(Tone::Bad))
                        .count(plan.present, None);
                }
                RunKind::Export(prune) => {
                    table.count(plan.present, None);
                    if prune == Prune::Destroy {
                        table.count(plan.vanished, Some(Tone::Bad));
                    }
                }
            }
        }
        table.write(styled, out)
    }

    fn write_counts(&self, styled: Styled, out: &mut dyn Write) -> io::Result<()> {
        if self.mode == RunMode::DryRun {
            return self.write_failures(styled, out);
        }
        let rows = &self.summary.per_type;
        if rows.is_empty() {
            return Ok(());
        }
        writeln!(out, "\n{}\n", styled.paint(Tone::Title, Title(self)))?;
        let mut table = Table::new(match self.kind {
            RunKind::Import => &IMPORT_COLUMNS[..],
            RunKind::Export(_) => &EXPORT_COLUMNS[..],
        });
        let mut total = TypeCounts::default();
        for (ty, counts) in rows {
            table.cell(ty.label(), None);
            self.counts(&mut table, counts, None);
            total.add(counts);
        }
        if rows.len() > 1 {
            table.cell(TOTAL, Some(Tone::Heading));
            self.counts(&mut table, &total, Some(Tone::Heading));
        }
        table.write(styled, out)?;
        self.write_retries(out)
    }

    fn write_failures(&self, styled: Styled, out: &mut dyn Write) -> io::Result<()> {
        let mut failed = self
            .summary
            .per_type
            .iter()
            .filter(|(_, counts)| counts.failed > 0)
            .peekable();
        if failed.peek().is_none() {
            return Ok(());
        }
        writeln!(out, "\n{}\n", styled.paint(Tone::Title, Title(self)))?;
        let mut table = Table::new(&FAILURE_COLUMNS);
        for (ty, counts) in failed {
            table
                .cell(ty.label(), None)
                .count(counts.failed, Some(Tone::Bad));
        }
        table.write(styled, out)?;
        self.write_retries(out)
    }

    fn write_retries(&self, out: &mut dyn Write) -> io::Result<()> {
        let summary = self.summary;
        if summary.retries_observed > 0 {
            writeln!(
                out,
                "\n  Retries: {} ({} honoured Retry-After)",
                Grouped(summary.retries_observed),
                Grouped(summary.retry_after_sleeps)
            )?;
        }
        Ok(())
    }

    fn counts(&self, table: &mut Table<'_>, counts: &TypeCounts, tone: Option<Tone>) {
        match self.kind {
            RunKind::Import => table
                .count(counts.created, tone)
                .count(counts.fetched, tone)
                .count(counts.updated, tone)
                .count(counts.deleted, tone)
                .count(counts.skipped, tone),
            RunKind::Export(_) => table
                .count(counts.created, tone)
                .count(counts.skipped, tone)
                .count(counts.updated, tone)
                .count(counts.deleted, tone),
        }
        .count(counts.failed, Some(Tone::Bad));
    }
}

struct Title<'r, 'a>(&'r Report<'a>);

impl fmt::Display for Title<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let report = self.0;
        let verb = match (report.kind, report.mode) {
            (RunKind::Import, RunMode::Apply) => "Imported into",
            (RunKind::Import, RunMode::DryRun) => "Dry run: import into",
            (RunKind::Export(_), RunMode::Apply) => "Exported from",
            (RunKind::Export(_), RunMode::DryRun) => "Dry run: export from",
        };
        write!(f, "{verb} {}", report.archive.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::{PlanCounts, SummaryType};
    use crate::types::ObjectType;

    fn render(kind: RunKind, mode: RunMode, summary: &Summary) -> String {
        let mut out = Vec::new();
        Report {
            kind,
            archive: Path::new("a.sqlite"),
            mode,
            summary,
        }
        .write(Styled::Plain, &mut out)
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    // An import summary has a title, one row per type in run order and a total row.
    #[test]
    fn import_summary_lists_types_and_total() {
        let mut summary = Summary::default();
        *summary.counts(ObjectType::Mailbox) = TypeCounts {
            created: 12,
            fetched: 12,
            ..TypeCounts::default()
        };
        *summary.counts(ObjectType::Email) = TypeCounts {
            created: 4210,
            fetched: 4210,
            failed: 2,
            ..TypeCounts::default()
        };
        summary.retries_observed = 3;
        summary.retry_after_sleeps = 2;
        assert_eq!(
            render(RunKind::Import, RunMode::Apply, &summary),
            "\nImported into a.sqlite\n\n\
             \x20 Type       Created  Fetched  Updated  Deleted  Skipped  Failed\n\
             \x20 Mailboxes       12       12        0        0        0       0\n\
             \x20 Emails       4,210    4,210        0        0        0       2\n\
             \x20 Total        4,222    4,222        0        0        0       2\n\
             \n  Retries: 3 (2 honoured Retry-After)\n"
        );
    }

    // Export maps matches to skipped and destroys to deleted; one type has no total row.
    #[test]
    fn export_summary_uses_export_columns() {
        let mut summary = Summary::default();
        *summary.counts(SummaryType::VacationResponse) = TypeCounts {
            updated: 1,
            skipped: 4,
            deleted: 5,
            ..TypeCounts::default()
        };
        assert_eq!(
            render(RunKind::Export(Prune::Keep), RunMode::Apply, &summary),
            "\nExported from a.sqlite\n\n\
             \x20 Type               Created  Matched  Updated  Destroyed  Failed\n\
             \x20 Vacation response        0        4        1          5       0\n"
        );
    }

    // Dry-run plans use the import or export columns; Destroy needs --prune.
    #[test]
    fn plan_columns_follow_direction_and_prune() {
        let mut summary = Summary::default();
        *summary.plan(ObjectType::Calendar) = PlanCounts {
            new: 1,
            vanished: 2,
            present: 3,
        };
        assert_eq!(
            render(RunKind::Import, RunMode::DryRun, &summary),
            "\nImport plan (dry run, nothing written)\n\n\
             \x20 Type       New  Vanished  Present\n\
             \x20 Calendars    1         2        3\n"
        );
        assert_eq!(
            render(RunKind::Export(Prune::Destroy), RunMode::DryRun, &summary),
            "\nExport plan (dry run, nothing written)\n\n\
             \x20 Type       Create  Matched  Destroy\n\
             \x20 Calendars       1        3        2\n"
        );
        assert_eq!(
            render(RunKind::Export(Prune::Keep), RunMode::DryRun, &summary),
            "\nExport plan (dry run, nothing written)\n\n\
             \x20 Type       Create  Matched\n\
             \x20 Calendars       1        3\n"
        );
    }

    // A dry run lists only the types that failed, after the plan.
    #[test]
    fn dry_run_counts_show_failures_only() {
        let mut summary = Summary::default();
        summary.counts(ObjectType::Mailbox).created = 3;
        assert!(render(RunKind::Import, RunMode::DryRun, &summary).is_empty());
        summary.counts(ObjectType::Email).failed = 2;
        assert_eq!(
            render(RunKind::Import, RunMode::DryRun, &summary),
            "\nDry run: import into a.sqlite\n\n\
             \x20 Type    Failed\n\
             \x20 Emails       2\n"
        );
        assert!(render(RunKind::Import, RunMode::Apply, &Summary::default()).is_empty());
    }
}
