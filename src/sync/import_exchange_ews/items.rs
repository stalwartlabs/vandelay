/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;

use crate::error::Error;
use crate::exchange_ews::EwsClient;
use crate::exchange_ews::error::EwsError;
use crate::exchange_ews::parse::{
    SyncChange, parse_find_item_response, parse_response_messages, parse_sync_folder_items_response,
};
use crate::exchange_ews::types::{FolderId, ItemId, ResponseCode};
use crate::exchange_ews::xml::{
    FolderRef, ItemShape, Traversal, find_item_body, get_item_body, sync_folder_items_body,
};
use crate::logging::Logger;

pub struct ItemRunCtx<'a> {
    pub client: &'a EwsClient,
    pub url: &'a str,
    pub source_id: i64,
    pub batch_size: usize,
    pub attachment_batch: usize,
    pub connections: usize,
    pub use_syncfolderitems: bool,
    pub sync_batch: u32,
    pub logger: Logger,
}

pub const SYNC_BATCH_MAX: u32 = 512;
const SYNC_BATCH_MIN: u32 = 32;

fn shrink_sync_batch(current: u32) -> u32 {
    (current / 2).max(SYNC_BATCH_MIN)
}

#[derive(Debug, Clone)]
pub struct EnumeratedItem {
    pub element: String,
    pub id: ItemId,
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

pub fn enumerate_folder(
    ctx: &ItemRunCtx<'_>,
    folder: &FolderId,
    prior_sync_state: Option<&str>,
) -> Result<EnumerationOutcome, EwsError> {
    if ctx.use_syncfolderitems
        && let Some(state) = prior_sync_state
        && !state.is_empty()
        && let Some(outcome) = try_sync_folder_items(ctx, folder, state)?
    {
        return Ok(outcome);
    }
    enumerate_via_find_item(ctx, folder).map(|items| EnumerationOutcome {
        items,
        mode: EnumerationMode::Full,
    })
}

fn sync_fallback_codes(code: &ResponseCode) -> bool {
    matches!(
        code,
        ResponseCode::InvalidIdMalformed | ResponseCode::AccessDenied | ResponseCode::Other(_)
    )
}

fn try_sync_folder_items(
    ctx: &ItemRunCtx<'_>,
    folder: &FolderId,
    prior_state: &str,
) -> Result<Option<EnumerationOutcome>, EwsError> {
    let mut sync_state = prior_state.to_owned();
    let mut items: Vec<EnumeratedItem> = Vec::new();
    let mut deletions: Vec<String> = Vec::new();
    let mut iters = 0;
    let mut batch = ctx.sync_batch.clamp(SYNC_BATCH_MIN, SYNC_BATCH_MAX);
    let version = ctx.client.server_version();
    loop {
        let retries_before = ctx.client.retries_observed();
        let body = sync_folder_items_body(folder, &sync_state, batch, version);
        let resp = match ctx.client.call(ctx.url, "SyncFolderItems", &body) {
            Ok(r) => r,
            Err(EwsError::SoapFault {
                code: ResponseCode::InvalidSyncStateData,
                ..
            }) => {
                return Ok(None);
            }
            Err(EwsError::SoapFault { code, .. }) if sync_fallback_codes(&code) => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let parsed = parse_sync_folder_items_response(&resp.body)?;
        for change in parsed.changes {
            match change {
                SyncChange::Create { id, element } | SyncChange::Update { id, element } => {
                    items.push(EnumeratedItem { element, id });
                }
                SyncChange::Delete { id } => {
                    deletions.push(id.id);
                }
                SyncChange::ReadFlagChange { id, .. } => {
                    items.push(EnumeratedItem {
                        element: "Message".to_owned(),
                        id,
                    });
                }
            }
        }
        if ctx.client.retries_observed() > retries_before {
            let shrunk = shrink_sync_batch(batch);
            if shrunk != batch {
                ctx.logger.warn(&format!(
                    "EWS SyncFolderItems throttled; shrinking change batch {batch} -> {shrunk}"
                ));
                batch = shrunk;
            }
        }
        sync_state = parsed.sync_state;
        if !parsed.more {
            break;
        }
        iters += 1;
        if iters > 200 {
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

fn enumerate_via_find_item(
    ctx: &ItemRunCtx<'_>,
    folder: &FolderId,
) -> Result<Vec<EnumeratedItem>, EwsError> {
    let mut items: Vec<EnumeratedItem> = Vec::new();
    let mut offset: u32 = 0;
    let page_size: u32 = 500;
    loop {
        let body = find_item_body(
            FolderRef::Concrete(folder),
            Traversal::Shallow,
            offset,
            page_size,
        );
        let resp = ctx.client.call(ctx.url, "FindItem", &body)?;
        let parsed = parse_find_item_response(&resp.body)?;
        let returned = parsed.items.len() as u32;
        for entry in parsed.items {
            items.push(EnumeratedItem {
                element: entry.element,
                id: entry.id,
            });
        }
        if !parsed.more {
            break;
        }
        if returned == 0 {
            break;
        }
        offset = offset.saturating_add(page_size);
        if offset > 1_000_000 {
            break;
        }
    }
    Ok(items)
}

#[derive(Debug, Clone)]
pub struct DiffPlan {
    pub new: Vec<ItemId>,
    pub vanished: Vec<(String, i64)>,
    pub present_changed: Vec<(ItemId, i64)>,
    pub present_unchanged: Vec<(String, i64)>,
}

pub fn plan_for(
    outcome: &EnumerationOutcome,
    local: &[crate::db::exchange_ews_ids::ItemRow],
) -> DiffPlan {
    match &outcome.mode {
        EnumerationMode::Full => diff(&outcome.items, local),
        EnumerationMode::Delta { deletions, .. } => diff_delta(&outcome.items, deletions, local),
    }
}

fn diff_delta(
    server_changes: &[EnumeratedItem],
    deletions: &[String],
    local: &[crate::db::exchange_ews_ids::ItemRow],
) -> DiffPlan {
    let local_map: HashMap<&str, &crate::db::exchange_ews_ids::ItemRow> =
        local.iter().map(|r| (r.item_id.as_str(), r)).collect();
    let mut plan = DiffPlan {
        new: Vec::new(),
        vanished: Vec::new(),
        present_changed: Vec::new(),
        present_unchanged: Vec::new(),
    };
    for s in server_changes {
        match local_map.get(s.id.id.as_str()) {
            None => plan.new.push(s.id.clone()),
            Some(row) => plan.present_changed.push((s.id.clone(), row.local_id)),
        }
    }
    for d in deletions {
        if let Some(row) = local_map.get(d.as_str()) {
            plan.vanished.push((d.clone(), row.local_id));
        }
    }
    plan
}

pub fn diff(server: &[EnumeratedItem], local: &[crate::db::exchange_ews_ids::ItemRow]) -> DiffPlan {
    let local_map: HashMap<&str, &crate::db::exchange_ews_ids::ItemRow> =
        local.iter().map(|r| (r.item_id.as_str(), r)).collect();
    let server_map: HashMap<&str, &EnumeratedItem> =
        server.iter().map(|s| (s.id.id.as_str(), s)).collect();
    let mut plan = DiffPlan {
        new: Vec::new(),
        vanished: Vec::new(),
        present_changed: Vec::new(),
        present_unchanged: Vec::new(),
    };
    for s in server {
        match local_map.get(s.id.id.as_str()) {
            None => plan.new.push(s.id.clone()),
            Some(row) => {
                if s.id.change_key.is_empty() || s.id.change_key == row.change_key {
                    plan.present_unchanged
                        .push((row.item_id.clone(), row.local_id));
                } else {
                    plan.present_changed.push((s.id.clone(), row.local_id));
                }
            }
        }
    }
    for row in local {
        if !server_map.contains_key(row.item_id.as_str()) {
            plan.vanished.push((row.item_id.clone(), row.local_id));
        }
    }
    plan
}

pub struct GetItemBatchOutcome {
    pub messages: Vec<crate::exchange_ews::parse::ResponseMessage>,
    pub failed_items: u64,
}

fn is_per_batch_fault(err: &EwsError) -> bool {
    matches!(
        err,
        EwsError::SoapFault { .. } | EwsError::HttpStatus { .. } | EwsError::Malformed(_)
    )
}

pub fn get_items(
    ctx: &ItemRunCtx<'_>,
    shape: ItemShape,
    ids: &[ItemId],
) -> Result<GetItemBatchOutcome, EwsError> {
    let batch = ctx.batch_size.max(1);
    let workers = ctx.connections.clamp(1, 8);
    let version = ctx.client.server_version();
    let mut failed_items: u64 = 0;
    if workers <= 1 || ids.len() <= batch {
        let mut all = Vec::new();
        for chunk in ids.chunks(batch) {
            let body = get_item_body(shape, chunk, version);
            match ctx.client.call(ctx.url, "GetItem", &body) {
                Ok(resp) => match parse_response_messages(&resp.body, b"GetItemResponseMessage") {
                    Ok(mut msgs) => all.append(&mut msgs),
                    Err(e) if is_per_batch_fault(&e) => {
                        ctx.logger.warn(&format!(
                            "GetItem batch parse failed ({e}); {} ids left for retry next run",
                            chunk.len()
                        ));
                        failed_items += chunk.len() as u64;
                    }
                    Err(e) => return Err(e),
                },
                Err(e) if is_per_batch_fault(&e) => {
                    ctx.logger.warn(&format!(
                        "GetItem batch failed ({e}); {} ids left for retry next run",
                        chunk.len()
                    ));
                    failed_items += chunk.len() as u64;
                }
                Err(e) => return Err(e),
            }
        }
        return Ok(GetItemBatchOutcome {
            messages: all,
            failed_items,
        });
    }
    let client = ctx.client.clone();
    let url = ctx.url.to_owned();
    type BatchResult = (
        usize,
        Result<Vec<crate::exchange_ews::parse::ResponseMessage>, EwsError>,
    );
    let pool: crate::sync::pool::Pool<Vec<ItemId>, BatchResult> =
        crate::sync::pool::Pool::new(workers, move |chunk: Vec<ItemId>| {
            let body = get_item_body(shape, &chunk, version);
            let n = chunk.len();
            let result = match client.call(&url, "GetItem", &body) {
                Ok(resp) => parse_response_messages(&resp.body, b"GetItemResponseMessage"),
                Err(e) => Err(e),
            };
            (n, result)
        });
    let mut submitted = 0usize;
    for chunk in ids.chunks(batch) {
        pool.submit(chunk.to_vec());
        submitted += 1;
    }
    let mut all = Vec::new();
    let mut abort_err: Option<EwsError> = None;
    for _ in 0..submitted {
        match pool.results().recv() {
            Ok((_, Ok(mut msgs))) => all.append(&mut msgs),
            Ok((n, Err(e))) => {
                if is_per_batch_fault(&e) {
                    ctx.logger.warn(&format!(
                        "GetItem batch failed ({e}); {n} ids left for retry next run"
                    ));
                    failed_items += n as u64;
                } else if abort_err.is_none() {
                    abort_err = Some(e);
                }
            }
            Err(_) => break,
        }
    }
    if let Some(e) = abort_err {
        return Err(e);
    }
    Ok(GetItemBatchOutcome {
        messages: all,
        failed_items,
    })
}

pub fn for_each_fetched_item<F>(
    ctx: &ItemRunCtx<'_>,
    shape: ItemShape,
    ids: &[ItemId],
    mut on_message: F,
) -> Result<u64, Error>
where
    F: FnMut(crate::exchange_ews::parse::ResponseMessage) -> Result<(), Error>,
{
    let batch = ctx.batch_size.max(1);
    let workers = ctx.connections.clamp(1, 8);
    let window = batch.saturating_mul(workers).max(batch);
    let mut failed_items = 0u64;
    for win in ids.chunks(window) {
        let outcome = get_items(ctx, shape, win).map_err(Error::from)?;
        failed_items = failed_items.saturating_add(outcome.failed_items);
        for msg in outcome.messages {
            on_message(msg)?;
        }
    }
    Ok(failed_items)
}

pub fn delete_vanished(
    conn: &mut rusqlite::Connection,
    source_id: i64,
    type_name: &str,
    table: &str,
    vanished: &[(String, i64)],
    counts: &mut crate::sync::TypeCounts,
) -> Result<(), Error> {
    use rusqlite::params;
    for (item_id, local_id) in vanished {
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| Error::Partial(e.to_string()))?;
        match tx.execute(
            &format!("DELETE FROM {table} WHERE id = ?1"),
            params![local_id],
        ) {
            Ok(_) => {
                crate::db::exchange_ews_ids::delete_item(&tx, source_id, type_name, item_id)
                    .map_err(|e| Error::Partial(e.to_string()))?;
                tx.commit().map_err(|e| Error::Partial(e.to_string()))?;
                counts.deleted += 1;
            }
            Err(_) => {
                let _ = tx.rollback();
                counts.failed += 1;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_batch_shrinks_by_half_down_to_floor() {
        assert_eq!(shrink_sync_batch(512), 256);
        assert_eq!(shrink_sync_batch(256), 128);
        assert_eq!(shrink_sync_batch(64), 32);
        assert_eq!(shrink_sync_batch(32), 32);
        assert_eq!(shrink_sync_batch(40), 32);
    }

    #[test]
    fn diff_delta_only_yields_changes_from_server_response() {
        use crate::db::exchange_ews_ids::ItemRow;
        let outcome = EnumerationOutcome {
            items: vec![EnumeratedItem {
                element: "Message".to_owned(),
                id: ItemId::new("A", "ck-2"),
            }],
            mode: EnumerationMode::Delta {
                deletions: vec!["Z".to_owned()],
                new_sync_state: "STATE2".to_owned(),
            },
        };
        let local = vec![
            ItemRow {
                item_id: "A".to_owned(),
                change_key: "ck-1".to_owned(),
                local_id: 1,
            },
            ItemRow {
                item_id: "B".to_owned(),
                change_key: "ck-1".to_owned(),
                local_id: 2,
            },
            ItemRow {
                item_id: "Z".to_owned(),
                change_key: "ck-9".to_owned(),
                local_id: 99,
            },
        ];
        let plan = plan_for(&outcome, &local);
        assert!(plan.new.is_empty());
        assert_eq!(plan.present_changed.len(), 1);
        assert_eq!(plan.present_changed[0].0.id, "A");
        assert_eq!(plan.vanished.len(), 1);
        assert_eq!(plan.vanished[0].0, "Z");
        assert_eq!(plan.present_unchanged.len(), 0);
    }

    #[test]
    fn diff_splits_into_new_unchanged_changed_vanished() {
        use crate::db::exchange_ews_ids::ItemRow;
        let server = vec![
            EnumeratedItem {
                element: "Message".to_owned(),
                id: ItemId::new("A", "ck-1"),
            },
            EnumeratedItem {
                element: "Message".to_owned(),
                id: ItemId::new("B", "ck-1"),
            },
            EnumeratedItem {
                element: "Message".to_owned(),
                id: ItemId::new("C", "ck-2"),
            },
        ];
        let local = vec![
            ItemRow {
                item_id: "A".to_owned(),
                change_key: "ck-1".to_owned(),
                local_id: 1,
            },
            ItemRow {
                item_id: "B".to_owned(),
                change_key: "ck-0".to_owned(),
                local_id: 2,
            },
            ItemRow {
                item_id: "Z".to_owned(),
                change_key: "ck-9".to_owned(),
                local_id: 99,
            },
        ];
        let plan = diff(&server, &local);
        assert_eq!(plan.new.len(), 1);
        assert_eq!(plan.new[0].id, "C");
        assert_eq!(plan.present_unchanged.len(), 1);
        assert_eq!(plan.present_unchanged[0].0, "A");
        assert_eq!(plan.present_changed.len(), 1);
        assert_eq!(plan.present_changed[0].0.id, "B");
        assert_eq!(plan.vanished.len(), 1);
        assert_eq!(plan.vanished[0].0, "Z");
    }
}
