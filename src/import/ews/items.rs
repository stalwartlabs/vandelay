/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};

use crate::archive::LocalId;
use crate::archive::ids::ews::ItemRow;
use crate::console::Counted;
use crate::error::Error;
use crate::exchange::ews::EwsClient;
use crate::exchange::ews::error::EwsError;
use crate::exchange::ews::parse::{
    FindItemResponse, ResponseMessage, SyncChange, SyncFolderItemsResponse,
};
use crate::exchange::ews::types::{FolderId, ItemId};
use crate::exchange::ews::xml::{EwsRequest, FolderRef, ItemShape, Traversal};
use crate::logging::Logger;
use crate::sync::pool::Pool;

pub const MAX_EWS_CONNECTIONS: usize = 8;
const FIND_ITEM_PAGE_SIZE: u32 = 500;
const MAX_FIND_ITEM_OFFSET: u32 = 1_000_000;
const MAX_SYNC_ROUNDS: u32 = 200;
const GET_ITEM_RESPONSE: &str = "GetItemResponseMessage";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemEnumeration {
    SyncFolderItems,
    FindItem,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncBatch(u32);

impl SyncBatch {
    pub const MAX: SyncBatch = SyncBatch(512);
    const MIN: SyncBatch = SyncBatch(32);

    pub fn new(changes: u32) -> SyncBatch {
        SyncBatch(changes.clamp(SyncBatch::MIN.0, SyncBatch::MAX.0))
    }

    pub fn get(self) -> u32 {
        self.0
    }

    pub fn shrink(self) -> SyncBatch {
        SyncBatch((self.0 / 2).max(SyncBatch::MIN.0))
    }
}

pub struct ItemRunCtx<'a> {
    pub client: &'a EwsClient,
    pub url: &'a str,
    pub owner_smtp: Option<&'a str>,
    pub batch_size: usize,
    pub attachment_batch: usize,
    pub connections: usize,
    pub enumeration: ItemEnumeration,
    pub sync_batch: SyncBatch,
    pub logger: Logger,
}

#[derive(Debug, Clone)]
pub struct EnumeratedItem {
    pub element: String,
    pub id: ItemId,
    pub item_class: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EnumerationOutcome {
    pub items: Vec<EnumeratedItem>,
    pub mode: EnumerationMode,
}

#[derive(Debug, Clone)]
pub enum EnumerationMode {
    Full,
    Delta {
        deletions: Vec<String>,
        new_sync_state: String,
    },
}

#[derive(Debug, Clone, Default)]
pub struct DiffPlan {
    pub new: Vec<ItemId>,
    pub vanished: Vec<(String, LocalId)>,
    pub present_changed: Vec<(ItemId, LocalId)>,
    pub present_unchanged: usize,
}

struct FetchedBatch {
    len: usize,
    result: Result<Vec<ResponseMessage>, EwsError>,
}

pub struct GetItemBatchOutcome {
    pub messages: Vec<ResponseMessage>,
    pub failed_items: u64,
}

impl ItemRunCtx<'_> {
    fn batch(&self) -> usize {
        self.batch_size.max(1)
    }

    fn workers(&self) -> usize {
        self.connections.clamp(1, MAX_EWS_CONNECTIONS)
    }

    pub fn enumerate(
        &self,
        folder: &FolderId,
        prior_sync_state: Option<&str>,
    ) -> Result<EnumerationOutcome, EwsError> {
        if self.enumeration == ItemEnumeration::SyncFolderItems
            && let Some(state) = prior_sync_state
            && !state.is_empty()
            && let Some(outcome) = self.sync_folder_items(folder, state)?
        {
            return Ok(outcome);
        }
        self.find_items(folder).map(|items| EnumerationOutcome {
            items,
            mode: EnumerationMode::Full,
        })
    }

    fn sync_folder_items(
        &self,
        folder: &FolderId,
        prior_state: &str,
    ) -> Result<Option<EnumerationOutcome>, EwsError> {
        let mut sync_state = prior_state.to_owned();
        let mut items = Vec::new();
        let mut deletions = Vec::new();
        let mut rounds = 0;
        let mut batch = self.sync_batch;
        let version = self.client.server_version();
        loop {
            let retries_before = self.client.stats().retries_observed;
            let body = EwsRequest::SyncFolderItems {
                folder,
                sync_state: &sync_state,
                max_changes: batch.get(),
                version,
            };
            let resp = match self.client.call(self.url, &body) {
                Ok(r) => r,
                Err(EwsError::SoapFault { code, .. }) if code.restarts_full_sync() => {
                    return Ok(None);
                }
                Err(e) => return Err(e),
            };
            let parsed = SyncFolderItemsResponse::parse(&resp.body)?;
            for change in parsed.changes {
                match change {
                    SyncChange::Create {
                        id,
                        element,
                        item_class,
                    }
                    | SyncChange::Update {
                        id,
                        element,
                        item_class,
                    } => items.push(EnumeratedItem {
                        element,
                        id,
                        item_class,
                    }),
                    SyncChange::Delete { id } => deletions.push(id.id),
                    SyncChange::ReadFlagChange { id, .. } => items.push(EnumeratedItem {
                        element: "Message".to_owned(),
                        id,
                        item_class: None,
                    }),
                }
            }
            if self.client.stats().retries_observed > retries_before {
                let shrunk = batch.shrink();
                if shrunk != batch {
                    self.logger.warn(format_args!(
                        "SyncFolderItems throttled; shrinking the change batch from {} to {}",
                        batch.get(),
                        shrunk.get()
                    ));
                    batch = shrunk;
                }
            }
            sync_state = parsed.sync_state;
            if !parsed.more {
                break;
            }
            rounds += 1;
            if rounds > MAX_SYNC_ROUNDS {
                break;
            }
        }
        Ok(Some(EnumerationOutcome {
            items,
            mode: EnumerationMode::Delta {
                deletions,
                new_sync_state: sync_state,
            },
        }))
    }

    fn find_items(&self, folder: &FolderId) -> Result<Vec<EnumeratedItem>, EwsError> {
        let mut items = Vec::new();
        let mut offset: u32 = 0;
        loop {
            let body = EwsRequest::FindItem {
                parent: FolderRef::Concrete(folder),
                traversal: Traversal::Shallow,
                offset,
                page_size: FIND_ITEM_PAGE_SIZE,
            };
            let resp = self.client.call(self.url, &body)?;
            let parsed = FindItemResponse::parse(&resp.body)?;
            let returned_none = parsed.items.is_empty();
            items.extend(parsed.items.into_iter().map(|entry| EnumeratedItem {
                element: entry.element,
                id: entry.id,
                item_class: entry.item_class,
            }));
            if !parsed.more || returned_none {
                break;
            }
            offset = offset.saturating_add(FIND_ITEM_PAGE_SIZE);
            if offset > MAX_FIND_ITEM_OFFSET {
                break;
            }
        }
        Ok(items)
    }

    pub fn get_items(
        &self,
        shape: ItemShape,
        ids: &[ItemId],
    ) -> Result<GetItemBatchOutcome, EwsError> {
        let batch = self.batch();
        let workers = self.workers();
        let version = self.client.server_version();
        let mut failed_items: u64 = 0;
        let mut messages = Vec::new();
        if workers <= 1 || ids.len() <= batch {
            for chunk in ids.chunks(batch) {
                let body = EwsRequest::GetItem {
                    shape,
                    ids: chunk,
                    version,
                };
                let parsed = self
                    .client
                    .call(self.url, &body)
                    .map_err(|e| (e, "GetItem batch failed"))
                    .and_then(|resp| {
                        ResponseMessage::parse_all(&resp.body, GET_ITEM_RESPONSE)
                            .map_err(|e| (e, "GetItem batch parse failed"))
                    });
                match parsed {
                    Ok(mut msgs) => messages.append(&mut msgs),
                    Err((e, what)) if e.is_per_batch() => {
                        self.logger.warn(format_args!(
                            "{what} ({e}); {} left for the next run",
                            Counted::new(chunk.len() as u64, "item", "items")
                        ));
                        failed_items += chunk.len() as u64;
                    }
                    Err((e, _)) => return Err(e),
                }
            }
            return Ok(GetItemBatchOutcome {
                messages,
                failed_items,
            });
        }
        let client = self.client.clone();
        let url = self.url.to_owned();
        let pool: Pool<Vec<ItemId>, FetchedBatch> =
            Pool::new(workers, move |chunk: Vec<ItemId>| {
                let body = EwsRequest::GetItem {
                    shape,
                    ids: &chunk,
                    version,
                };
                FetchedBatch {
                    len: chunk.len(),
                    result: client
                        .call(&url, &body)
                        .and_then(|resp| ResponseMessage::parse_all(&resp.body, GET_ITEM_RESPONSE)),
                }
            });
        let mut submitted = 0usize;
        for chunk in ids.chunks(batch) {
            pool.submit(chunk.to_vec());
            submitted += 1;
        }
        let mut abort = None;
        for _ in 0..submitted {
            let Ok(FetchedBatch { len: n, result }) = pool.results().recv() else {
                break;
            };
            match result {
                Ok(mut msgs) => messages.append(&mut msgs),
                Err(e) if e.is_per_batch() => {
                    self.logger.warn(format_args!(
                        "GetItem batch failed ({e}); {} left for the next run",
                        Counted::new(n as u64, "item", "items")
                    ));
                    failed_items += n as u64;
                }
                Err(e) => {
                    abort.get_or_insert(e);
                }
            }
        }
        match abort {
            Some(e) => Err(e),
            None => Ok(GetItemBatchOutcome {
                messages,
                failed_items,
            }),
        }
    }

    pub fn for_each_fetched_item(
        &self,
        shape: ItemShape,
        ids: &[ItemId],
        mut on_message: impl FnMut(ResponseMessage) -> Result<(), Error>,
    ) -> Result<u64, Error> {
        let batch = self.batch();
        let window = batch.saturating_mul(self.workers()).max(batch);
        let mut failed_items = 0u64;
        for win in ids.chunks(window) {
            let outcome = self.get_items(shape, win)?;
            failed_items = failed_items.saturating_add(outcome.failed_items);
            for msg in outcome.messages {
                if let Err(e) = on_message(msg) {
                    if e.aborts_run() {
                        return Err(e);
                    }
                    self.logger.warn(format_args!("EWS item not imported: {e}"));
                    failed_items = failed_items.saturating_add(1);
                }
            }
        }
        Ok(failed_items)
    }
}

impl EnumerationOutcome {
    pub fn plan(&self, local: &[ItemRow]) -> DiffPlan {
        match &self.mode {
            EnumerationMode::Full => DiffPlan::full(&self.items, local),
            EnumerationMode::Delta { deletions, .. } => {
                DiffPlan::delta(&self.items, deletions, local)
            }
        }
    }
}

impl DiffPlan {
    fn delta(changes: &[EnumeratedItem], deletions: &[String], local: &[ItemRow]) -> DiffPlan {
        let local: HashMap<&str, &ItemRow> =
            local.iter().map(|r| (r.item_id.as_str(), r)).collect();
        let mut plan = DiffPlan::default();
        for s in changes {
            match local.get(s.id.id.as_str()) {
                None => plan.new.push(s.id.clone()),
                Some(row) => plan.present_changed.push((s.id.clone(), row.local_id)),
            }
        }
        plan.vanished.extend(
            deletions
                .iter()
                .filter_map(|d| local.get(d.as_str()).map(|row| (d.clone(), row.local_id))),
        );
        plan
    }

    pub fn full(server: &[EnumeratedItem], local: &[ItemRow]) -> DiffPlan {
        let by_id: HashMap<&str, &ItemRow> =
            local.iter().map(|r| (r.item_id.as_str(), r)).collect();
        let mut on_server = HashSet::with_capacity(server.len());
        let mut plan = DiffPlan::default();
        for s in server {
            on_server.insert(s.id.id.as_str());
            match by_id.get(s.id.id.as_str()) {
                None => plan.new.push(s.id.clone()),
                Some(row) if s.id.change_key.is_empty() || s.id.change_key == row.change_key => {
                    plan.present_unchanged += 1;
                }
                Some(row) => plan.present_changed.push((s.id.clone(), row.local_id)),
            }
        }
        plan.vanished.extend(
            local
                .iter()
                .filter(|row| !on_server.contains(row.item_id.as_str()))
                .map(|row| (row.item_id.clone(), row.local_id)),
        );
        plan
    }

    pub fn into_fetch(self) -> (Vec<ItemId>, HashMap<String, LocalId>) {
        let mut to_fetch = self.new;
        let mut existing = HashMap::with_capacity(self.present_changed.len());
        to_fetch.reserve(self.present_changed.len());
        for (id, local) in self.present_changed {
            existing.insert(id.id.clone(), local);
            to_fetch.push(id);
        }
        (to_fetch, existing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::ews::parse::MessageItem;
    use crate::logging::Level;
    use crate::net::auth::Auth;
    use crate::net::http::{ClientOptions, TlsPolicy};
    use crate::net::retry::RetryPolicy;

    const NS: &str = " xmlns:soap=\"http://schemas.xmlsoap.org/soap/envelope/\" \
                      xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\" \
                      xmlns:m=\"http://schemas.microsoft.com/exchange/services/2006/messages\"";

    fn fetch_three(fail_with: fn() -> Error) -> (Result<u64, Error>, Vec<String>) {
        let mut server = mockito::Server::new();
        let url = format!("{}/EWS/Exchange.asmx", server.url());
        let messages: String = ["A", "B", "C"]
            .iter()
            .map(|id| {
                format!(
                    "<m:GetItemResponseMessage ResponseClass=\"Success\">\
                     <m:ResponseCode>NoError</m:ResponseCode>\
                     <m:Items><t:Message><t:ItemId Id=\"{id}\" ChangeKey=\"K\"/></t:Message></m:Items>\
                     </m:GetItemResponseMessage>"
                )
            })
            .collect();
        let _m = server
            .mock("POST", "/EWS/Exchange.asmx")
            .with_status(200)
            .with_header("content-type", "text/xml; charset=utf-8")
            .with_body(format!(
                "<soap:Envelope{NS}><soap:Body><m:GetItemResponse{NS}><m:ResponseMessages>\
                 {messages}</m:ResponseMessages></m:GetItemResponse></soap:Body></soap:Envelope>"
            ))
            .create();
        let client = EwsClient::new(
            Auth::Bearer {
                token: "t".to_owned(),
            },
            ClientOptions {
                retry: RetryPolicy::new(0),
                tls: TlsPolicy::Verify,
                logger: Logger::new(Level::Quiet),
            },
        );
        let ctx = ItemRunCtx {
            client: &client,
            url: &url,
            owner_smtp: None,
            batch_size: 10,
            attachment_batch: 10,
            connections: 1,
            enumeration: ItemEnumeration::FindItem,
            sync_batch: SyncBatch::MAX,
            logger: Logger::new(Level::Default),
        };
        let ids: Vec<ItemId> = ["A", "B", "C"]
            .iter()
            .map(|id| ItemId::new(*id, "K"))
            .collect();
        let mut seen = Vec::new();
        let result = ctx.for_each_fetched_item(ItemShape::Message, &ids, |msg| {
            let parsed = MessageItem::parse(&msg.inner_xml).map_err(Error::partial)?;
            seen.push(parsed.id.id.clone());
            if parsed.id.id == "A" {
                return Err(fail_with());
            }
            Ok(())
        });
        (result, seen)
    }

    fn row(item_id: &str, change_key: &str, local_id: i64) -> ItemRow {
        ItemRow {
            item_id: item_id.to_owned(),
            change_key: change_key.to_owned(),
            local_id: LocalId::from_raw(local_id),
        }
    }

    fn message(id: &str, change_key: &str) -> EnumeratedItem {
        EnumeratedItem {
            element: "Message".to_owned(),
            id: ItemId::new(id, change_key),
            item_class: None,
        }
    }

    #[test]
    fn per_item_failure_does_not_abort_the_folder() {
        let (result, seen) = fetch_three(|| Error::Partial("UNIQUE constraint failed".to_owned()));
        assert_eq!(result.unwrap(), 1);
        assert_eq!(seen, ["A", "B", "C"]);
    }

    #[test]
    fn run_aborting_failure_still_stops() {
        let (result, seen) = fetch_three(|| Error::Connection("gone".to_owned()));
        assert!(result.is_err());
        assert_eq!(seen, ["A"]);
    }

    #[test]
    fn sync_batch_shrinks_by_half_down_to_floor() {
        let shrink = |n| SyncBatch(n).shrink().get();
        assert_eq!(shrink(512), 256);
        assert_eq!(shrink(256), 128);
        assert_eq!(shrink(64), 32);
        assert_eq!(shrink(32), 32);
        assert_eq!(shrink(40), 32);
        assert_eq!(SyncBatch::new(4096), SyncBatch::MAX);
        assert_eq!(SyncBatch::new(1).get(), 32);
    }

    #[test]
    fn diff_delta_only_yields_changes_from_server_response() {
        let outcome = EnumerationOutcome {
            items: vec![message("A", "ck-2")],
            mode: EnumerationMode::Delta {
                deletions: vec!["Z".to_owned()],
                new_sync_state: "STATE2".to_owned(),
            },
        };
        let local = [
            row("A", "ck-1", 1),
            row("B", "ck-1", 2),
            row("Z", "ck-9", 99),
        ];
        let plan = outcome.plan(&local);
        assert!(plan.new.is_empty());
        assert_eq!(plan.present_changed.len(), 1);
        assert_eq!(plan.present_changed[0].0.id, "A");
        assert_eq!(plan.vanished, [("Z".to_owned(), LocalId::from_raw(99))]);
        assert_eq!(plan.present_unchanged, 0);
    }

    #[test]
    fn diff_splits_into_new_unchanged_changed_vanished() {
        let server = [
            message("A", "ck-1"),
            message("B", "ck-1"),
            message("C", "ck-2"),
        ];
        let local = [
            row("A", "ck-1", 1),
            row("B", "ck-0", 2),
            row("Z", "ck-9", 99),
        ];
        let plan = DiffPlan::full(&server, &local);
        assert_eq!(plan.new.len(), 1);
        assert_eq!(plan.new[0].id, "C");
        assert_eq!(plan.present_unchanged, 1);
        assert_eq!(plan.present_changed.len(), 1);
        assert_eq!(plan.present_changed[0].0.id, "B");
        assert_eq!(plan.vanished.len(), 1);
        assert_eq!(plan.vanished[0].0, "Z");
        let (to_fetch, existing) = plan.into_fetch();
        let fetched: Vec<&str> = to_fetch.iter().map(|id| id.id.as_str()).collect();
        assert_eq!(fetched, ["C", "B"]);
        assert_eq!(existing.get("B"), Some(&LocalId::from_raw(2)));
    }
}
