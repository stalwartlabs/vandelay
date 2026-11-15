/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::RecvTimeoutError;

use crate::archive::rows::{EmailRecord, Write};
use crate::archive::state::imap::FolderState;
use crate::archive::{LocalId, SourceId, Store};
use crate::console::{Counted, Grouped};
use crate::error::Error;
use crate::imap::command::{Command, UidSet};
use crate::sync::TypeCounts;
use crate::sync::dates::{MissingDate, Rfc3339};
use crate::sync::diff::IdDiff;
use crate::sync::emailmeta::EmailMeta;

use super::MimeKeywords;
use super::fetch::FetchAttrs;
use super::folders::ResolvedFolder;
use super::internaldate::InternalDate;
use super::keywords::Translation;
use super::pool::{FetchEvent, FetchJob, WorkerPool};
use super::run::{ImapRun, MessageOptions};

const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(45);
const FLAG_ITEMS: &[&str] = &["UID", "FLAGS"];

struct FetchTarget<'a> {
    folder: &'a str,
    uidvalidity: u32,
    mailbox: LocalId,
}

impl ImapRun {
    pub(super) fn reconcile_folder(
        &mut self,
        source: SourceId,
        pool: &WorkerPool,
        folder: &ResolvedFolder,
        opts: MessageOptions,
    ) -> Result<(), Error> {
        let logger = opts.logger;
        let store = self.run.store();
        let ids = store.imap_ids(source);
        let state = store.imap_folder_state(source);

        if let Some(status) = folder.status.as_ref()
            && status.messages == Some(0)
            && let Some(Ok(uidvalidity)) = status.uidvalidity.map(u32::try_from)
            && let Some(Ok(uidnext)) = status.uidnext.map(u32::try_from)
            && ids.items_in(&folder.name)?.is_empty()
        {
            state.upsert(
                &folder.name,
                &FolderState {
                    uidvalidity,
                    uidnext,
                    last_seen: Rfc3339::now(),
                },
            )?;
            logger.progress(format_args!(
                "Folder \"{}\" is empty (LIST-STATUS); skipped",
                folder.name
            ));
            return Ok(());
        }

        let selected = self.control.select(folder)?;
        let uidvalidity = selected.uidvalidity;
        if uidvalidity == 0 {
            return Err(Error::Partial(format!(
                "SELECT {:?}: server gave no UIDVALIDITY",
                folder.name
            )));
        }

        let counts = &mut self.counts.email;
        if let Some(prev) = state.get(&folder.name)?
            && prev.uidvalidity != uidvalidity
        {
            logger.note(format_args!(
                "UIDVALIDITY of folder \"{}\" changed from {} to {}; its archived messages are replaced",
                folder.name, prev.uidvalidity, uidvalidity
            ));
            let tx = store.begin()?;
            let ids = tx.store().imap_ids(source);
            let rows = tx.store().emails();
            for local in ids.items_in(&folder.name)?.into_values() {
                rows.delete(local)?;
                counts.deleted += 1;
            }
            ids.delete_items_in(&folder.name)?;
            tx.commit()?;
        }

        let server = self
            .control
            .uids()
            .map_err(|e| Error::Partial(format!("UID enum {:?}: {e}", folder.name)))?;
        let local = ids.items_at_validity(&folder.name, uidvalidity)?;
        let mut diff = IdDiff::compute(server, &local);
        diff.new.sort_unstable();
        diff.present.sort_unstable_by_key(|(uid, _)| *uid);

        logger.progress(format_args!(
            "Folder \"{}\": {} new, {} vanished, {} present",
            folder.name,
            Grouped(diff.new.len() as u64),
            Grouped(diff.vanished.len() as u64),
            Grouped(diff.present.len() as u64)
        ));

        if !diff.vanished.is_empty() {
            let tx = store.begin()?;
            let ids = tx.store().imap_ids(source);
            let rows = tx.store().emails();
            for &(uid, local) in &diff.vanished {
                rows.delete(local)?;
                ids.delete_item(&folder.name, uidvalidity, uid)?;
                counts.deleted += 1;
            }
            tx.commit()?;
        }

        if !diff.new.is_empty() {
            let mailbox = ids.folder_id(&folder.name)?.ok_or_else(|| {
                Error::Partial(format!(
                    "mailbox {:?} missing from sync_id_imap",
                    folder.name
                ))
            })?;
            let folder_name: Arc<str> = Arc::from(folder.name.as_str());
            let wire_name: Arc<str> = Arc::from(folder.wire_name.as_str());
            let mut batches = 0usize;
            for batch in diff.new.chunks(opts.fetch_batch) {
                pool.submit(FetchJob {
                    folder: folder_name.clone(),
                    wire_name: wire_name.clone(),
                    uids: batch.to_vec(),
                });
                batches += 1;
            }
            let target = FetchTarget {
                folder: &folder.name,
                uidvalidity,
                mailbox,
            };
            let tx = store.begin()?;
            let mut done = 0usize;
            while done < batches {
                let event = loop {
                    match pool.recv_timeout(KEEPALIVE_INTERVAL) {
                        Ok(event) => break event,
                        Err(RecvTimeoutError::Timeout) => self.control.noop(),
                        Err(RecvTimeoutError::Disconnected) => {
                            return Err(Error::Partial("worker channel disconnected".to_owned()));
                        }
                    }
                };
                match event {
                    FetchEvent::Item(attrs) => {
                        ImapRun::insert_message(tx.store(), source, &target, attrs, opts, counts)?;
                    }
                    FetchEvent::ChunkDone {
                        folder: chunk_folder,
                        requested,
                        outcome,
                    } => {
                        done += 1;
                        if let Err(e) = outcome {
                            logger.note(format_args!(
                                "UID FETCH chunk failed in folder \"{chunk_folder}\": {e}"
                            ));
                            counts.failed += requested as u64;
                        }
                    }
                }
            }
            tx.commit()?;
        }

        if !diff.present.is_empty() {
            let tx = store.begin()?;
            let mut updated = 0;
            let mut uids: Vec<u32> = Vec::with_capacity(opts.fetch_batch);
            for batch in diff.present.chunks(opts.fetch_batch) {
                uids.clear();
                uids.extend(batch.iter().map(|(uid, _)| *uid));
                let set = UidSet::from_sorted(&uids);
                let resp = self.control.run_collect(&Command::UidFetch {
                    set: &set,
                    items: FLAG_ITEMS,
                })?;
                for u in resp.untagged {
                    let Some(attrs) = FetchAttrs::from_untagged(u) else {
                        continue;
                    };
                    let Some(&id) = attrs.uid.and_then(|uid| local.get(&uid)) else {
                        continue;
                    };
                    if ImapRun::refresh_keywords(tx.store(), id, &attrs, opts)? == Write::Changed {
                        updated += 1;
                    }
                }
            }
            tx.commit()?;
            counts.updated += updated;
        }

        state.upsert(
            &folder.name,
            &FolderState {
                uidvalidity,
                uidnext: selected.uidnext,
                last_seen: Rfc3339::now(),
            },
        )?;
        Ok(())
    }

    fn insert_message(
        store: Store<'_>,
        source: SourceId,
        target: &FetchTarget<'_>,
        attrs: FetchAttrs,
        opts: MessageOptions,
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        let Some(uid) = attrs.uid else {
            return Ok(());
        };
        let Some(body) = attrs.body.as_deref() else {
            counts.skipped += 1;
            return Ok(());
        };
        if let Some(declared) = attrs.size
            && u64::try_from(body.len()).ok() != Some(declared)
        {
            opts.logger.note(format_args!(
                "message UID {uid} in folder \"{}\" has {} but RFC822.SIZE says {}; importing the fetched literal",
                target.folder,
                Counted::new(body.len() as u64, "byte", "bytes"),
                Grouped(declared)
            ));
        }
        let translation = Translation::from_flags(&attrs.flags, opts.deleted);
        if translation.skips(opts.deleted) {
            counts.skipped += 1;
            return Ok(());
        }
        let received_at = match attrs.internaldate.as_deref() {
            Some(s) => InternalDate(s).to_rfc3339()?,
            None => MissingDate::Now.resolve(),
        };
        let blob = store.blobs().intern(body)?;
        let mut keywords = translation.keywords;
        let index = match opts.mime_keywords {
            MimeKeywords::Extract => EmailMeta::index_with_mime_keywords(body, &mut keywords),
            MimeKeywords::Skip => EmailMeta::from_blob(body).index,
        };
        let message_match = index.to_json();
        let keywords = keywords.to_json();
        let local = store.emails().insert(&EmailRecord {
            blob,
            received_at: &received_at,
            mailboxes: &[target.mailbox],
            keywords: &keywords,
            message_match: Some(&message_match),
        })?;
        store
            .imap_ids(source)
            .insert_item(target.folder, target.uidvalidity, uid, local)?;
        counts.created += 1;
        counts.fetched += 1;
        Ok(())
    }

    fn refresh_keywords(
        store: Store<'_>,
        id: LocalId,
        attrs: &FetchAttrs,
        opts: MessageOptions,
    ) -> Result<Write, Error> {
        let translation = Translation::from_flags(&attrs.flags, opts.deleted);
        if translation.skips(opts.deleted) {
            return Ok(Write::Unchanged);
        }
        let rows = store.emails();
        let Some((blob, stored)) = rows.keywords(id)? else {
            return Ok(Write::Unchanged);
        };
        let stored: Vec<String> = serde_json::from_str(&stored).unwrap_or_default();
        let mut keywords = translation.keywords;
        if opts.mime_keywords == MimeKeywords::Extract
            && let Some(blob) = store.blobs().bytes(blob)?
        {
            EmailMeta::add_mime_keywords(&blob, &mut keywords);
        }
        let unchanged = {
            let stored: HashSet<&str> = stored.iter().map(String::as_str).collect();
            let fresh: HashSet<&str> = keywords.iter().collect();
            stored == fresh
        };
        if unchanged {
            return Ok(Write::Unchanged);
        }
        Ok(rows.update_keywords(id, &keywords.to_json())?)
    }
}
