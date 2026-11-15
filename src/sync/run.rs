/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::archive::sources::{SourceChange, SourceKey, SourceKind};
use crate::archive::{Archive, SourceId, Store};
use crate::error::Error;
use crate::net::http::RetryStats;
use crate::sync::{RunMode, RunOptions, RunOutcome, Summary, SummaryType, TypeCounts};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceClaim {
    Registered(SourceId),
    DryRun(Option<SourceId>),
}

impl SourceClaim {
    pub fn id(&self) -> Option<SourceId> {
        match *self {
            SourceClaim::Registered(id) => Some(id),
            SourceClaim::DryRun(id) => id,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SourceLabel<'a> {
    pub name: Option<&'a str>,
    pub username: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcFailure {
    Abort,
    Warn,
    WarnAsPartial,
}

pub struct ImportRun {
    archive: Archive,
    claim: SourceClaim,
    options: RunOptions,
    summary: Summary,
}

impl ImportRun {
    pub fn start(
        options: RunOptions,
        key: &SourceKey<'_>,
        label: SourceLabel<'_>,
        change: SourceChange,
    ) -> Result<ImportRun, Error> {
        let archive = Archive::open(&options.archive)?;
        ImportRun::start_on(archive, options, key, label, change)
    }

    pub fn start_on(
        archive: Archive,
        options: RunOptions,
        key: &SourceKey<'_>,
        label: SourceLabel<'_>,
        change: SourceChange,
    ) -> Result<ImportRun, Error> {
        let sources = archive.store().sources();
        let sql_error = |e| ImportRun::source_sql_error(key.kind, e);
        if change == SourceChange::Reject
            && let Some((url, account)) = sources.conflicting(key).map_err(sql_error)?
        {
            return Err(ImportRun::source_changed(key.kind, &url, &account));
        }
        let claim = match options.mode {
            RunMode::Apply => SourceClaim::Registered(
                sources
                    .upsert(key, label.name, label.username)
                    .map_err(sql_error)?,
            ),
            RunMode::DryRun => SourceClaim::DryRun(sources.find(key).map_err(sql_error)?),
        };
        Ok(ImportRun {
            archive,
            claim,
            options,
            summary: Summary::default(),
        })
    }

    #[cold]
    fn source_sql_error(kind: SourceKind, e: rusqlite::Error) -> Error {
        match kind {
            SourceKind::Jmap | SourceKind::CalDav | SourceKind::CardDav | SourceKind::WebDav => {
                Error::partial(e)
            }
            SourceKind::Imap
            | SourceKind::Maildir
            | SourceKind::ManageSieve
            | SourceKind::Takeout
            | SourceKind::ExchangeEws
            | SourceKind::ExchangeGraph => Error::from(e),
        }
    }

    #[cold]
    fn source_changed(kind: SourceKind, url: &str, account: &str) -> Error {
        Error::SourceChange(match kind {
            SourceKind::Imap
            | SourceKind::Maildir
            | SourceKind::ManageSieve
            | SourceKind::Takeout => {
                format!(
                    "archive already records {kind} source {url} / {account}; \
                     re-run with --allow-source-change or use a fresh archive"
                )
            }
            SourceKind::CalDav | SourceKind::CardDav | SourceKind::WebDav => format!(
                "archive already records {kind} source ({url}, account {account}); \
                 pass --allow-source-change to import a different account, \
                 or --replace-source to drop the recorded one"
            ),
            SourceKind::Jmap => format!(
                "archive already records JMAP source ({url}, account {account}); \
                 pass --allow-source-change to import a different account"
            ),
            SourceKind::ExchangeEws | SourceKind::ExchangeGraph => format!(
                "archive already records {kind} source ({url}, account {account}); \
                 pass --allow-source-change to import a different account"
            ),
        })
    }

    pub fn archive(&mut self) -> &mut Archive {
        &mut self.archive
    }

    pub fn store(&self) -> Store<'_> {
        self.archive.store()
    }

    pub fn claim(&self) -> &SourceClaim {
        &self.claim
    }

    pub fn options(&self) -> &RunOptions {
        &self.options
    }

    pub fn summary(&mut self) -> &mut Summary {
        &mut self.summary
    }

    pub fn counts(&mut self, ty: impl Into<SummaryType>) -> &mut TypeCounts {
        self.summary.counts(ty)
    }

    pub fn finish(mut self, stats: RetryStats, gc: GcFailure) -> RunOutcome {
        self.summary.set_retry_stats(stats);
        let mut error = None;
        if self.options.mode == RunMode::Apply
            && !self.summary.any_failed()
            && let Err(e) = self.collect_garbage()
        {
            match gc {
                GcFailure::Abort => error = Some(Error::from(e)),
                GcFailure::Warn => self
                    .options
                    .logger
                    .warn(format_args!("blob GC skipped: {}", Error::from(e))),
                GcFailure::WarnAsPartial => self
                    .options
                    .logger
                    .warn(format_args!("blob GC skipped: {}", Error::partial(e))),
            }
        }
        RunOutcome {
            summary: self.summary,
            error,
        }
    }

    pub fn abort(mut self, error: Error, stats: RetryStats) -> RunOutcome {
        self.summary.set_retry_stats(stats);
        RunOutcome {
            summary: self.summary,
            error: Some(error),
        }
    }

    fn collect_garbage(&self) -> rusqlite::Result<usize> {
        let tx = self.archive.store().begin()?;
        let removed = tx.store().blobs().gc_orphans()?;
        tx.commit()?;
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::error::ExitStatus;
    use crate::logging::{Level, Logger};
    use crate::net::http::TlsPolicy;
    use crate::types::ObjectType;

    fn options(mode: RunMode) -> RunOptions {
        RunOptions {
            archive: PathBuf::new(),
            threads: 1,
            mode,
            max_retries: 0,
            tls: TlsPolicy::Verify,
            logger: Logger::new(Level::Quiet),
        }
    }

    fn key(kind: SourceKind, account_id: &str) -> SourceKey<'_> {
        SourceKey {
            kind,
            url: "imap://mail.example.com",
            account_id,
        }
    }

    const LABEL: SourceLabel<'static> = SourceLabel {
        name: Some("alice"),
        username: "alice",
    };

    fn blob_rows(archive: &Archive) -> i64 {
        archive
            .connection()
            .query_row("SELECT count(*) FROM blobs", [], |r| r.get(0))
            .unwrap()
    }

    fn seeded(kind: SourceKind) -> Archive {
        let archive = Archive::open_in_memory().unwrap();
        archive
            .store()
            .sources()
            .upsert(&key(kind, "alice"), Some("alice"), "alice")
            .unwrap();
        archive
    }

    #[test]
    fn apply_registers_the_source() {
        let run = ImportRun::start_on(
            Archive::open_in_memory().unwrap(),
            options(RunMode::Apply),
            &key(SourceKind::Imap, "alice"),
            LABEL,
            SourceChange::Reject,
        )
        .unwrap();
        assert!(matches!(run.claim(), SourceClaim::Registered(_)));
        assert!(run.claim().id().is_some());
    }

    #[test]
    fn dry_run_finds_without_registering() {
        let run = ImportRun::start_on(
            Archive::open_in_memory().unwrap(),
            options(RunMode::DryRun),
            &key(SourceKind::Imap, "alice"),
            LABEL,
            SourceChange::Reject,
        )
        .unwrap();
        assert_eq!(*run.claim(), SourceClaim::DryRun(None));
        let run = ImportRun::start_on(
            seeded(SourceKind::Imap),
            options(RunMode::DryRun),
            &key(SourceKind::Imap, "alice"),
            LABEL,
            SourceChange::Reject,
        )
        .unwrap();
        assert!(matches!(run.claim(), SourceClaim::DryRun(Some(_))));
    }

    #[test]
    fn guard_rejects_another_account_in_both_modes() {
        for mode in [RunMode::Apply, RunMode::DryRun] {
            let err = ImportRun::start_on(
                seeded(SourceKind::Imap),
                options(mode),
                &key(SourceKind::Imap, "bob"),
                LABEL,
                SourceChange::Reject,
            )
            .err()
            .unwrap();
            assert_eq!(err.exit_status(), ExitStatus::SourceChange, "{mode:?}");
        }
    }

    // a failing source query keeps each protocol's old exit class
    #[test]
    fn source_sql_failure_keeps_the_exit_class_per_kind() {
        for (kind, status) in [
            (SourceKind::Jmap, ExitStatus::Partial),
            (SourceKind::CardDav, ExitStatus::Partial),
            (SourceKind::Imap, ExitStatus::Archive),
        ] {
            for (mode, change) in [
                (RunMode::Apply, SourceChange::Reject),
                (RunMode::Apply, SourceChange::Allow),
                (RunMode::DryRun, SourceChange::Allow),
            ] {
                let archive = Archive::open_in_memory().unwrap();
                archive
                    .connection()
                    .execute_batch("PRAGMA foreign_keys = OFF; DROP TABLE sources;")
                    .unwrap();
                let err =
                    ImportRun::start_on(archive, options(mode), &key(kind, "alice"), LABEL, change)
                        .err()
                        .unwrap();
                assert_eq!(err.exit_status(), status, "{kind:?} {mode:?} {change:?}");
            }
        }
    }

    #[test]
    fn guard_allows_when_asked() {
        for change in [SourceChange::Allow, SourceChange::Replace] {
            assert!(
                ImportRun::start_on(
                    seeded(SourceKind::CalDav),
                    options(RunMode::Apply),
                    &key(SourceKind::CalDav, "bob"),
                    LABEL,
                    change,
                )
                .is_ok()
            );
        }
    }

    #[test]
    fn guard_messages_are_unchanged_per_kind() {
        let text = |kind| ImportRun::source_changed(kind, "u", "a").to_string();
        assert_eq!(
            text(SourceKind::Imap),
            "source-change protection: archive already records imap source u / a; \
             re-run with --allow-source-change or use a fresh archive"
        );
        assert_eq!(
            text(SourceKind::Jmap),
            "source-change protection: archive already records JMAP source (u, account a); \
             pass --allow-source-change to import a different account"
        );
        assert_eq!(
            text(SourceKind::ExchangeGraph),
            "source-change protection: archive already records exchange_graph source (u, account a); \
             pass --allow-source-change to import a different account"
        );
        assert_eq!(
            text(SourceKind::CardDav),
            "source-change protection: archive already records carddav source (u, account a); \
             pass --allow-source-change to import a different account, \
             or --replace-source to drop the recorded one"
        );
    }

    #[test]
    fn finish_collects_garbage_only_after_a_clean_apply() {
        for (mode, failed, left) in [
            (RunMode::Apply, 0, 0),
            (RunMode::Apply, 1, 1),
            (RunMode::DryRun, 0, 1),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("archive.sqlite");
            Archive::open(&path)
                .unwrap()
                .store()
                .blobs()
                .intern(b"orphan")
                .unwrap();
            let mut opts = options(mode);
            opts.archive = path.clone();
            let mut run = ImportRun::start(
                opts,
                &key(SourceKind::Maildir, "alice"),
                LABEL,
                SourceChange::Reject,
            )
            .unwrap();
            run.counts(ObjectType::Email).failed = failed;
            let outcome = run.finish(
                RetryStats {
                    retries_observed: 2,
                    retry_after_sleeps: 1,
                },
                GcFailure::Abort,
            );
            assert!(outcome.error.is_none());
            assert_eq!(outcome.summary.retries_observed, 2);
            assert_eq!(
                blob_rows(&Archive::open(&path).unwrap()),
                left,
                "{mode:?} failed={failed}"
            );
        }
    }

    #[test]
    fn abort_keeps_counts() {
        let mut run = ImportRun::start_on(
            Archive::open_in_memory().unwrap(),
            options(RunMode::Apply),
            &key(SourceKind::Takeout, "alice"),
            LABEL,
            SourceChange::Reject,
        )
        .unwrap();
        run.counts(ObjectType::Mailbox).created = 3;
        let outcome = run.abort(Error::Connection("gone".to_owned()), RetryStats::default());
        assert_eq!(
            outcome.summary.get(ObjectType::Mailbox).map(|c| c.created),
            Some(3)
        );
        assert!(outcome.into_result().is_err());
    }
}
