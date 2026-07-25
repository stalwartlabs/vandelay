/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::{Connection, Transaction, params};
use serde_json::json;

use crate::db::{blobs, exchange_graph_ids};
use crate::error::Error;
use crate::exchange_graph::api;
use crate::exchange_graph::client::Accept;
use crate::exchange_graph::error::GraphError;
use crate::logging::LEVEL_PROGRESS;
use crate::sync::TypeCounts;
use crate::sync::emailmeta::email_meta_from_blob;
use crate::sync::pool::Pool;
use crate::sync::keys::index_to_json;

use super::coordinator::{CHUNK_SIZE, GraphCoordinator};
use super::folders::MailFolder;

pub fn reconcile_all(
    conn: &mut Connection,
    ctx: &GraphCoordinator<'_>,
    folders: &[MailFolder],
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let local: std::collections::HashMap<String, i64> =
        exchange_graph_ids::ids_of_type(conn, ctx.source_id, exchange_graph_ids::EMAIL)?;
    let mut server_total: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut planned: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut any_failure = false;

    for folder in folders {
        let url = ctx.endpoints.folder_messages_ids(&folder.graph_id, ctx.top);
        let ids = match api::collect_all_ids(ctx.client, &url, &[]) {
            Ok(v) => v,
            Err(e) => {
                ctx.logger.warn(&format!(
                    "folder {} message enumeration failed: {e}",
                    folder.graph_id
                ));
                counts.failed += 1;
                any_failure = true;
                continue;
            }
        };
        if ctx.logger.enabled(LEVEL_PROGRESS) {
            eprintln!(
                "graph folder {} enumerated {} messages",
                folder.graph_id,
                ids.len()
            );
        }
        for id in &ids {
            server_total.insert(id.clone());
        }
        let new_ids: Vec<String> = ids
            .into_iter()
            .filter(|id| !local.contains_key(id) && planned.insert(id.clone()))
            .collect();
        if new_ids.is_empty() {
            continue;
        }
        fetch_and_insert(conn, ctx, folder, &new_ids, counts)?;
    }

    if any_failure {
        ctx.logger.warn(
            "graph message vanished-cleanup skipped: one or more folders failed to enumerate; \
             a clean re-run will reconcile deletions",
        );
    } else {
        delete_vanished(conn, ctx.source_id, &local, &server_total, counts)?;
    }
    Ok(())
}

fn fetch_and_insert(
    conn: &mut Connection,
    ctx: &GraphCoordinator<'_>,
    folder: &MailFolder,
    ids: &[String],
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let client = ctx.client.clone();
    let endpoints: crate::exchange_graph::api::Endpoints = (*ctx.endpoints).clone();
    type FetchResult = (String, Result<Vec<u8>, GraphError>);
    let pool: Pool<String, FetchResult> = Pool::new(ctx.workers, move |id: String| {
        let url = endpoints.message_mime(&id);
        match client.get_with_prefer(&url, Accept::Text, &[]) {
            Ok(resp) => (id, Ok(resp.body)),
            Err(e) => (id, Err(e)),
        }
    });
    for id in ids {
        pool.submit(id.clone());
    }
    let mut tx_opt: Option<Transaction<'_>> = None;
    let mut in_batch: usize = 0;
    for _ in 0..ids.len() {
        let Ok((graph_id, result)) = pool.results().recv() else {
            break;
        };
        match result {
            Ok(bytes) => {
                if tx_opt.is_none() {
                    tx_opt = Some(conn.unchecked_transaction()?);
                }
                let tx = tx_opt.as_mut().expect("tx is Some");
                apply_message_in_tx(tx, ctx, folder, &graph_id, &bytes, counts)?;
                in_batch += 1;
                if in_batch >= CHUNK_SIZE {
                    if let Some(t) = tx_opt.take() {
                        t.commit()?;
                    }
                    in_batch = 0;
                }
            }
            Err(GraphError::Vanished) => {
                counts.skipped += 1;
            }
            Err(e) => {
                counts.failed += 1;
                ctx.logger
                    .warn(&format!("graph message {graph_id} fetch failed: {e}"));
            }
        }
    }
    if let Some(t) = tx_opt.take() {
        t.commit()?;
    }
    Ok(())
}

fn apply_message_in_tx(
    tx: &Transaction<'_>,
    ctx: &GraphCoordinator<'_>,
    folder: &MailFolder,
    graph_id: &str,
    bytes: &[u8],
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let (idx, date_header) = email_meta_from_blob(bytes);
    let message_match = index_to_json(&idx);
    let received_at = date_header.unwrap_or_else(|| "1970-01-01T00:00:00Z".to_owned());
    let mailbox_ids = json!([folder.local_id]).to_string();
    let keywords = "[]".to_owned();

    let blob_id = blobs::intern_blob(tx, bytes)?;
    tx.execute(
        "INSERT INTO emails (blob_id, received_at, mailbox_ids, keywords, message_match)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![blob_id, received_at, mailbox_ids, keywords, message_match],
    )?;
    let new_id = tx.last_insert_rowid();
    exchange_graph_ids::insert(
        tx,
        ctx.source_id,
        exchange_graph_ids::EMAIL,
        graph_id,
        new_id,
    )?;
    counts.created += 1;
    Ok(())
}

fn delete_vanished(
    conn: &mut Connection,
    source_id: i64,
    local: &std::collections::HashMap<String, i64>,
    server: &std::collections::HashSet<String>,
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let vanished: Vec<(&String, &i64)> = local
        .iter()
        .filter(|(graph_id, _)| !server.contains(graph_id.as_str()))
        .collect();
    for chunk in vanished.chunks(CHUNK_SIZE) {
        let tx = conn.unchecked_transaction()?;
        for (graph_id, local_id) in chunk {
            let result = tx.execute("DELETE FROM emails WHERE id = ?1", params![local_id]);
            match result {
                Ok(_) => {
                    exchange_graph_ids::delete(
                        &tx,
                        source_id,
                        exchange_graph_ids::EMAIL,
                        graph_id,
                    )?;
                    counts.deleted += 1;
                }
                Err(_) => {
                    counts.failed += 1;
                }
            }
        }
        tx.commit()?;
    }
    Ok(())
}
