/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;

use rusqlite::{Connection, Transaction, params};
use serde_json::Value;

use crate::db::{blobs, exchange_graph_ids};
use crate::error::Error;
use crate::exchange_graph::api;
use crate::exchange_graph::client::Accept;
use crate::exchange_graph::error::GraphError;
use crate::logging::LEVEL_PROGRESS;
use crate::sync::TypeCounts;
use crate::sync::import_jmap::pool::Pool;

use super::coordinator::{CHUNK_SIZE, GraphCoordinator};

#[derive(Debug, Clone)]
struct DriveNode {
    graph_id: String,
    name: String,
    is_folder: bool,
    media_type: Option<String>,
    created: String,
    modified: Option<String>,
    parent_graph_id: Option<String>,
}

pub fn reconcile_all(
    conn: &mut Connection,
    ctx: &GraphCoordinator<'_>,
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let Some(root_id) = drive_root_id(ctx) else {
        ctx.logger
            .warn("graph drive unavailable for this account; file import skipped");
        return Ok(());
    };

    let nodes = match enumerate_drive(ctx, &root_id) {
        Ok(v) => v,
        Err(e) => {
            counts.failed += 1;
            ctx.logger
                .warn(&format!("graph drive enumeration failed: {e}"));
            return Ok(());
        }
    };
    if ctx.logger.enabled(LEVEL_PROGRESS) {
        eprintln!("graph drive items enumerated: {}", nodes.len());
    }

    let local: HashMap<String, i64> =
        exchange_graph_ids::ids_of_type(conn, ctx.source_id, exchange_graph_ids::FILE_NODE)?;
    let mut by_graph_id: HashMap<String, i64> = HashMap::new();
    let mut server_ids: Vec<String> = Vec::new();

    let (folders, files): (Vec<DriveNode>, Vec<DriveNode>) =
        nodes.into_iter().partition(|n| n.is_folder);

    for node in order_by_depth(folders) {
        server_ids.push(node.graph_id.clone());
        let parent_local = node
            .parent_graph_id
            .as_deref()
            .and_then(|p| by_graph_id.get(p))
            .copied();
        let local_id = upsert_directory(conn, ctx, &node, parent_local, &local, counts)?;
        by_graph_id.insert(node.graph_id.clone(), local_id);
    }

    let mut wanted: Vec<DriveNode> = Vec::new();
    for node in files {
        server_ids.push(node.graph_id.clone());
        if local.contains_key(&node.graph_id) {
            counts.fetched += 1;
        } else {
            wanted.push(node);
        }
    }

    download_and_insert(conn, ctx, &wanted, &by_graph_id, counts)?;
    delete_vanished(conn, ctx.source_id, &local, &server_ids, counts, ctx)?;
    Ok(())
}

fn drive_root_id(ctx: &GraphCoordinator<'_>) -> Option<String> {
    let value = ctx.client.get_json(&ctx.endpoints.drive_root()).ok()?;
    value.get("id").and_then(Value::as_str).map(str::to_owned)
}

fn enumerate_drive(
    ctx: &GraphCoordinator<'_>,
    root_id: &str,
) -> Result<Vec<DriveNode>, GraphError> {
    let mut out: Vec<DriveNode> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut frontier: Vec<String> = vec![root_id.to_owned()];
    seen.insert(root_id.to_owned());

    while let Some(parent) = frontier.pop() {
        let url = ctx.endpoints.drive_children(&parent, ctx.top);
        let children = api::collect_all_values(ctx.client, &url, &[])?;
        for child in &children {
            let Some(node) = parse_node(child, &parent) else {
                continue;
            };
            if !seen.insert(node.graph_id.clone()) {
                continue;
            }
            if node.is_folder {
                frontier.push(node.graph_id.clone());
            }
            out.push(node);
        }
    }
    Ok(out)
}

fn parse_node(value: &Value, parent_graph_id: &str) -> Option<DriveNode> {
    let graph_id = value.get("id").and_then(Value::as_str)?.to_owned();
    let name = value.get("name").and_then(Value::as_str)?.to_owned();
    let is_folder = value.get("folder").is_some_and(|f| !f.is_null());
    let is_file = value.get("file").is_some_and(|f| !f.is_null());
    if !is_folder && !is_file {
        return None;
    }
    Some(DriveNode {
        graph_id,
        name,
        is_folder,
        media_type: value
            .get("file")
            .and_then(|f| f.get("mimeType"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        created: value
            .get("createdDateTime")
            .and_then(Value::as_str)
            .unwrap_or("1970-01-01T00:00:00Z")
            .to_owned(),
        modified: value
            .get("lastModifiedDateTime")
            .and_then(Value::as_str)
            .map(str::to_owned),
        parent_graph_id: Some(parent_graph_id.to_owned()),
    })
}

fn order_by_depth(folders: Vec<DriveNode>) -> Vec<DriveNode> {
    let parents: HashMap<String, Option<String>> = folders
        .iter()
        .map(|n| (n.graph_id.clone(), n.parent_graph_id.clone()))
        .collect();
    let depth = |start: &str| -> usize {
        let mut d = 0;
        let mut id = start.to_owned();
        let mut guard = 0;
        while let Some(Some(parent)) = parents.get(&id) {
            d += 1;
            id = parent.clone();
            guard += 1;
            if guard > 256 {
                break;
            }
        }
        d
    };
    let mut ordered: Vec<(usize, DriveNode)> = folders
        .iter()
        .map(|n| (depth(n.graph_id.as_str()), n.clone()))
        .collect();
    ordered.sort_by_key(|(d, _)| *d);
    ordered.into_iter().map(|(_, n)| n).collect()
}

fn upsert_directory(
    conn: &mut Connection,
    ctx: &GraphCoordinator<'_>,
    node: &DriveNode,
    parent_local: Option<i64>,
    local: &HashMap<String, i64>,
    counts: &mut TypeCounts,
) -> Result<i64, Error> {
    let tx = conn.unchecked_transaction()?;
    let id = if let Some(existing) = local.get(&node.graph_id).copied() {
        tx.execute(
            "UPDATE file_nodes SET parent_id = ?1, name = ?2, modified = ?3 WHERE id = ?4",
            params![parent_local, node.name, node.modified, existing],
        )?;
        counts.fetched += 1;
        existing
    } else {
        tx.execute(
            "INSERT INTO file_nodes (parent_id, node_type, blob_id, target, name, media_type,
                                      created, modified, is_subscribed, role)
             VALUES (?1, 'directory', NULL, NULL, ?2, NULL, ?3, ?4, 1, NULL)",
            params![parent_local, node.name, node.created, node.modified],
        )?;
        let new_id = tx.last_insert_rowid();
        exchange_graph_ids::insert(
            &tx,
            ctx.source_id,
            exchange_graph_ids::FILE_NODE,
            &node.graph_id,
            new_id,
        )?;
        counts.created += 1;
        new_id
    };
    tx.commit()?;
    Ok(id)
}

fn download_and_insert(
    conn: &mut Connection,
    ctx: &GraphCoordinator<'_>,
    nodes: &[DriveNode],
    by_graph_id: &HashMap<String, i64>,
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    if nodes.is_empty() {
        return Ok(());
    }
    let node_by_id: HashMap<&str, &DriveNode> =
        nodes.iter().map(|n| (n.graph_id.as_str(), n)).collect();
    let client = ctx.client.clone();
    let endpoints: crate::exchange_graph::api::Endpoints = (*ctx.endpoints).clone();
    type FetchResult = (String, Result<Vec<u8>, GraphError>);
    let pool: Pool<String, FetchResult> = Pool::new(ctx.workers, move |id: String| {
        let url = endpoints.drive_item_content(&id);
        match client.get_with_prefer(&url, Accept::Binary, &[]) {
            Ok(resp) => (id, Ok(resp.body)),
            Err(e) => (id, Err(e)),
        }
    });
    for node in nodes {
        pool.submit(node.graph_id.clone());
    }

    let mut tx_opt: Option<Transaction<'_>> = None;
    let mut in_batch = 0usize;
    for _ in 0..nodes.len() {
        let Ok((graph_id, result)) = pool.results().recv() else {
            break;
        };
        let Some(node) = node_by_id.get(graph_id.as_str()).copied() else {
            continue;
        };
        match result {
            Ok(bytes) => {
                if tx_opt.is_none() {
                    tx_opt = Some(conn.unchecked_transaction()?);
                }
                let tx = tx_opt.as_mut().expect("tx is Some");
                let parent_local = node
                    .parent_graph_id
                    .as_deref()
                    .and_then(|p| by_graph_id.get(p))
                    .copied();
                insert_file_in_tx(tx, ctx, node, parent_local, &bytes, counts)?;
                in_batch += 1;
                if in_batch >= CHUNK_SIZE {
                    if let Some(t) = tx_opt.take() {
                        t.commit()?;
                    }
                    in_batch = 0;
                }
            }
            Err(GraphError::Vanished) => counts.skipped += 1,
            Err(e) => {
                counts.failed += 1;
                ctx.logger
                    .warn(&format!("graph drive item {graph_id} download failed: {e}"));
            }
        }
    }
    if let Some(t) = tx_opt.take() {
        t.commit()?;
    }
    Ok(())
}

fn insert_file_in_tx(
    tx: &Transaction<'_>,
    ctx: &GraphCoordinator<'_>,
    node: &DriveNode,
    parent_local: Option<i64>,
    bytes: &[u8],
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let blob_id = blobs::intern_blob(tx, bytes)?;
    tx.execute(
        "INSERT INTO file_nodes (parent_id, node_type, blob_id, target, name, media_type,
                                  created, modified, is_subscribed, role)
         VALUES (?1, 'file', ?2, NULL, ?3, ?4, ?5, ?6, 1, NULL)",
        params![
            parent_local,
            blob_id,
            node.name,
            node.media_type,
            node.created,
            node.modified,
        ],
    )?;
    let new_id = tx.last_insert_rowid();
    exchange_graph_ids::insert(
        tx,
        ctx.source_id,
        exchange_graph_ids::FILE_NODE,
        &node.graph_id,
        new_id,
    )?;
    counts.created += 1;
    Ok(())
}

fn delete_vanished(
    conn: &mut Connection,
    source_id: i64,
    local: &HashMap<String, i64>,
    server_ids: &[String],
    counts: &mut TypeCounts,
    ctx: &GraphCoordinator<'_>,
) -> Result<(), Error> {
    let server: std::collections::HashSet<&str> = server_ids.iter().map(String::as_str).collect();
    let mut vanished: Vec<(&String, &i64)> = local
        .iter()
        .filter(|(graph_id, _)| !server.contains(graph_id.as_str()))
        .collect();
    let depths = node_depths(conn)?;
    vanished.sort_by_key(|(_, local_id)| std::cmp::Reverse(*depths.get(*local_id).unwrap_or(&0)));

    for (graph_id, local_id) in vanished {
        let tx = conn.unchecked_transaction()?;
        match tx.execute("DELETE FROM file_nodes WHERE id = ?1", params![local_id]) {
            Ok(_) => {
                exchange_graph_ids::delete(
                    &tx,
                    source_id,
                    exchange_graph_ids::FILE_NODE,
                    graph_id,
                )?;
                tx.commit()?;
                counts.deleted += 1;
            }
            Err(e) => {
                let _ = tx.rollback();
                ctx.logger.warn(&format!(
                    "file node {graph_id} (local id {local_id}) could not be deleted: {e}"
                ));
                counts.failed += 1;
            }
        }
    }
    Ok(())
}

fn node_depths(conn: &Connection) -> Result<HashMap<i64, usize>, Error> {
    let mut parents: HashMap<i64, Option<i64>> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT id, parent_id FROM file_nodes")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            parents.insert(row.get(0)?, row.get(1)?);
        }
    }
    let mut depths = HashMap::new();
    for id in parents.keys().copied() {
        let mut d = 0;
        let mut cursor = id;
        let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
        while seen.insert(cursor) {
            match parents.get(&cursor).copied().flatten() {
                Some(p) => {
                    d += 1;
                    cursor = p;
                }
                None => break,
            }
        }
        depths.insert(id, d);
    }
    Ok(depths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_node_rejects_items_without_file_or_folder_facet() {
        let vault = json!({"id": "V", "name": "Personal Vault", "remoteItem": {}});
        assert!(parse_node(&vault, "root").is_none());
    }

    #[test]
    fn parse_node_reads_folder_and_file_facets() {
        let folder = json!({"id": "F", "name": "Docs", "folder": {"childCount": 2}});
        let parsed = parse_node(&folder, "root").expect("folder parses");
        assert!(parsed.is_folder);

        let file = json!({
            "id": "X", "name": "a.txt", "size": 12,
            "file": {"mimeType": "text/plain"},
            "createdDateTime": "2026-01-01T00:00:00Z"
        });
        let parsed = parse_node(&file, "root").expect("file parses");
        assert!(!parsed.is_folder);
        assert_eq!(parsed.media_type.as_deref(), Some("text/plain"));
    }

    #[test]
    fn order_by_depth_puts_parents_before_children() {
        let node = |id: &str, parent: Option<&str>| DriveNode {
            graph_id: id.to_owned(),
            name: id.to_owned(),
            is_folder: true,
            media_type: None,
            created: "2026-01-01T00:00:00Z".to_owned(),
            modified: None,
            parent_graph_id: parent.map(str::to_owned),
        };
        let ordered = order_by_depth(vec![
            node("C", Some("B")),
            node("B", Some("A")),
            node("A", None),
        ]);
        let names: Vec<&str> = ordered.iter().map(|n| n.graph_id.as_str()).collect();
        assert_eq!(names, ["A", "B", "C"]);
    }
}
