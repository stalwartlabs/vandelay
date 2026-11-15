/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::Row;

use super::{IdRefs, JsonColumn, UtcText};
use crate::archive::rows::{FileNodeRecord, NodeType, Subscribed, Write};
use crate::archive::{BlobId, LocalId, Store};
use crate::jmap::error::JmapError;
use crate::jmap::wire::JmapId;
use crate::jmap::wire::file_node::{DirectoryProps, FileNode, FileProps, NodeKind, SymLinkProps};
use crate::types::ObjectType;

#[derive(Debug, Clone)]
pub(crate) struct FileNodeRow {
    pub(crate) wire: FileNode,
    pub(crate) blob: Option<BlobId>,
}

impl FileNodeRow {
    pub(crate) const SELECT: &str = "SELECT id, parent_id, node_type, blob_id, target, name, \
         media_type, created, modified, is_subscribed, role, executable FROM file_nodes";

    pub(crate) fn from_row(
        row: &Row<'_>,
        resolve: &impl Fn(ObjectType, LocalId) -> Option<JmapId>,
    ) -> Result<FileNodeRow, JmapError> {
        let parent_id = row
            .get::<_, Option<LocalId>>(1)?
            .map(|p| {
                resolve(ObjectType::FileNode, p)
                    .ok_or_else(|| JmapError::malformed("unresolved file_node parent on export"))
            })
            .transpose()?;
        let node_type: String = row.get(2)?;
        let kind = match NodeType::parse(&node_type) {
            Some(NodeType::File) => NodeKind::File(FileProps {
                blob_id: None,
                media_type: row.get(6)?,
                executable: row.get::<_, i64>(11)? != 0,
            }),
            Some(NodeType::Directory) => NodeKind::Directory(DirectoryProps {
                role: row.get(10)?,
                is_subscribed: row.get::<_, i64>(9)? != 0,
            }),
            Some(NodeType::SymLink) => NodeKind::SymLink(SymLinkProps {
                target: JsonColumn::decode(row.get(4)?)?.unwrap_or_default(),
            }),
            None => {
                return Err(JmapError::malformed(format!(
                    "unknown file_node type {node_type}"
                )));
            }
        };
        Ok(FileNodeRow {
            wire: FileNode {
                id: None,
                parent_id,
                name: row.get(5)?,
                created: UtcText::parse(&row.get::<_, String>(7)?)?,
                modified: UtcText::parse_opt(row.get(8)?)?,
                kind,
            },
            blob: row.get(3)?,
        })
    }
}

impl FileNode {
    fn with_record<R>(
        &self,
        blob: Option<BlobId>,
        resolve: &impl Fn(ObjectType, &str) -> Option<LocalId>,
        write: impl FnOnce(&FileNodeRecord<'_>) -> rusqlite::Result<R>,
    ) -> Result<R, JmapError> {
        let parent = IdRefs::parent(self.parent_id.as_ref(), ObjectType::FileNode, resolve);
        let target = match &self.kind {
            NodeKind::SymLink(l) => Some(serde_json::to_string(&l.target)?),
            NodeKind::File(_) | NodeKind::Directory(_) => None,
        };
        let created = UtcText::format(&self.created)?;
        let modified = UtcText::format_opt(self.modified.as_ref())?;
        let (node_type, media_type, executable, role, subscribed) = match &self.kind {
            NodeKind::File(f) => (
                NodeType::File,
                f.media_type.as_deref(),
                f.executable,
                None,
                Subscribed::Yes,
            ),
            NodeKind::Directory(d) => (
                NodeType::Directory,
                None,
                false,
                d.role.as_deref(),
                Subscribed::from(d.is_subscribed),
            ),
            NodeKind::SymLink(_) => (NodeType::SymLink, None, false, None, Subscribed::Yes),
        };
        Ok(write(&FileNodeRecord {
            parent,
            node_type,
            blob,
            target: target.as_deref(),
            name: &self.name,
            media_type,
            created: &created,
            modified: modified.as_deref(),
            subscribed,
            role,
            executable,
        })?)
    }

    pub(crate) fn insert(
        &self,
        store: Store<'_>,
        blob: Option<BlobId>,
        resolve: &impl Fn(ObjectType, &str) -> Option<LocalId>,
    ) -> Result<LocalId, JmapError> {
        self.with_record(blob, resolve, |r| store.file_nodes().insert(r))
    }

    pub(crate) fn update(
        &self,
        store: Store<'_>,
        id: LocalId,
        blob: Option<BlobId>,
        resolve: &impl Fn(ObjectType, &str) -> Option<LocalId>,
    ) -> Result<Write, JmapError> {
        self.with_record(blob, resolve, |r| store.file_nodes().update(id, r))
    }
}
