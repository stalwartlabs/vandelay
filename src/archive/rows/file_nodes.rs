/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::params;
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};

use super::{FileNodes, Subscribed, Write};
use crate::archive::{BlobId, LocalId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeType {
    File,
    Directory,
    SymLink,
}

impl NodeType {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeType::File => "file",
            NodeType::Directory => "directory",
            NodeType::SymLink => "symlink",
        }
    }

    pub fn parse(name: &str) -> Option<NodeType> {
        hashify::map!(
            name.as_bytes(),
            NodeType,
            "file" => NodeType::File,
            "directory" => NodeType::Directory,
            "symlink" => NodeType::SymLink,
        )
        .copied()
    }
}

impl ToSql for NodeType {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for NodeType {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        NodeType::parse(value.as_str()?).ok_or(FromSqlError::InvalidType)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FileNodeRecord<'r> {
    pub parent: Option<LocalId>,
    pub node_type: NodeType,
    pub blob: Option<BlobId>,
    pub target: Option<&'r str>,
    pub name: &'r str,
    pub media_type: Option<&'r str>,
    pub created: &'r str,
    pub modified: Option<&'r str>,
    pub subscribed: Subscribed,
    pub role: Option<&'r str>,
    pub executable: bool,
}

impl FileNodes<'_> {
    pub fn insert(&self, f: &FileNodeRecord<'_>) -> rusqlite::Result<LocalId> {
        self.conn.execute(
            "INSERT INTO file_nodes (parent_id, node_type, blob_id, target, name, media_type,
                created, modified, is_subscribed, role, executable)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                f.parent,
                f.node_type,
                f.blob,
                f.target,
                f.name,
                f.media_type,
                f.created,
                f.modified,
                f.subscribed,
                f.role,
                f.executable
            ],
        )?;
        Ok(LocalId(self.conn.last_insert_rowid()))
    }

    pub fn update(&self, id: LocalId, f: &FileNodeRecord<'_>) -> rusqlite::Result<Write> {
        self.conn
            .execute(
                "UPDATE file_nodes SET parent_id = ?1, node_type = ?2, blob_id = ?3, target = ?4,
                    name = ?5, media_type = ?6, created = ?7, modified = ?8, is_subscribed = ?9,
                    role = ?10, executable = ?11
                 WHERE id = ?12 AND (parent_id IS NOT ?1 OR node_type IS NOT ?2 OR blob_id IS NOT ?3
                    OR target IS NOT ?4 OR name IS NOT ?5 OR media_type IS NOT ?6
                    OR created IS NOT ?7 OR modified IS NOT ?8 OR is_subscribed IS NOT ?9
                    OR role IS NOT ?10 OR executable IS NOT ?11)",
                params![
                    f.parent,
                    f.node_type,
                    f.blob,
                    f.target,
                    f.name,
                    f.media_type,
                    f.created,
                    f.modified,
                    f.subscribed,
                    f.role,
                    f.executable,
                    id
                ],
            )
            .map(Write::from_rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Archive;

    #[test]
    fn insert_update_delete() {
        let archive = Archive::open_in_memory().unwrap();
        let store = archive.store();
        let nodes = store.file_nodes();
        let dir = nodes
            .insert(&FileNodeRecord {
                parent: None,
                node_type: NodeType::Directory,
                blob: None,
                target: None,
                name: "docs",
                media_type: None,
                created: "2020-01-01T00:00:00Z",
                modified: None,
                subscribed: Subscribed::Yes,
                role: None,
                executable: false,
            })
            .unwrap();
        let blob = store.blobs().intern(b"hello").unwrap();
        let mut file = FileNodeRecord {
            parent: Some(dir),
            node_type: NodeType::File,
            blob: Some(blob),
            target: None,
            name: "a.txt",
            media_type: Some("text/plain"),
            created: "2020-01-01T00:00:00Z",
            modified: None,
            subscribed: Subscribed::Yes,
            role: None,
            executable: false,
        };
        let id = nodes.insert(&file).unwrap();
        assert_eq!(nodes.update(id, &file).unwrap(), Write::Unchanged);
        file.executable = true;
        assert_eq!(nodes.update(id, &file).unwrap(), Write::Changed);
        nodes.delete(id).unwrap();
        nodes.delete(dir).unwrap();
    }
}
