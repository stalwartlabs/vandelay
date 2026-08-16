/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};
use url::Url;

use crate::db;
use crate::db::sources::SourceKey;
use crate::error::Error;
use crate::logging::{LEVEL_DEFAULT, LEVEL_PROGRESS, Logger};
use crate::sync::{CommonConfig, RunOutcome, Summary, TypeCounts};

use super::messages;
use super::tree;
use super::tree::{DiscoverError, FolderFilters, ResolvedFolder};

pub struct MaildirImportConfig {
    pub maildir: PathBuf,
    pub include: Vec<Regex>,
    pub exclude: Vec<Regex>,
    pub folder: Vec<String>,
    pub automap: bool,
    pub include_deleted: bool,
    pub allow_source_change: bool,
}

pub fn run(common: CommonConfig, config: MaildirImportConfig) -> Result<Summary, Error> {
    run_reporting(common, config).into_result()
}

pub fn run_reporting(common: CommonConfig, config: MaildirImportConfig) -> RunOutcome {
    let mut summary = Summary::default();
    let error = run_into(common, config, &mut summary).err();
    RunOutcome { summary, error }
}

fn run_into(
    common: CommonConfig,
    config: MaildirImportConfig,
    summary: &mut Summary,
) -> Result<(), Error> {
    let logger = common.logger;
    if common.threads > 1 {
        log_at(
            logger,
            LEVEL_PROGRESS,
            "maildir importer is single-threaded; --threads value will be ignored",
        );
    }

    let canonical = std::fs::canonicalize(&config.maildir)
        .map_err(|e| Error::Usage(format!("--maildir {:?}: {e}", config.maildir)))?;

    warn_if_looks_like_subfolder(&canonical, logger);

    let discovered = match tree::discover(&canonical, config.automap) {
        Ok(v) => v,
        Err(DiscoverError::NotFound(p)) => {
            return Err(Error::Usage(format!("maildir path does not exist: {p:?}")));
        }
        Err(DiscoverError::NotADirectory(p)) => {
            return Err(Error::Usage(format!("not a directory: {p:?}")));
        }
        Err(DiscoverError::NotAMaildir(p)) => {
            return Err(Error::Usage(format!(
                "not a Maildir: {p:?} (missing cur/ subdirectory)"
            )));
        }
        Err(DiscoverError::NotMaildirPlus(name)) => {
            return Err(Error::Usage(format!(
                "only Maildir++ layout is supported; found non-prefixed subfolder {name:?} \
                 (subfolders must be named with a leading '.', e.g. '.{name}')"
            )));
        }
        Err(DiscoverError::Io(p, e)) => {
            return Err(Error::Usage(format!("walking {p:?}: {e}")));
        }
    };

    let include_deleted = config.include_deleted;
    let filters = FolderFilters {
        include: config.include,
        exclude: config.exclude,
        explicit: config.folder,
    };
    if !filters.explicit.is_empty() && (!filters.include.is_empty() || !filters.exclude.is_empty())
    {
        return Err(Error::Usage(
            "--folder is mutually exclusive with --include/--exclude".to_owned(),
        ));
    }
    let mut resolved = tree::apply_filters(discovered, &filters);
    tree::restore_ephemeral_parents(&mut resolved, &canonical);
    let run_flags = RunFlags { include_deleted };

    let mut conn = db::init::open(&common.archive)?;
    let session_url = file_url_for(&canonical)?;
    let account_id = canonical.to_string_lossy().into_owned();
    let account_name = canonical
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_owned);

    if let Some((existing_url, existing_account)) =
        db::sources::conflicting_source(&conn, "maildir", &session_url, &account_id)?
        && !config.allow_source_change
    {
        return Err(Error::SourceChange(format!(
            "archive already records maildir source {existing_url} / {existing_account}; \
             re-run with --allow-source-change or use a fresh archive"
        )));
    }
    let source_key = SourceKey {
        kind: "maildir".to_owned(),
        session_url: session_url.clone(),
        account_id: account_id.clone(),
    };
    let source_id = if common.dry_run {
        db::sources::find_source(&conn, &source_key)?.unwrap_or(-1)
    } else {
        db::sources::upsert_source(&conn, &source_key, account_name.as_deref(), "")?
    };

    log_at(
        logger,
        LEVEL_PROGRESS,
        &format!("maildir source {session_url}"),
    );

    if common.dry_run {
        *summary = build_dry_run_summary(&conn, source_id, &resolved, logger)?;
        return Ok(());
    }

    let mut mailbox_counts = TypeCounts::default();
    let mut email_counts = TypeCounts::default();

    let local_mailboxes = db::maildir_ids::mailbox_folders(&conn, source_id)?;
    let mailbox_local_ids = upsert_mailboxes(&mut conn, source_id, &resolved, &mut mailbox_counts)?;

    let server_names: HashSet<&str> = resolved.iter().map(|f| f.name.as_str()).collect();
    let mut vanished: Vec<String> = local_mailboxes
        .keys()
        .filter(|n| !server_names.contains(n.as_str()))
        .cloned()
        .collect();
    tree::vanished_depth_sort(&mut vanished);
    if !vanished.is_empty() {
        delete_vanished_folders(
            &mut conn,
            source_id,
            &vanished,
            &mut mailbox_counts,
            &mut email_counts,
            logger,
        )?;
    }

    for folder in &resolved {
        if folder.ephemeral {
            continue;
        }
        let mailbox_local = match mailbox_local_ids.get(&folder.name).copied() {
            Some(id) => id,
            None => {
                log_at(
                    logger,
                    LEVEL_DEFAULT,
                    &format!("folder {:?}: missing from mailbox id map", folder.name),
                );
                email_counts.failed += 1;
                continue;
            }
        };
        if let Err(e) = reconcile_folder(
            &mut conn,
            source_id,
            folder,
            mailbox_local,
            &run_flags,
            &mut email_counts,
            logger,
        ) {
            if e.aborts_run() {
                *summary = Summary {
                    per_type: vec![("mailbox", mailbox_counts), ("email", email_counts)],
                    retries_observed: 0,
                    retry_after_sleeps: 0,
                };
                return Err(e);
            }
            log_at(
                logger,
                LEVEL_DEFAULT,
                &format!("folder {:?}: {e}", folder.name),
            );
            email_counts.failed += 1;
        }
    }

    if email_counts.failed == 0 && mailbox_counts.failed == 0 {
        let tx = conn.unchecked_transaction()?;
        db::blobs::gc_orphan_blobs(&tx)?;
        tx.commit()?;
    }

    log_at(
        logger,
        LEVEL_DEFAULT,
        &format!(
            "maildir: mailbox created={} deleted={} skipped={} failed={}; \
             email created={} fetched={} deleted={} skipped={} failed={}",
            mailbox_counts.created,
            mailbox_counts.deleted,
            mailbox_counts.skipped,
            mailbox_counts.failed,
            email_counts.created,
            email_counts.fetched,
            email_counts.deleted,
            email_counts.skipped,
            email_counts.failed
        ),
    );

    *summary = Summary {
        per_type: vec![("mailbox", mailbox_counts), ("email", email_counts)],
        retries_observed: 0,
        retry_after_sleeps: 0,
    };
    Ok(())
}

struct RunFlags {
    include_deleted: bool,
}

fn file_url_for(p: &Path) -> Result<String, Error> {
    Url::from_file_path(p).map(|u| u.to_string()).map_err(|_| {
        Error::Usage(format!(
            "cannot encode maildir path as a file:// URL: {p:?}"
        ))
    })
}

fn warn_if_looks_like_subfolder(canonical: &Path, logger: Logger) {
    let Some(basename) = canonical.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    if !basename.starts_with('.') {
        return;
    }
    let Some(parent) = canonical.parent() else {
        return;
    };
    if !parent.join("cur").is_dir() {
        return;
    }
    log_at(
        logger,
        LEVEL_DEFAULT,
        &format!(
            "warning: {canonical:?} looks like a Maildir++ subfolder of {parent:?} \
             (basename starts with '.' and the parent has its own cur/); proceeding, \
             but you may have meant to import the parent"
        ),
    );
}

fn upsert_mailboxes(
    conn: &mut Connection,
    source_id: i64,
    folders: &[ResolvedFolder],
    counts: &mut TypeCounts,
) -> Result<HashMap<String, i64>, Error> {
    let tx = conn.transaction()?;
    let mut local_ids: HashMap<String, i64> = HashMap::new();
    for folder in folders {
        let parent_local = match folder.parent_path.as_deref() {
            Some(p) => local_ids.get(p).copied().or_else(|| {
                db::maildir_ids::local_for_mailbox(&tx, source_id, p)
                    .ok()
                    .flatten()
            }),
            None => None,
        };
        let is_subscribed: i64 = if folder.ephemeral { 0 } else { 1 };
        let existing = db::maildir_ids::local_for_mailbox(&tx, source_id, &folder.name)?;
        let id = if let Some(id) = existing {
            tx.execute(
                "UPDATE mailboxes SET name = ?1, parent_id = ?2, role = ?3, is_subscribed = ?4
                 WHERE id = ?5",
                params![folder.leaf, parent_local, folder.role, is_subscribed, id],
            )?;
            id
        } else {
            tx.execute(
                "INSERT INTO mailboxes (name, parent_id, role, sort_order, is_subscribed)
                 VALUES (?1, ?2, ?3, 0, ?4)",
                params![folder.leaf, parent_local, folder.role, is_subscribed],
            )?;
            let new_id = tx.last_insert_rowid();
            db::maildir_ids::insert_mailbox(&tx, source_id, &folder.name, new_id)?;
            counts.created += 1;
            counts.fetched += 1;
            new_id
        };
        local_ids.insert(folder.name.clone(), id);
    }
    tx.commit()?;
    Ok(local_ids)
}

fn delete_vanished_folders(
    conn: &mut Connection,
    source_id: i64,
    folders: &[String],
    mailbox_counts: &mut TypeCounts,
    email_counts: &mut TypeCounts,
    logger: Logger,
) -> Result<(), Error> {
    let tx = conn.transaction()?;
    for name in folders {
        let local_id = match db::maildir_ids::local_for_mailbox(&tx, source_id, name)? {
            Some(id) => id,
            None => continue,
        };
        let surviving_child: Option<i64> = tx
            .query_row(
                "SELECT id FROM mailboxes WHERE parent_id = ?1 LIMIT 1",
                params![local_id],
                |row| row.get(0),
            )
            .optional()?;
        if surviving_child.is_some() {
            log_at(
                logger,
                LEVEL_DEFAULT,
                &format!(
                    "folder {name:?}: vanished from disk but still has child mailboxes in the \
                     archive; skipping delete (parent_id RESTRICT)"
                ),
            );
            mailbox_counts.failed += 1;
            continue;
        }
        let email_ids: Vec<i64> = tx
            .prepare(
                "SELECT local_id FROM sync_id_maildir
                 WHERE source_id = ?1 AND type_name = 'email' AND folder = ?2",
            )?
            .query_map(params![source_id, name], |row| row.get(0))?
            .collect::<Result<Vec<i64>, _>>()?;
        for eid in &email_ids {
            tx.execute("DELETE FROM emails WHERE id = ?1", params![eid])?;
            email_counts.deleted += 1;
        }
        db::maildir_ids::delete_all_emails_in_folder(&tx, source_id, name)?;
        tx.execute("DELETE FROM mailboxes WHERE id = ?1", params![local_id])?;
        db::maildir_ids::delete_mailbox(&tx, source_id, name)?;
        mailbox_counts.deleted += 1;
    }
    tx.commit()?;
    Ok(())
}

fn reconcile_folder(
    conn: &mut Connection,
    source_id: i64,
    folder: &ResolvedFolder,
    mailbox_local: i64,
    flags: &RunFlags,
    counts: &mut TypeCounts,
    logger: Logger,
) -> Result<(), Error> {
    let listing = match messages::list_folder(&folder.path) {
        Ok(l) => l,
        Err(e) => {
            return Err(Error::Partial(format!(
                "list {folder:?}: {e}",
                folder = folder.path
            )));
        }
    };
    if listing.io_failures > 0 {
        counts.failed += listing.io_failures;
    }
    let local = db::maildir_ids::email_ids_in_folder(conn, source_id, &folder.name)?;
    let diff = messages::diff(listing.entries, &local);

    log_at(
        logger,
        LEVEL_PROGRESS,
        &format!(
            "folder {:?}: new={} present={} vanished={}",
            folder.name,
            diff.new.len(),
            diff.present.len(),
            diff.vanished.len()
        ),
    );

    let ctx = messages::InsertContext {
        source_id,
        folder: &folder.name,
        mailbox_local,
        include_deleted: flags.include_deleted,
    };
    messages::apply_folder(conn, ctx, diff, counts, logger)?;
    Ok(())
}

fn build_dry_run_summary(
    conn: &Connection,
    source_id: i64,
    folders: &[ResolvedFolder],
    logger: Logger,
) -> Result<Summary, Error> {
    let mut mailbox = TypeCounts::default();
    let mut email = TypeCounts::default();
    let local_mailboxes = if source_id < 0 {
        HashMap::new()
    } else {
        db::maildir_ids::mailbox_folders(conn, source_id)?
    };
    let server_set: HashSet<&str> = folders.iter().map(|f| f.name.as_str()).collect();
    let new_folders: Vec<&str> = folders
        .iter()
        .map(|f| f.name.as_str())
        .filter(|n| !local_mailboxes.contains_key(*n))
        .collect();
    let vanished_folder_names: Vec<&str> = local_mailboxes
        .keys()
        .filter(|n| !server_set.contains(n.as_str()))
        .map(|s| s.as_str())
        .collect();
    mailbox.created += new_folders.len() as u64;
    mailbox.fetched += new_folders.len() as u64;
    mailbox.deleted += vanished_folder_names.len() as u64;

    for folder in folders {
        if folder.ephemeral {
            continue;
        }
        let listing = match messages::list_folder(&folder.path) {
            Ok(l) => l,
            Err(e) => {
                logger.warn(&format!("dry-run list {:?}: {e}", folder.name));
                email.failed += 1;
                continue;
            }
        };
        email.failed += listing.io_failures;
        let disk_ids: HashSet<String> = listing
            .entries
            .iter()
            .map(|e| e.unique_id.clone())
            .collect();
        let local = if source_id < 0 {
            HashMap::new()
        } else {
            db::maildir_ids::email_ids_in_folder(conn, source_id, &folder.name)?
        };
        let local_keys: HashSet<&String> = local.keys().collect();
        let new_count = disk_ids.iter().filter(|u| !local.contains_key(*u)).count() as u64;
        let vanished_count = local_keys
            .iter()
            .filter(|k| !disk_ids.contains(k.as_str()))
            .count() as u64;
        let present_count = disk_ids.iter().filter(|u| local.contains_key(*u)).count() as u64;
        email.created += new_count;
        email.fetched += new_count;
        email.deleted += vanished_count;
        email.skipped += present_count;
    }
    for name in &vanished_folder_names {
        if source_id >= 0 {
            let n = db::maildir_ids::email_ids_in_folder(conn, source_id, name)?.len() as u64;
            email.deleted += n;
        }
    }

    Ok(Summary {
        per_type: vec![("mailbox", mailbox), ("email", email)],
        retries_observed: 0,
        retry_after_sleeps: 0,
    })
}

fn log_at(logger: Logger, level: u8, msg: &str) {
    if logger.enabled(level) {
        eprintln!("{msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_url_for_ascii_path_is_passthrough() {
        let p = std::path::Path::new("/home/alice/Maildir");
        let url = file_url_for(p).unwrap();
        assert_eq!(url, "file:///home/alice/Maildir");
    }

    #[test]
    fn file_url_for_percent_encodes_spaces_and_unicode() {
        let p = std::path::Path::new("/home/alice/My Maildir/Sübfolder");
        let url = file_url_for(p).unwrap();
        assert!(url.starts_with("file:///home/alice/My%20Maildir/"));
        assert!(url.contains("S%C3%BCbfolder"));
    }

    #[test]
    fn file_url_for_rejects_relative_paths() {
        let p = std::path::Path::new("./relative");
        let err = file_url_for(p).unwrap_err();
        assert!(matches!(err, Error::Usage(_)));
    }
}
