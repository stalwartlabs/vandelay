/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::archive::rows::{MailboxRecord, Subscribed};
use crate::archive::sources::{SourceKey, SourceKind};
use crate::archive::{Archive, LocalId, SourceId};
use crate::error::Error;
use crate::imap::client::Capability;
use crate::imap::command::Command;
use crate::imap::session::{IMAP, ImapSession};
use crate::logging::Logger;
use crate::net::backoff::BackoffState;
use crate::net::stream::{Connector, Endpoint};
use crate::sync::run::{GcFailure, ImportRun, SourceClaim, SourceLabel};
use crate::sync::tree::FolderPath;
use crate::sync::vanish::{Children, OnRowError, RowFailure, VanishedSweep};
use crate::sync::{RunMode, RunOptions, RunOutcome, Summary, TypeCounts};
use crate::types::ObjectType;

use super::control::Control;
use super::folders::{DiscoveredFolder, FolderFilters, ResolvedFolder};
use super::pool::{WorkerArgs, WorkerPool};
use super::{DeletedMessages, ImapImport, MimeKeywords};

const DEFAULT_DELIMITER: char = '/';

#[derive(Clone, Copy)]
pub(super) struct MessageOptions {
    pub fetch_batch: usize,
    pub deleted: DeletedMessages,
    pub mime_keywords: MimeKeywords,
    pub logger: Logger,
}

#[derive(Default)]
pub(super) struct Counts {
    pub mailbox: TypeCounts,
    pub email: TypeCounts,
}

pub(super) struct ImapRun {
    pub run: ImportRun,
    pub control: Control,
    pub counts: Counts,
}

impl ImapImport {
    pub fn run(self, options: RunOptions) -> RunOutcome {
        let failed = |error: Error| RunOutcome {
            summary: Summary::default(),
            error: Some(error),
        };
        let archive = match Archive::open(&options.archive) {
            Ok(archive) => archive,
            Err(e) => return failed(e.into()),
        };
        let ImapImport {
            url,
            auth,
            cleartext,
            compress,
            include,
            exclude,
            exclude_special,
            folder,
            scope,
            automap,
            deleted,
            mime_keywords,
            fetch_batch,
            imap_connections,
            source_change,
        } = self;
        let session = match Endpoint::parse(&url, &IMAP).and_then(|endpoint| {
            let connector = Connector::new(options.tls).map_err(Error::connection)?;
            Ok(ImapSession::new(
                connector,
                endpoint,
                cleartext,
                auth,
                compress,
                options.logger,
            ))
        }) {
            Ok(session) => Arc::new(session),
            Err(e) => return failed(e),
        };
        let session_url = session.session_url();
        let (client, account_id) = match session.open() {
            Ok(opened) => opened,
            Err(e) => return failed(e),
        };
        let key = SourceKey {
            kind: SourceKind::Imap,
            url: &session_url,
            account_id: &account_id,
        };
        let label = SourceLabel {
            name: Some(&account_id),
            username: &account_id,
        };
        let max_retries = options.max_retries;
        let messages = MessageOptions {
            fetch_batch: fetch_batch.max(1),
            deleted,
            mime_keywords,
            logger: options.logger,
        };
        let run = match ImportRun::start_on(archive, options, &key, label, source_change) {
            Ok(run) => run,
            Err(e) => return failed(e),
        };
        let backoff = BackoffState::new();
        let mut imap = ImapRun {
            run,
            control: Control::new(client, session.clone(), max_retries, backoff.clone()),
            counts: Counts::default(),
        };
        let filters = FolderFilters {
            include,
            exclude,
            exclude_special,
            explicit: folder,
            scope,
            automap,
            namespace_prefix: String::new(),
        };
        let workers = WorkerArgs {
            session,
            max_retries,
            backoff,
        };
        let result = imap.execute(filters, messages, workers, imap_connections);
        imap.finish(result)
    }
}

impl ImapRun {
    fn logger(&self) -> Logger {
        self.run.options().logger
    }

    fn execute(
        &mut self,
        mut filters: FolderFilters,
        messages: MessageOptions,
        workers: WorkerArgs,
        connections: usize,
    ) -> Result<(), Error> {
        filters.namespace_prefix = self.control.namespace_prefix();
        let resolved = self.discover(&filters)?;
        let source = match *self.run.claim() {
            SourceClaim::DryRun(source) => return self.dry_run(source, &resolved),
            SourceClaim::Registered(source) => source,
        };
        self.sync_mailboxes(source, &resolved)?;
        let pool = WorkerPool::start(workers, connections.max(1));
        let result = self.sync_folders(source, &pool, &resolved, messages);
        pool.shutdown();
        result
    }

    fn finish(self, result: Result<(), Error>) -> RunOutcome {
        let ImapRun {
            mut run,
            mut control,
            counts,
        } = self;
        if run.options().mode == RunMode::Apply {
            control.logout();
        }
        *run.counts(ObjectType::Mailbox) = counts.mailbox;
        *run.counts(ObjectType::Email) = counts.email;
        let stats = control.stats();
        match result {
            Ok(()) => run.finish(stats, GcFailure::Warn),
            Err(error) => run.abort(error, stats),
        }
    }

    fn discover(&mut self, filters: &FolderFilters) -> Result<Vec<ResolvedFolder>, Error> {
        let client = self.control.client();
        let extended = client.has(Capability::ListExtended);
        let list_status = extended && client.has(Capability::ListStatus);
        let mut returns: Vec<&str> = Vec::with_capacity(4);
        if extended {
            if client.has(Capability::SpecialUse) {
                returns.push("SPECIAL-USE");
            }
            returns.push("SUBSCRIBED");
            if list_status {
                returns.push("CHILDREN");
                returns.push("STATUS (UIDVALIDITY UIDNEXT MESSAGES)");
            }
        }
        let mut untagged = self
            .control
            .run_collect(&Command::List {
                reference: "",
                patterns: &["*"],
                selection: &[],
                returns: &returns,
            })?
            .untagged;
        if !list_status
            && let Ok(lsub) = self.control.run_collect(&Command::Lsub {
                reference: "",
                pattern: "*",
            })
        {
            untagged.extend(lsub.untagged);
        }
        let discovered = DiscoveredFolder::collect(&untagged, self.control.name_encoding())
            .map_err(|e| Error::Partial(format!("LIST parse: {e}")))?;
        let mut resolved = filters.apply(discovered);
        ResolvedFolder::sort_parents_first(&mut resolved);
        Ok(resolved)
    }

    fn sync_mailboxes(
        &mut self,
        source: SourceId,
        resolved: &[ResolvedFolder],
    ) -> Result<(), Error> {
        let local = self.run.store().imap_ids(source).folders()?;
        self.upsert_mailboxes(source, resolved)?;
        let server: HashSet<&str> = resolved.iter().map(|f| f.name.as_str()).collect();
        let delimiter = resolved
            .iter()
            .find_map(|f| f.delimiter)
            .unwrap_or(DEFAULT_DELIMITER);
        let mut vanished: Vec<(String, LocalId)> = local
            .into_iter()
            .filter(|(name, _)| !server.contains(name.as_str()))
            .collect();
        FolderPath::sort_children_first_by(&mut vanished, delimiter, |(name, _)| name);
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
                let ids = store.imap_ids(source);
                let rows = store.emails();
                for local in ids.items_in(name)?.into_values() {
                    rows.delete(local)?;
                    emails.deleted += 1;
                }
                ids.delete_items_in(name)?;
                store.imap_folder_state(source).delete(name)?;
                ids.delete_folder(name)
            },
            |name, _, failure| {
                if let RowFailure::LiveChildren = failure {
                    logger.note(format_args!(
                        "folder \"{name}\" vanished from the server but still has child mailboxes in the archive; not deleted"
                    ));
                }
            },
        )?;
        Ok(())
    }

    fn upsert_mailboxes(
        &mut self,
        source: SourceId,
        folders: &[ResolvedFolder],
    ) -> Result<(), Error> {
        let logger = self.logger();
        let tx = self.run.store().begin()?;
        let store = tx.store();
        let ids = store.imap_ids(source);
        let mailboxes = store.mailboxes();
        let mut local_ids: HashMap<&str, LocalId> = HashMap::with_capacity(folders.len());
        for folder in folders {
            let parent = match folder.parent_path.as_deref() {
                Some(path) => {
                    let found = match local_ids.get(path) {
                        Some(&id) => Some(id),
                        None => ids.folder_id(path)?,
                    };
                    if found.is_none() {
                        logger.note(format_args!(
                            "parent \"{path}\" of folder \"{}\" could not be resolved; stored at the top level",
                            folder.name
                        ));
                    }
                    found
                }
                None => None,
            };
            let record = MailboxRecord {
                name: &folder.leaf,
                parent,
                role: folder.role,
                sort_order: 0,
                subscribed: if folder.subscribed {
                    Subscribed::Yes
                } else {
                    Subscribed::No
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
        Ok(())
    }

    fn sync_folders(
        &mut self,
        source: SourceId,
        pool: &WorkerPool,
        resolved: &[ResolvedFolder],
        messages: MessageOptions,
    ) -> Result<(), Error> {
        for (i, folder) in resolved.iter().filter(|f| f.selectable).enumerate() {
            if i > 0 {
                self.control.noop();
            }
            match self.reconcile_folder(source, pool, folder, messages) {
                Ok(()) => {}
                Err(e) if e.aborts_run() => return Err(e),
                Err(e) => {
                    messages
                        .logger
                        .note(format_args!("folder \"{}\" failed: {e}", folder.name));
                    self.counts.email.failed += 1;
                }
            }
        }
        Ok(())
    }
}
