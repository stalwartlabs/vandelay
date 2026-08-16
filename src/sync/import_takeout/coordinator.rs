/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};
use url::Url;

use crate::db;
use crate::db::sources::SourceKey;
use crate::db::takeout_ids;
use crate::error::Error;
use crate::logging::{LEVEL_DEFAULT, LEVEL_PROGRESS, Logger};
use crate::sync::{CommonConfig, RunOutcome, Summary, TypeCounts};

use super::calendar;
use super::contacts;
use super::labels::MappingOptions;
use super::mail;
use super::walk::{self, FileKind};

#[derive(Debug)]
pub struct TakeoutImportConfig {
    pub takeout_root: PathBuf,
    pub allow_source_change: bool,
    pub automap: bool,
}

pub fn run(common: CommonConfig, config: TakeoutImportConfig) -> Result<Summary, Error> {
    run_reporting(common, config).into_result()
}

pub fn run_reporting(common: CommonConfig, config: TakeoutImportConfig) -> RunOutcome {
    let mut summary = Summary::default();
    let error = run_into(common, config, &mut summary).err();
    RunOutcome { summary, error }
}

fn run_into(
    common: CommonConfig,
    config: TakeoutImportConfig,
    summary: &mut Summary,
) -> Result<(), Error> {
    let logger = common.logger;
    if common.threads > 1 && logger.enabled(LEVEL_PROGRESS) {
        eprintln!("takeout importer is single-threaded; --threads value will be ignored");
    }

    let canonical = std::fs::canonicalize(&config.takeout_root)
        .map_err(|e| Error::Usage(format!("--path {:?}: {e}", config.takeout_root)))?;
    if !canonical.is_dir() {
        return Err(Error::Usage(format!("not a directory: {canonical:?}")));
    }

    let walk_result = walk::walk_with_logger(&canonical, logger)
        .map_err(|e| Error::Usage(format!("walking {canonical:?}: {e}")))?;
    if walk_result.is_empty() {
        return Err(Error::Usage(format!(
            "{canonical:?}: no .mbox / .ics / .vcf files found under this path"
        )));
    }

    if logger.enabled(LEVEL_PROGRESS) {
        let mbox_n = walk_result.by_kind(FileKind::Mbox).count();
        let ics_n = walk_result.by_kind(FileKind::Ics).count();
        let vcf_n = walk_result.by_kind(FileKind::Vcf).count();
        eprintln!(
            "takeout discovery: {} .mbox, {} .ics, {} .vcf (io failures: {}, symlink cycles: {})",
            mbox_n, ics_n, vcf_n, walk_result.io_failures, walk_result.symlink_cycles
        );
    }

    let mut conn = db::init::open(&common.archive)?;
    let session_url = file_url_for(&canonical)?;
    let account_id = canonical.to_string_lossy().into_owned();
    let account_name = canonical
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_owned);

    if let Some((existing_url, existing_account)) =
        db::sources::conflicting_source(&conn, "takeout", &session_url, &account_id)?
        && !config.allow_source_change
    {
        return Err(Error::SourceChange(format!(
            "archive already records takeout source {existing_url} / {existing_account}; \
             re-run with --allow-source-change or use a fresh archive"
        )));
    }

    let source_key = SourceKey {
        kind: "takeout".to_owned(),
        session_url: session_url.clone(),
        account_id: account_id.clone(),
    };
    let source_id = if common.dry_run {
        db::sources::find_source(&conn, &source_key)?.unwrap_or(-1)
    } else {
        db::sources::upsert_source(&conn, &source_key, account_name.as_deref(), "")?
    };

    if common.dry_run {
        *summary = dry_run_summary(&conn, source_id, &walk_result, logger);
        return Ok(());
    }

    let options = MappingOptions {
        automap: config.automap,
    };

    let mut mailbox_counts = TypeCounts::default();
    let mut email_counts = TypeCounts::default();
    let mut calendar_counts = TypeCounts::default();
    let mut calendar_event_counts = TypeCounts::default();
    let mut book_counts = TypeCounts::default();
    let mut card_counts = TypeCounts::default();

    let mut mailbox_cache: HashMap<String, i64> =
        takeout_ids::all_for_type(&conn, source_id, takeout_ids::MAILBOX)?;

    let mut aborted: Option<Error> = None;
    if let Err(e) = process_mbox_files(
        &mut conn,
        source_id,
        &walk_result,
        options,
        &mut mailbox_cache,
        &mut mailbox_counts,
        &mut email_counts,
        logger,
    ) {
        aborted = Some(e);
    }

    if aborted.is_none()
        && let Err(e) = process_ics_files(
            &mut conn,
            source_id,
            &walk_result,
            &mut calendar_counts,
            &mut calendar_event_counts,
            logger,
        )
    {
        aborted = Some(e);
    }

    if aborted.is_none()
        && let Err(e) = process_vcf_files(
            &mut conn,
            source_id,
            &walk_result,
            &mut book_counts,
            &mut card_counts,
            logger,
        )
    {
        aborted = Some(e);
    }

    let no_failures = aborted.is_none()
        && mailbox_counts.failed == 0
        && email_counts.failed == 0
        && calendar_counts.failed == 0
        && calendar_event_counts.failed == 0
        && book_counts.failed == 0
        && card_counts.failed == 0;
    if no_failures {
        let tx = conn.unchecked_transaction()?;
        db::blobs::gc_orphan_blobs(&tx)?;
        tx.commit()?;
    }

    if logger.enabled(LEVEL_DEFAULT) {
        eprintln!(
            "takeout: mailbox c={} f={} | email c={} u={} f={} | calendar c={} f={} | \
             event c={} u={} f={} | addressbook c={} | card c={} u={} f={}",
            mailbox_counts.created,
            mailbox_counts.failed,
            email_counts.created,
            email_counts.fetched.saturating_sub(email_counts.created),
            email_counts.failed,
            calendar_counts.created,
            calendar_counts.failed,
            calendar_event_counts.created,
            calendar_event_counts
                .fetched
                .saturating_sub(calendar_event_counts.created),
            calendar_event_counts.failed,
            book_counts.created,
            card_counts.created,
            card_counts.fetched.saturating_sub(card_counts.created),
            card_counts.failed,
        );
    }

    *summary = Summary {
        per_type: vec![
            ("mailbox", mailbox_counts),
            ("email", email_counts),
            ("calendar", calendar_counts),
            ("calendarevent", calendar_event_counts),
            ("addressbook", book_counts),
            ("contactcard", card_counts),
        ],
        retries_observed: 0,
        retry_after_sleeps: 0,
    };
    match aborted {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[allow(clippy::too_many_arguments)]
fn process_mbox_files(
    conn: &mut Connection,
    source_id: i64,
    walk_result: &walk::WalkResult,
    options: MappingOptions,
    mailbox_cache: &mut HashMap<String, i64>,
    mailbox_counts: &mut TypeCounts,
    email_counts: &mut TypeCounts,
    logger: Logger,
) -> Result<(), Error> {
    for file in walk_result.by_kind(FileKind::Mbox) {
        let fallback = mailbox_name_from_filename(&file.path);
        if logger.enabled(LEVEL_PROGRESS) {
            eprintln!(
                "mbox: processing {:?} (fallback mailbox: {:?})",
                file.path, fallback
            );
        }
        let ctx = mail::InsertContext {
            source_id,
            fallback_mailbox: &fallback,
            options,
            mailbox_cache,
        };
        match mail::process_file(conn, &file.path, ctx, mailbox_counts, email_counts, logger) {
            Ok(()) => {}
            Err(e) if e.aborts_run() => return Err(e),
            Err(e) => {
                logger.warn(&format!("mbox {:?}: {e}", file.path));
                email_counts.failed += 1;
            }
        }
    }
    Ok(())
}

fn process_ics_files(
    conn: &mut Connection,
    source_id: i64,
    walk_result: &walk::WalkResult,
    calendar_counts: &mut TypeCounts,
    event_counts: &mut TypeCounts,
    logger: Logger,
) -> Result<(), Error> {
    let mut cache: HashMap<String, i64> =
        takeout_ids::all_for_type(conn, source_id, takeout_ids::CALENDAR)?;
    for file in walk_result.by_kind(FileKind::Ics) {
        if logger.enabled(LEVEL_PROGRESS) {
            eprintln!("ics: processing {:?}", file.path);
        }
        if let Err(e) = calendar::process_file(
            conn,
            &file.path,
            source_id,
            &mut cache,
            calendar_counts,
            event_counts,
            logger,
        ) {
            if e.aborts_run() {
                return Err(e);
            }
            logger.warn(&format!("ics {:?}: {e}", file.path));
            event_counts.failed += 1;
        }
    }
    Ok(())
}

fn process_vcf_files(
    conn: &mut Connection,
    source_id: i64,
    walk_result: &walk::WalkResult,
    book_counts: &mut TypeCounts,
    card_counts: &mut TypeCounts,
    logger: Logger,
) -> Result<(), Error> {
    let mut book_local: Option<i64> =
        takeout_ids::local_for(conn, source_id, takeout_ids::ADDRESS_BOOK, "Imported")?;
    for file in walk_result.by_kind(FileKind::Vcf) {
        if logger.enabled(LEVEL_PROGRESS) {
            eprintln!("vcf: processing {:?}", file.path);
        }
        if let Err(e) = contacts::process_file(
            conn,
            &file.path,
            source_id,
            &mut book_local,
            book_counts,
            card_counts,
            logger,
        ) {
            if e.aborts_run() {
                return Err(e);
            }
            logger.warn(&format!("vcf {:?}: {e}", file.path));
            card_counts.failed += 1;
        }
    }
    Ok(())
}

fn mailbox_name_from_filename(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| "Imported".to_owned())
}

fn file_url_for(p: &Path) -> Result<String, Error> {
    Url::from_file_path(p)
        .map(|u| u.to_string())
        .map_err(|_| Error::Usage(format!("cannot encode takeout path as file:// URL: {p:?}")))
}

fn dry_run_summary(
    conn: &Connection,
    source_id: i64,
    walk_result: &walk::WalkResult,
    logger: Logger,
) -> Summary {
    let mbox_n = walk_result.by_kind(FileKind::Mbox).count();
    let ics_n = walk_result.by_kind(FileKind::Ics).count();
    let vcf_n = walk_result.by_kind(FileKind::Vcf).count();
    let local_emails = if source_id < 0 {
        0
    } else {
        count_rows(conn, source_id, takeout_ids::EMAIL)
    };
    let local_events = if source_id < 0 {
        0
    } else {
        count_rows(conn, source_id, takeout_ids::CALENDAR_EVENT)
    };
    let local_cards = if source_id < 0 {
        0
    } else {
        count_rows(conn, source_id, takeout_ids::CONTACT_CARD)
    };
    if logger.enabled(LEVEL_DEFAULT) {
        eprintln!("dry-run: mbox files={mbox_n} ics files={ics_n} vcf files={vcf_n}");
        eprintln!(
            "  existing in archive: emails={local_emails} events={local_events} cards={local_cards}"
        );
    }
    Summary {
        per_type: vec![
            ("mailbox", TypeCounts::default()),
            ("email", TypeCounts::default()),
            ("calendar", TypeCounts::default()),
            ("calendarevent", TypeCounts::default()),
            ("addressbook", TypeCounts::default()),
            ("contactcard", TypeCounts::default()),
        ],
        retries_observed: 0,
        retry_after_sleeps: 0,
    }
}

fn count_rows(conn: &Connection, source_id: i64, type_name: &str) -> u64 {
    conn.query_row(
        "SELECT COUNT(*) FROM sync_id_takeout WHERE source_id = ?1 AND type_name = ?2",
        params![source_id, type_name],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_url_encodes_unicode_path() {
        let p = Path::new("/tmp/Año");
        let u = file_url_for(p).unwrap();
        assert!(u.starts_with("file://"));
        assert!(u.contains("A%C3%B1o"));
    }

    #[test]
    fn file_url_rejects_relative_path() {
        let err = file_url_for(Path::new("./relative")).unwrap_err();
        assert!(matches!(err, Error::Usage(_)));
    }

    #[test]
    fn mailbox_name_from_filename_strips_extension() {
        let p = Path::new("/some/dir/Github.mbox");
        assert_eq!(mailbox_name_from_filename(p), "Github");
    }

    #[test]
    fn mailbox_name_from_filename_handles_dotted_basename() {
        let p = Path::new("/x/All mail Including Spam and Trash.mbox");
        assert_eq!(
            mailbox_name_from_filename(p),
            "All mail Including Spam and Trash"
        );
    }
}
