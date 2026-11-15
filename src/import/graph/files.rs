/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::archive::rows::{FileNodeRecord, NodeType, Subscribed};
use crate::archive::{BlobId, LocalId};
use crate::console::Counted;
use crate::error::Error;
use crate::exchange::graph::client::Accept;
use crate::exchange::graph::error::GraphError;
use crate::sync::TypeCounts;
use crate::sync::dates::Rfc3339;
use crate::sync::diff::IdDiff;
use crate::sync::pool::Pool;
use crate::sync::tree::{DanglingParent, ParentMap};
use crate::types::ObjectType;

use super::coordinator::{CHUNK_SIZE, GraphCoordinator};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriveFacet {
    Folder,
    File,
}

#[derive(Debug, Clone)]
struct DriveNode {
    graph_id: String,
    name: String,
    facet: DriveFacet,
    media_type: Option<String>,
    created: String,
    modified: Option<String>,
    parent_graph_id: String,
}

impl DriveNode {
    fn parse(value: &Value, parent_graph_id: &str) -> Option<DriveNode> {
        let has = |facet: &str| value.get(facet).is_some_and(|f| !f.is_null());
        let facet = if has("folder") {
            DriveFacet::Folder
        } else if has("file") {
            DriveFacet::File
        } else {
            return None;
        };
        let text = |key: &str| value.get(key).and_then(Value::as_str);
        Some(DriveNode {
            graph_id: text("id")?.to_owned(),
            name: text("name")?.to_owned(),
            facet,
            media_type: value
                .get("file")
                .and_then(|f| f.get("mimeType"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            created: text("createdDateTime").unwrap_or(Rfc3339::EPOCH).to_owned(),
            modified: text("lastModifiedDateTime").map(str::to_owned),
            parent_graph_id: parent_graph_id.to_owned(),
        })
    }

    fn record<'r>(&'r self, parent: Option<LocalId>, blob: Option<BlobId>) -> FileNodeRecord<'r> {
        let (node_type, media_type) = match self.facet {
            DriveFacet::Folder => (NodeType::Directory, None),
            DriveFacet::File => (NodeType::File, self.media_type.as_deref()),
        };
        FileNodeRecord {
            parent,
            node_type,
            blob,
            target: None,
            name: &self.name,
            media_type,
            created: &self.created,
            modified: self.modified.as_deref(),
            subscribed: Subscribed::Yes,
            role: None,
            executable: false,
        }
    }
}

impl GraphCoordinator<'_> {
    pub fn reconcile_files(&self, counts: &mut TypeCounts) -> Result<(), Error> {
        let Some(root_id) = self.drive_root_id() else {
            self.logger.warn(format_args!(
                "no OneDrive drive for this account; files skipped"
            ));
            return Ok(());
        };
        let nodes = match self.enumerate_drive(root_id) {
            Ok(nodes) => nodes,
            Err(e) => {
                counts.failed += 1;
                self.logger
                    .warn(format_args!("OneDrive enumeration failed: {e}"));
                return Ok(());
            }
        };
        self.logger.progress(format_args!(
            "Found {} in OneDrive",
            Counted::new(nodes.len() as u64, "item", "items")
        ));

        let local = self.known(ObjectType::FileNode)?;
        let (mut folders, files): (Vec<DriveNode>, Vec<DriveNode>) = nodes
            .into_iter()
            .partition(|node| node.facet == DriveFacet::Folder);
        let mut tree = ParentMap::with_capacity(DanglingParent::Counts, folders.len());
        tree.extend(
            folders
                .iter()
                .map(|node| (node.graph_id.clone(), Some(node.parent_graph_id.clone()))),
        );
        tree.parents_first(&mut folders, |node| &node.graph_id);

        let mut by_graph_id: HashMap<&str, LocalId> = HashMap::with_capacity(folders.len());
        for node in &folders {
            let parent = by_graph_id.get(node.parent_graph_id.as_str()).copied();
            let tx = self.store.begin()?;
            let store = tx.store();
            let record = node.record(parent, None);
            let id = match local.get(&node.graph_id) {
                Some(&id) => {
                    store.file_nodes().update(id, &record)?;
                    counts.fetched += 1;
                    id
                }
                None => {
                    let id = store.file_nodes().insert(&record)?;
                    self.ids_in(store)
                        .insert(ObjectType::FileNode, &node.graph_id, id)?;
                    counts.created += 1;
                    id
                }
            };
            tx.commit()?;
            by_graph_id.insert(&node.graph_id, id);
        }

        let mut wanted = Vec::new();
        for node in &files {
            if local.contains_key(&node.graph_id) {
                counts.fetched += 1;
            } else {
                wanted.push(node);
            }
        }
        self.download_files(&wanted, &by_graph_id, counts)?;

        let server = folders
            .iter()
            .chain(&files)
            .map(|node| node.graph_id.clone());
        let mut vanished = IdDiff::compute(server, &local).vanished;
        self.parents(ObjectType::FileNode)?
            .children_first(&mut vanished, |(_, id)| id);
        self.sweep(ObjectType::FileNode, &vanished, counts)
    }

    fn drive_root_id(&self) -> Option<String> {
        let mut value = self.client.get_json(&self.endpoints.drive_root()).ok()?;
        match value.get_mut("id")?.take() {
            Value::String(id) => Some(id),
            _ => None,
        }
    }

    fn enumerate_drive(&self, root_id: String) -> Result<Vec<DriveNode>, GraphError> {
        let mut out = Vec::new();
        let mut seen = HashSet::from([root_id.clone()]);
        let mut frontier = vec![root_id];
        while let Some(parent) = frontier.pop() {
            let url = self.endpoints.drive_children(&parent, self.top);
            for child in self.client.collect_all_values(&url, &[])? {
                let Some(node) = DriveNode::parse(&child, &parent) else {
                    continue;
                };
                if !seen.insert(node.graph_id.clone()) {
                    continue;
                }
                if node.facet == DriveFacet::Folder {
                    frontier.push(node.graph_id.clone());
                }
                out.push(node);
            }
        }
        Ok(out)
    }

    fn download_files(
        &self,
        nodes: &[&DriveNode],
        by_graph_id: &HashMap<&str, LocalId>,
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        if nodes.is_empty() {
            return Ok(());
        }
        let node_by_id: HashMap<&str, &DriveNode> = nodes
            .iter()
            .map(|node| (node.graph_id.as_str(), *node))
            .collect();
        let client = self.client.clone();
        let endpoints = self.endpoints.clone();
        let pool: Pool<String, (String, Result<Vec<u8>, GraphError>)> =
            Pool::new(self.workers, move |id: String| {
                let result = client
                    .get_with_prefer(&endpoints.drive_item_content(&id), Accept::Binary, &[])
                    .map(|response| response.body);
                (id, result)
            });
        for node in nodes {
            pool.submit(node.graph_id.clone());
        }

        let mut batch = None;
        let mut in_batch = 0usize;
        for _ in 0..nodes.len() {
            let Ok((graph_id, result)) = pool.results().recv() else {
                break;
            };
            let Some(node) = node_by_id.get(graph_id.as_str()).copied() else {
                continue;
            };
            let bytes = match result {
                Ok(bytes) => bytes,
                Err(GraphError::Vanished) => {
                    counts.skipped += 1;
                    continue;
                }
                Err(e) => {
                    counts.failed += 1;
                    self.logger.warn(format_args!(
                        "OneDrive item {graph_id} download failed: {e}"
                    ));
                    continue;
                }
            };
            let tx = match batch.take() {
                Some(tx) => tx,
                None => self.store.begin()?,
            };
            let store = tx.store();
            let parent = by_graph_id.get(node.parent_graph_id.as_str()).copied();
            let blob = store.blobs().intern(&bytes)?;
            let id = store
                .file_nodes()
                .insert(&node.record(parent, Some(blob)))?;
            self.ids_in(store)
                .insert(ObjectType::FileNode, &node.graph_id, id)?;
            counts.created += 1;
            in_batch += 1;
            if in_batch >= CHUNK_SIZE {
                tx.commit()?;
                in_batch = 0;
            } else {
                batch = Some(tx);
            }
        }
        if let Some(tx) = batch {
            tx.commit()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parse_rejects_items_without_file_or_folder_facet() {
        let vault = json!({"id": "V", "name": "Personal Vault", "remoteItem": {}});
        assert!(DriveNode::parse(&vault, "root").is_none());
    }

    #[test]
    fn parse_reads_folder_and_file_facets() {
        let folder = json!({"id": "F", "name": "Docs", "folder": {"childCount": 2}});
        let parsed = DriveNode::parse(&folder, "root").expect("folder parses");
        assert_eq!(parsed.facet, DriveFacet::Folder);

        let file = json!({
            "id": "X", "name": "a.txt", "size": 12,
            "file": {"mimeType": "text/plain"},
            "createdDateTime": "2026-01-01T00:00:00Z"
        });
        let parsed = DriveNode::parse(&file, "root").expect("file parses");
        assert_eq!(parsed.facet, DriveFacet::File);
        assert_eq!(parsed.media_type.as_deref(), Some("text/plain"));
    }
}
