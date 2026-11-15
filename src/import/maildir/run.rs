/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use url::Url;

use crate::archive::rows::{MailboxRecord, Subscribed};
use crate::archive::sources::{SourceKey, SourceKind};
use crate::archive::{Archive, LocalId, SourceId};
use crate::console::Grouped;
use crate::error::Error;
use crate::imap::automap::Automap;
use crate::import::imap::DeletedMessages;
use crate::logging::Logger;
use crate::net::http::RetryStats;
use crate::sync::run::{GcFailure, ImportRun, SourceClaim, SourceLabel};
use crate::sync::tree::FolderPath;
use crate::sync::vanish::{Children, OnRowError, RowFailure, VanishedSweep};
use crate::sync::{PlanCounts, RunOptions, RunOutcome, Summary, TypeCounts};
use crate::types::ObjectType;

use super::MaildirImport;
use super::messages::{DiskListing, FolderDiff, FolderWriter, StoredKeywords};
use super::tree::{DELIMITER, DiscoverError, FolderFilters, ResolvedFolder};

#[derive(Default)]
struct Counts {
    mailbox: TypeCounts,
    email: TypeCounts,
}

struct MaildirRun {
    run: ImportRun,
    deleted: DeletedMessages,
    counts: Counts,
}

impl MaildirImport {
    pub fn run(self, options: RunOptions) -> RunOutcome {
        let failed = |error: Error| RunOutcome {
            summary: Summary::default(),
            error: Some(error),
        };
        let logger = options.logger;
        if options.threads > 1 {
            logger.progress(format_args!(
                "Maildir import runs on one thread; --threads is ignored"
            ));
        }
        let MaildirImport {
            maildir,
            include,
            exclude,
            folder,
            automap,
            deleted,
            source_change,
        } = self;
        let canonical = match fs::canonicalize(&maildir) {
            Ok(canonical) => canonical,
            Err(e) => return failed(Error::Usage(format!("--maildir {maildir:?}: {e}"))),
        };
        MaildirImport::warn_if_subfolder(&canonical, logger);
        let filters = FolderFilters {
            include,
            exclude,
            explicit: folder,
        };
        let resolved = match MaildirImport::resolve(&canonical, automap, &filters) {
            Ok(resolved) => resolved,
            Err(e) => return failed(e),
        };
        let archive = match Archive::open(&options.archive) {
            Ok(archive) => archive,
            Err(e) => return failed(e.into()),
        };
        let session_url = match MaildirImport::source_url(&canonical) {
            Ok(url) => url,
            Err(e) => return failed(e),
        };
        let account_id = canonical.to_string_lossy();
        let key = SourceKey {
            kind: SourceKind::Maildir,
            url: &session_url,
            account_id: &account_id,
        };
        let label = SourceLabel {
            name: canonical.file_name().and_then(|n| n.to_str()),
            username: "",
        };
        let run = match ImportRun::start_on(archive, options, &key, label, source_change) {
            Ok(run) => run,
            Err(e) => return failed(e),
        };
        logger.progress(format_args!("Maildir source {session_url}"));
        let mut maildir = MaildirRun {
            run,
            deleted,
            counts: Counts::default(),
        };
        let result = maildir.execute(&resolved);
        maildir.finish(result)
    }

    fn resolve(
        canonical: &Path,
        automap: Automap,
        filters: &FolderFilters,
    ) -> Result<Vec<ResolvedFolder>, Error> {
        let discovered = ResolvedFolder::discover(canonical, automap).map_err(|e| {
            Error::Usage(match e {
                DiscoverError::NotFound(p) => format!("maildir path does not exist: {p:?}"),
                DiscoverError::NotADirectory(p) => format!("not a directory: {p:?}"),
                DiscoverError::NotAMaildir(p) => {
                    format!("not a Maildir: {p:?} (missing cur/ subdirectory)")
                }
                DiscoverError::NotMaildirPlus(name) => format!(
                    "only Maildir++ layout is supported; found non-prefixed subfolder {name:?} \
                     (subfolders must be named with a leading '.', e.g. '.{name}')"
                ),
                DiscoverError::Io(p, e) => format!("walking {p:?}: {e}"),
            })
        })?;
        if !filters.explicit.is_empty()
            && (!filters.include.is_empty() || !filters.exclude.is_empty())
        {
            return Err(Error::Usage(
                "--folder is mutually exclusive with --include/--exclude".to_owned(),
            ));
        }
        let mut resolved = filters.apply(discovered);
        ResolvedFolder::restore_ephemeral_parents(&mut resolved, canonical);
        Ok(resolved)
    }

    fn source_url(path: &Path) -> Result<String, Error> {
        Url::from_file_path(path).map(String::from).map_err(|_| {
            Error::Usage(format!(
                "cannot encode maildir path as a file:// URL: {path:?}"
            ))
        })
    }

    fn warn_if_subfolder(canonical: &Path, logger: Logger) {
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
        logger.note(format_args!(
            "{} looks like a Maildir++ subfolder of {} (its name starts with '.' and the \
             parent has its own cur/); importing it anyway, but you may have meant the parent",
            canonical.display(),
            parent.display()
        ));
    }
}

impl MaildirRun {
    fn logger(&self) -> Logger {
        self.run.options().logger
    }

    fn execute(&mut self, resolved: &[ResolvedFolder]) -> Result<(), Error> {
        let source = match *self.run.claim() {
            SourceClaim::DryRun(source) => return self.dry_run(source, resolved),
            SourceClaim::Registered(source) => source,
        };
        let local = self.run.store().maildir_ids(source).folders()?;
        let mailboxes = self.upsert_mailboxes(source, resolved)?;
        self.sweep_vanished(source, local, resolved)?;
        let logger = self.logger();
        for folder in resolved.iter().filter(|f| !f.is_ephemeral()) {
            let Some(&mailbox) = mailboxes.get(folder.name.as_str()) else {
                logger.note(format_args!(
                    "folder \"{}\" is missing from the mailbox id map",
                    folder.name
                ));
                self.counts.email.failed += 1;
                continue;
            };
            if let Err(e) = self.reconcile_folder(source, folder, mailbox) {
                if e.aborts_run() {
                    return Err(e);
                }
                logger.note(format_args!("folder \"{}\" failed: {e}", folder.name));
                self.counts.email.failed += 1;
            }
        }
        Ok(())
    }

    fn finish(self, result: Result<(), Error>) -> RunOutcome {
        let MaildirRun {
            mut run, counts, ..
        } = self;
        *run.counts(ObjectType::Mailbox) = counts.mailbox;
        *run.counts(ObjectType::Email) = counts.email;
        match result {
            Ok(()) => run.finish(RetryStats::default(), GcFailure::Abort),
            Err(error) => run.abort(error, RetryStats::default()),
        }
    }

    fn upsert_mailboxes<'r>(
        &mut self,
        source: SourceId,
        folders: &'r [ResolvedFolder],
    ) -> Result<HashMap<&'r str, LocalId>, Error> {
        let tx = self.run.archive().transaction()?;
        let store = tx.store();
        let ids = store.maildir_ids(source);
        let mailboxes = store.mailboxes();
        let mut local_ids: HashMap<&str, LocalId> = HashMap::with_capacity(folders.len());
        for folder in folders {
            let parent = match folder.parent_path.as_deref() {
                Some(p) => match local_ids.get(p) {
                    Some(&id) => Some(id),
                    None => ids.folder_id(p).ok().flatten(),
                },
                None => None,
            };
            let record = MailboxRecord {
                name: &folder.leaf,
                parent,
                role: folder.role,
                sort_order: 0,
                subscribed: if folder.is_ephemeral() {
                    Subscribed::No
                } else {
                    Subscribed::Yes
                },
            };
            let id = match ids.folder_id(&folder.name)? {
                Some(id) => {
                    mailboxes.update(id, &record)?;
                    id
                }
                None => {
                    let id = mailboxes.insert(&record)?;
                    ids.insert_folder(&folder.name, id)?;
                    self.counts.mailbox.created += 1;
                    self.counts.mailbox.fetched += 1;
                    id
                }
            };
            local_ids.insert(&folder.name, id);
        }
        tx.commit()?;
        Ok(local_ids)
    }

    fn sweep_vanished(
        &mut self,
        source: SourceId,
        local: HashMap<String, LocalId>,
        resolved: &[ResolvedFolder],
    ) -> Result<(), Error> {
        let on_disk: HashSet<&str> = resolved.iter().map(|f| f.name.as_str()).collect();
        let mut vanished: Vec<(String, LocalId)> = local
            .into_iter()
            .filter(|(name, _)| !on_disk.contains(name.as_str()))
            .collect();
        FolderPath::sort_children_first_by(&mut vanished, DELIMITER, |(name, _)| name);
        let logger = self.logger();
        let emails = &mut self.counts.email;
        VanishedSweep::new(
            self.run.store(),
            OnRowError::Abort,
            Children::RefuseSubfolders,
        )
        .run(
            ObjectType::Mailbox,
            &vanished,
            &mut self.counts.mailbox,
            |store, name, _| {
                let ids = store.maildir_ids(source);
                let rows = store.emails();
                for local in ids.items_in(name)?.into_values() {
                    rows.delete(local)?;
                    emails.deleted += 1;
                }
                ids.delete_items_in(name)?;
                ids.delete_folder(name)
            },
            |name, _, failure| {
                if let RowFailure::LiveChildren = failure {
                    logger.note(format_args!(
                        "folder \"{name}\" vanished from disk but still has child mailboxes in \
                         the archive; not deleted"
                    ));
                }
            },
        )?;
        Ok(())
    }

    fn reconcile_folder(
        &mut self,
        source: SourceId,
        folder: &ResolvedFolder,
        mailbox: LocalId,
    ) -> Result<(), Error> {
        let listing = DiskListing::read(&folder.path)
            .map_err(|e| Error::Partial(format!("list {:?}: {e}", folder.path)))?;
        let counts = &mut self.counts.email;
        counts.failed += listing.io_failures;
        let store = self.run.store();
        let local = store.maildir_ids(source).items_in(&folder.name)?;
        let diff = FolderDiff::compute(listing.entries, &local);
        let logger = self.run.options().logger;
        logger.progress(format_args!(
            "Folder \"{}\": {} new, {} vanished, {} present",
            folder.name,
            Grouped(diff.new.len() as u64),
            Grouped(diff.vanished.len() as u64),
            Grouped(diff.present.len() as u64)
        ));
        let stored = StoredKeywords::load(store, source, &folder.name).unwrap_or_default();
        let deleted = self.deleted;
        let tx = self.run.archive().transaction()?;
        FolderWriter {
            store: tx.store(),
            source,
            folder: &folder.name,
            mailbox,
            deleted,
        }
        .apply(&diff, &stored, &mut self.counts.email, logger);
        tx.commit()?;
        Ok(())
    }

    fn dry_run(
        &mut self,
        source: Option<SourceId>,
        folders: &[ResolvedFolder],
    ) -> Result<(), Error> {
        let logger = self.logger();
        let store = self.run.store();
        let ids = source.map(|source| store.maildir_ids(source));
        let local_mailboxes = match ids {
            Some(ids) => ids.folders()?,
            None => HashMap::new(),
        };
        let failed = &mut self.counts.email.failed;
        let mut mailbox = PlanCounts::default();
        let mut email = PlanCounts::default();
        let on_disk: HashSet<&str> = folders.iter().map(|f| f.name.as_str()).collect();
        mailbox.new = folders
            .iter()
            .filter(|f| !local_mailboxes.contains_key(&f.name))
            .count() as u64;
        mailbox.present = folders.len() as u64 - mailbox.new;
        for folder in folders.iter().filter(|f| !f.is_ephemeral()) {
            let listing = match DiskListing::read(&folder.path) {
                Ok(listing) => listing,
                Err(e) => {
                    logger.warn(format_args!("cannot list folder \"{}\": {e}", folder.name));
                    *failed += 1;
                    continue;
                }
            };
            *failed += listing.io_failures;
            let local = match ids {
                Some(ids) => ids.items_in(&folder.name)?,
                None => HashMap::new(),
            };
            let disk: HashSet<&str> = listing
                .entries
                .iter()
                .map(|e| e.unique_id.as_str())
                .collect();
            let present = disk.iter().filter(|u| local.contains_key(**u)).count() as u64;
            email.new += disk.len() as u64 - present;
            email.present += present;
            email.vanished += local.keys().filter(|k| !disk.contains(k.as_str())).count() as u64;
        }
        for name in local_mailboxes
            .keys()
            .filter(|n| !on_disk.contains(n.as_str()))
        {
            mailbox.vanished += 1;
            if let Some(ids) = ids {
                email.vanished += ids.items_in(name)?.len() as u64;
            }
        }
        let summary = self.run.summary();
        *summary.plan(ObjectType::Mailbox) = mailbox;
        *summary.plan(ObjectType::Email) = email;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_url_for_ascii_path_is_passthrough() {
        let url = MaildirImport::source_url(Path::new("/home/alice/Maildir")).unwrap();
        assert_eq!(url, "file:///home/alice/Maildir");
    }

    #[test]
    fn source_url_percent_encodes_spaces_and_unicode() {
        let url = MaildirImport::source_url(Path::new("/home/alice/My Maildir/Sübfolder")).unwrap();
        assert!(url.starts_with("file:///home/alice/My%20Maildir/"));
        assert!(url.contains("S%C3%BCbfolder"));
    }

    #[test]
    fn source_url_rejects_relative_paths() {
        let err = MaildirImport::source_url(Path::new("./relative")).unwrap_err();
        assert!(matches!(err, Error::Usage(_)));
    }
}
