/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use super::blobs::{BlobRef, StoredBlobs};
use super::tree::TreeOrder;
use super::{JmapImporter, Listing};
use crate::archive::rows::Write;
use crate::archive::{BlobId, LocalId, Store};
use crate::console::Initial;
use crate::error::Error;
use crate::jmap::error::JmapError;
use crate::jmap::session::Lane;
use crate::jmap::wire::JmapId;
use crate::jmap::wire::address_book::AddressBook;
use crate::jmap::wire::calendar::Calendar;
use crate::jmap::wire::calendar_event::CalendarEvent;
use crate::jmap::wire::common::Lenient;
use crate::jmap::wire::contact_card::ContactCard;
use crate::jmap::wire::email::Email;
use crate::jmap::wire::file_node::{FileNode, FileProps, NodeKind};
use crate::jmap::wire::identity::Identity;
use crate::jmap::wire::mailbox::Mailbox;
use crate::jmap::wire::participant_identity::ParticipantIdentity;
use crate::jmap::wire::sieve_script::SieveScript;
use crate::sync::TypeCounts;
use crate::sync::diff::IdDiff;
use crate::sync::pool::Pool;
use crate::sync::vanish::{Children, OnRowError, VanishedSweep};
use crate::types::ObjectType;

const WRITE_BATCH: usize = 200;

type Fetched = Result<Vec<Value>, JmapError>;

enum Cursor {
    Stored(String),
    Fresh(Option<String>),
}

impl JmapImporter<'_> {
    pub(super) fn reconcile(&self, ty: ObjectType, counts: &mut TypeCounts) -> Result<(), Error> {
        let ids = self.store.jmap_ids(self.source);
        let local = ids.all(ty).map_err(Error::partial)?;
        let listing = Listing::fetch(self.account, ty)?;
        let diff = IdDiff::compute(listing.ids, &local);

        let stored = self
            .store
            .jmap_state(self.source)
            .get(ty)
            .map_err(Error::partial)?;
        let cursor = match stored {
            Some(since) if ty.supports_changes() => Cursor::Stored(since),
            _ if !ty.supports_changes() => Cursor::Fresh(None),
            _ if ty.is_queryless() => Cursor::Fresh(listing.state),
            None => Cursor::Fresh(self.account.state(ty.jmap_name())?),
            Some(_) => Cursor::Fresh(None),
        };

        if !diff.new.is_empty() {
            let objects = match listing.preloaded {
                Some(list) => {
                    let want: HashSet<&str> = diff.new.iter().map(String::as_str).collect();
                    list.into_iter()
                        .filter(|v| {
                            v.get("id")
                                .and_then(Value::as_str)
                                .is_some_and(|s| want.contains(s))
                        })
                        .collect()
                }
                None => {
                    let new: Vec<&str> = diff.new.iter().map(String::as_str).collect();
                    self.fetch(ty, &new, ty.get_props(), counts)
                }
            };
            self.insert_objects(ty, objects, &local, counts)?;
        }

        if !diff.vanished.is_empty() {
            self.delete_vanished(ty, &diff.vanished)?;
            counts.deleted += diff.vanished.len() as u64;
        }

        self.reconcile_updates(ty, &diff.present, cursor, counts)?;

        self.logger.info(format_args!(
            "  {}: {} fetched, {} deleted, {} skipped, {} failed",
            ty.label(),
            counts.fetched,
            counts.deleted,
            counts.skipped,
            counts.failed
        ));
        Ok(())
    }

    fn fetch(
        &self,
        ty: ObjectType,
        ids: &[&str],
        props: Option<&'static [&'static str]>,
        counts: &mut TypeCounts,
    ) -> Vec<Value> {
        let chunk =
            usize::try_from(self.account.limits().max_objects_in_get.max(1)).unwrap_or(usize::MAX);
        let workers = self.account.limits().workers(self.threads, Lane::Api);
        let account = Arc::clone(self.account);
        let pool: Pool<Vec<JmapId>, Fetched> = Pool::new(workers, move |ids: Vec<JmapId>| {
            account
                .get::<Value>(ty.jmap_name(), &ids, props)
                .map(|r| r.list)
        });

        let mut batches = 0u64;
        for part in ids.chunks(chunk) {
            pool.submit(part.iter().map(|id| JmapId((*id).to_owned())).collect());
            batches += 1;
        }

        let mut out = Vec::with_capacity(ids.len());
        let mut done = 0u64;
        for result in pool.finish() {
            match result {
                Ok(list) => out.extend(list),
                Err(e) => {
                    self.logger
                        .warn(format_args!("{}/get chunk failed: {e}", ty.jmap_name()));
                    counts.failed += 1;
                }
            }
            done += 1;
        }
        self.logger.progress(format_args!(
            "  {}: {done} / {batches} chunks fetched",
            ty.label()
        ));
        out
    }

    fn insert_objects(
        &self,
        ty: ObjectType,
        objects: Vec<Value>,
        known: &HashMap<String, LocalId>,
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        let (ordered, warnings) = TreeOrder::of_objects(ty, &objects, |p| known.contains_key(p));
        for w in &warnings {
            self.logger.warn(format_args!("{w}"));
        }

        for batch in ordered.chunks(WRITE_BATCH) {
            let refs = BlobRef::collect(ty, batch.iter().filter_map(|&i| objects.get(i)));
            let blobs = self.download_blobs(ty, refs)?;
            let tx = self.store.begin().map_err(Error::partial)?;
            let store = tx.store();
            for obj in batch.iter().filter_map(|&i| objects.get(i)) {
                let Some(jmap_id) = obj.get("id").and_then(Value::as_str) else {
                    counts.failed += 1;
                    continue;
                };
                match self.insert_one(store, ty, obj, &blobs) {
                    Ok(local_id) => {
                        store
                            .jmap_ids(self.source)
                            .insert(ty, jmap_id, local_id)
                            .map_err(Error::partial)?;
                        counts.fetched += 1;
                    }
                    Err(e) => {
                        self.logger
                            .warn(format_args!("{} {jmap_id} skipped: {e}", ty.noun()));
                        counts.failed += 1;
                    }
                }
            }
            tx.commit().map_err(Error::partial)?;
        }
        Ok(())
    }

    fn insert_one(
        &self,
        store: Store<'_>,
        ty: ObjectType,
        obj: &Value,
        blobs: &StoredBlobs,
    ) -> Result<LocalId, JmapError> {
        let ids = store.jmap_ids(self.source);
        let resolve = |ty: ObjectType, id: &str| ids.local_id(ty, id).ok().flatten();
        let mut intern = |id: &str| blobs.intern(id);
        match ty {
            ObjectType::Mailbox => Mailbox::deserialize(obj)?.insert(store, &resolve),
            ObjectType::Identity => Identity::deserialize(obj)?.insert(store),
            ObjectType::AddressBook => AddressBook::deserialize(obj)?.insert(store),
            ObjectType::Calendar => Calendar::deserialize(obj)?.insert(store),
            ObjectType::ParticipantIdentity => ParticipantIdentity::deserialize(obj)?.insert(store),
            ObjectType::Email => {
                let mut w = Email::deserialize(obj)?;
                let stored = blobs.get(&w.blob_id.0, "email")?;
                let meta = stored
                    .email
                    .as_ref()
                    .ok_or_else(|| JmapError::malformed("email blob missing"))?;
                if w.received_at.is_none() {
                    w.received_at = meta
                        .date_header
                        .as_deref()
                        .and_then(Lenient::parse_utc_date);
                    let id = obj.get("id").and_then(Value::as_str).unwrap_or("?");
                    let raw = obj
                        .get("receivedAt")
                        .and_then(Value::as_str)
                        .unwrap_or("<missing>");
                    self.logger.warn(format_args!(
                        "email {id}: unusable receivedAt {raw:?}; falling back to {}",
                        match &w.received_at {
                            Some(_) => "the Date header",
                            None => "1970-01-01T00:00:00Z",
                        }
                    ));
                }
                w.insert(store, stored.local_id, &meta.match_key, &resolve)
            }
            ObjectType::SieveScript => {
                let w = SieveScript::deserialize(obj)?;
                let blob = blobs.get(&w.blob_id.0, "sieve")?.local_id;
                w.insert(store, blob)
            }
            ObjectType::FileNode => {
                let w = FileNode::deserialize(obj)?;
                let blob = JmapImporter::file_blob(&w, blobs)?;
                w.insert(store, blob, &resolve)
            }
            ObjectType::ContactCard => {
                ContactCard::deserialize(obj)?.insert(store, &resolve, &mut intern)
            }
            ObjectType::CalendarEvent => {
                CalendarEvent::deserialize(obj)?.insert(store, &resolve, &mut intern)
            }
        }
    }

    fn file_blob(node: &FileNode, blobs: &StoredBlobs) -> Result<Option<BlobId>, JmapError> {
        match &node.kind {
            NodeKind::File(FileProps {
                blob_id: Some(b), ..
            }) => Ok(Some(blobs.get(&b.0, "file")?.local_id)),
            _ => Ok(None),
        }
    }

    fn reconcile_updates(
        &self,
        ty: ObjectType,
        present: &[(String, LocalId)],
        cursor: Cursor,
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        let in_order = || present.iter().map(|(id, _)| id.as_str());
        let by_id: HashMap<&str, LocalId> = present
            .iter()
            .map(|(id, local)| (id.as_str(), *local))
            .collect();
        let since = match cursor {
            Cursor::Stored(since) => since,
            Cursor::Fresh(run_state) => {
                let clean = self.fetch_and_update(ty, in_order(), &by_id, counts)?;
                if let Some(state) = run_state {
                    self.advance_state(ty, &state, clean)?;
                }
                return Ok(());
            }
        };
        match self.account.changes(ty.jmap_name(), &since) {
            Ok(changes) => {
                let updated = changes
                    .updated
                    .iter()
                    .map(|j| j.0.as_str())
                    .filter(|j| by_id.contains_key(j));
                let clean = self.fetch_and_update(ty, updated, &by_id, counts)?;
                self.advance_state(ty, &changes.new_state, clean)
            }
            Err(err @ (JmapError::CannotCalculateChanges | JmapError::UnknownMethod)) => {
                let reason = if matches!(err, JmapError::UnknownMethod) {
                    "server does not implement /changes"
                } else {
                    "server cannot calculate changes from stored state"
                };
                self.logger.warn(format_args!(
                    "{}: {reason}; refreshing every present one",
                    Initial::Lower(ty.label())
                ));
                let captured = self.account.state(ty.jmap_name())?;
                let clean = self.fetch_and_update(ty, in_order(), &by_id, counts)?;
                match captured {
                    Some(state) => self.advance_state(ty, &state, clean),
                    None => Ok(()),
                }
            }
            Err(e) => Err(Error::from(e)),
        }
    }

    fn advance_state(&self, ty: ObjectType, state: &str, clean: bool) -> Result<(), Error> {
        if !clean {
            self.logger.warn(format_args!(
                "{}: holding the sync state; some updates failed and are retried on the next run",
                Initial::Lower(ty.label())
            ));
            return Ok(());
        }
        self.store
            .jmap_state(self.source)
            .upsert(ty, state)
            .map_err(Error::partial)
    }

    fn fetch_and_update<'i>(
        &self,
        ty: ObjectType,
        ids: impl Iterator<Item = &'i str>,
        present: &HashMap<&str, LocalId>,
        counts: &mut TypeCounts,
    ) -> Result<bool, Error> {
        let ids: Vec<&str> = ids.collect();
        if ids.is_empty() {
            return Ok(true);
        }
        let failed_before = counts.failed;
        let objects = self.fetch(ty, &ids, ty.update_props(), counts);
        self.update_objects(ty, &objects, present, counts)?;
        Ok(counts.failed == failed_before)
    }

    fn update_objects(
        &self,
        ty: ObjectType,
        objects: &[Value],
        present: &HashMap<&str, LocalId>,
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        for batch in objects.chunks(WRITE_BATCH) {
            let blobs = self.download_blobs(ty, BlobRef::collect(ty, batch.iter()))?;
            let tx = self.store.begin().map_err(Error::partial)?;
            let store = tx.store();
            for obj in batch {
                let Some(jmap_id) = obj.get("id").and_then(Value::as_str) else {
                    counts.failed += 1;
                    continue;
                };
                let Some(&local_id) = present.get(jmap_id) else {
                    continue;
                };
                match self.update_one(store, ty, local_id, obj, &blobs) {
                    Ok(Write::Changed) => counts.updated += 1,
                    Ok(Write::Unchanged) => {}
                    Err(e) => {
                        self.logger
                            .warn(format_args!("{} {jmap_id} update skipped: {e}", ty.noun()));
                        counts.failed += 1;
                    }
                }
            }
            tx.commit().map_err(Error::partial)?;
        }
        Ok(())
    }

    fn update_one(
        &self,
        store: Store<'_>,
        ty: ObjectType,
        local_id: LocalId,
        obj: &Value,
        blobs: &StoredBlobs,
    ) -> Result<Write, JmapError> {
        let ids = store.jmap_ids(self.source);
        let resolve = |ty: ObjectType, id: &str| ids.local_id(ty, id).ok().flatten();
        let mut intern = |id: &str| blobs.intern(id);
        match ty {
            ObjectType::Mailbox => Mailbox::deserialize(obj)?.update(store, local_id, &resolve),
            ObjectType::Identity => Identity::deserialize(obj)?.update(store, local_id),
            ObjectType::AddressBook => AddressBook::deserialize(obj)?.update(store, local_id),
            ObjectType::Calendar => Calendar::deserialize(obj)?.update(store, local_id),
            ObjectType::ParticipantIdentity => {
                ParticipantIdentity::deserialize(obj)?.update(store, local_id)
            }
            ObjectType::Email => Email::update_placement(store, local_id, obj, &resolve),
            ObjectType::SieveScript => {
                let w = SieveScript::deserialize(obj)?;
                let blob = blobs.get(&w.blob_id.0, "sieve")?.local_id;
                w.update(store, local_id, blob)
            }
            ObjectType::FileNode => {
                let w = FileNode::deserialize(obj)?;
                let blob = JmapImporter::file_blob(&w, blobs)?;
                w.update(store, local_id, blob, &resolve)
            }
            ObjectType::ContactCard => {
                ContactCard::deserialize(obj)?.update(store, local_id, &resolve, &mut intern)
            }
            ObjectType::CalendarEvent => {
                CalendarEvent::deserialize(obj)?.update(store, local_id, &resolve, &mut intern)
            }
        }
    }

    fn delete_vanished(&self, ty: ObjectType, vanished: &[(String, LocalId)]) -> Result<(), Error> {
        let children = if ty.is_tree() {
            Children::DetachSurvivors
        } else {
            Children::Restrict
        };
        let mut scratch = TypeCounts::default();
        VanishedSweep::new(self.store, OnRowError::Abort, children)
            .run_then(
                ty,
                vanished,
                &mut scratch,
                |store, jmap_id, _| store.jmap_ids(self.source).delete(ty, jmap_id),
                |_, _, _| {},
                |store| match ty.member_list() {
                    Some(list) => {
                        let removed: HashSet<LocalId> =
                            vanished.iter().map(|(_, id)| *id).collect();
                        store.prune_members(list, &removed)
                    }
                    None => Ok(()),
                },
            )
            .map_err(Error::partial)?;
        self.logger.progress(format_args!(
            "  {}: {} vanished from the source, deleted",
            ty.label(),
            vanished.len()
        ));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::archive::sources::{SourceKey, SourceKind};
    use crate::archive::{Archive, SourceId};
    use crate::jmap::connect::JmapAccount;
    use crate::jmap::http::JmapClient;
    use crate::jmap::session::Session;
    use crate::logging::{Level, Logger};
    use crate::net::auth::Auth;
    use crate::net::http::{ClientOptions, TlsPolicy};
    use crate::net::retry::RetryPolicy;

    fn account() -> Arc<JmapAccount> {
        let session: Session = serde_json::from_value(json!({
            "apiUrl": "http://unused.invalid/api",
            "uploadUrl": "http://unused.invalid/up",
            "downloadUrl": "http://unused.invalid/down",
            "capabilities": { "urn:ietf:params:jmap:core": {
                "maxObjectsInGet": 500, "maxObjectsInSet": 500, "maxCallsInRequest": 16,
                "maxConcurrentRequests": 4, "maxConcurrentUpload": 4,
                "maxSizeRequest": 10000000, "maxSizeUpload": 50000000 } },
            "accounts": { "w": { "name": "n" } }
        }))
        .unwrap();
        let limits = session.core_limits().unwrap();
        let client = JmapClient::new(
            Auth::Basic {
                user: "u".into(),
                password: "p".into(),
            },
            ClientOptions {
                retry: RetryPolicy::new(0),
                tls: TlsPolicy::Verify,
                logger: Logger::new(Level::Quiet),
            },
        );
        Arc::new(JmapAccount::from_parts(client, session, "w".into(), limits))
    }

    fn source(archive: &Archive) -> SourceId {
        archive
            .store()
            .sources()
            .upsert(
                &SourceKey {
                    kind: SourceKind::Jmap,
                    url: "u",
                    account_id: "w",
                },
                None,
                "u",
            )
            .unwrap()
    }

    fn sweep(archive: &Archive, source: SourceId, ty: ObjectType, vanished: &[(&str, i64)]) {
        let account = account();
        let importer = JmapImporter {
            store: archive.store(),
            source,
            account: &account,
            threads: 1,
            logger: Logger::new(Level::Quiet),
        };
        let vanished: Vec<(String, LocalId)> = vanished
            .iter()
            .map(|(k, id)| ((*k).to_owned(), LocalId::from_raw(*id)))
            .collect();
        importer.delete_vanished(ty, &vanished).unwrap();
    }

    #[test]
    fn vanished_tree_deletes_leaf_first_and_nulls_true_orphan() {
        let archive = Archive::open_in_memory().unwrap();
        let sid = source(&archive);
        let c = archive.connection();
        c.execute_batch(
            "INSERT INTO mailboxes (id,name,parent_id) VALUES (1,'root',NULL);
             INSERT INTO mailboxes (id,name,parent_id) VALUES (2,'mid',1);
             INSERT INTO mailboxes (id,name,parent_id) VALUES (3,'leaf',2);
             INSERT INTO mailboxes (id,name,parent_id) VALUES (4,'survivor',2);",
        )
        .unwrap();
        let ids = archive.store().jmap_ids(sid);
        for (jid, local) in [("R", 1), ("M", 2), ("L", 3), ("S", 4)] {
            ids.insert(ObjectType::Mailbox, jid, LocalId::from_raw(local))
                .unwrap();
        }
        sweep(
            &archive,
            sid,
            ObjectType::Mailbox,
            &[("L", 3), ("M", 2), ("R", 1)],
        );
        let remaining: i64 = c
            .query_row("SELECT count(*) FROM mailboxes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1, "only the surviving child remains");
        let parent: Option<i64> = c
            .query_row("SELECT parent_id FROM mailboxes WHERE id=4", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(parent, None, "true orphan's parent was nulled");
        assert!(
            ids.local_id(ObjectType::Mailbox, "L").unwrap().is_none(),
            "vanished sync_id rows removed"
        );
    }

    #[test]
    fn vanished_mailbox_is_pruned_from_email_mailbox_ids() {
        let archive = Archive::open_in_memory().unwrap();
        let sid = source(&archive);
        let c = archive.connection();
        c.execute_batch(
            "INSERT INTO mailboxes (id,name,parent_id) VALUES (1,'a',NULL);
             INSERT INTO mailboxes (id,name,parent_id) VALUES (2,'b',NULL);",
        )
        .unwrap();
        let blob = archive.store().blobs().intern(b"msg").unwrap();
        c.execute(
            "INSERT INTO emails (blob_id,received_at,mailbox_ids,keywords)
             VALUES (?1,'2020-01-01T00:00:00Z','[1,2]','[]')",
            rusqlite::params![blob],
        )
        .unwrap();
        let ids = archive.store().jmap_ids(sid);
        ids.insert(ObjectType::Mailbox, "A", LocalId::from_raw(1))
            .unwrap();
        ids.insert(ObjectType::Mailbox, "B", LocalId::from_raw(2))
            .unwrap();
        sweep(&archive, sid, ObjectType::Mailbox, &[("A", 1)]);
        let mailbox_ids: String = c
            .query_row("SELECT mailbox_ids FROM emails", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            mailbox_ids, "[2]",
            "deleted mailbox id removed from referring email"
        );
    }
}
