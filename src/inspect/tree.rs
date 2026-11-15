/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write;

use rusqlite::{OptionalExtension, params};

use super::View;
use crate::archive::rows::NodeType;
use crate::archive::{BlobId, LocalId};
use crate::console::{ByteSize, Counted, Grouped, Initial, Tone};
use crate::error::Error;
use crate::types::ObjectType;

const DEFAULT_MEDIA_TYPE: &str = "application/octet-stream";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Branch {
    Middle,
    Last,
}

impl Branch {
    fn connector(self) -> &'static str {
        match self {
            Branch::Middle => "├── ",
            Branch::Last => "└── ",
        }
    }

    fn indent(self) -> &'static str {
        match self {
            Branch::Middle => "│   ",
            Branch::Last => "    ",
        }
    }
}

struct MailboxNode {
    id: LocalId,
    parent_id: Option<LocalId>,
    name: String,
    role: Option<String>,
    sort_order: i64,
    msg_count: u64,
}

struct FileNode {
    id: LocalId,
    parent_id: Option<LocalId>,
    node_type: Option<NodeType>,
    blob_id: Option<BlobId>,
    name: String,
    media_type: Option<String>,
    target: Option<String>,
    child_count: u64,
}

impl FileNode {
    fn is_directory(&self) -> bool {
        self.node_type == Some(NodeType::Directory)
    }
}

trait TreeNode {
    fn id(&self) -> LocalId;
    fn parent(&self) -> Option<LocalId>;
}

impl TreeNode for MailboxNode {
    fn id(&self) -> LocalId {
        self.id
    }

    fn parent(&self) -> Option<LocalId> {
        self.parent_id
    }
}

impl TreeNode for FileNode {
    fn id(&self) -> LocalId {
        self.id
    }

    fn parent(&self) -> Option<LocalId> {
        self.parent_id
    }
}

struct Forest<'n, N> {
    children: HashMap<Option<LocalId>, Vec<&'n N>>,
}

impl<'n, N: TreeNode> Forest<'n, N> {
    fn new(nodes: &'n [N], order: impl Fn(&&'n N, &&'n N) -> Ordering) -> Forest<'n, N> {
        let mut children: HashMap<Option<LocalId>, Vec<&'n N>> = HashMap::new();
        for n in nodes {
            children.entry(n.parent()).or_default().push(n);
        }
        for siblings in children.values_mut() {
            siblings.sort_by(&order);
        }
        Forest { children }
    }

    fn walk(
        &self,
        parent: Option<LocalId>,
        prefix: &mut String,
        visit: &mut impl FnMut(&'n N, &str, Branch) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let Some((last, rest)) = self.children.get(&parent).and_then(|c| c.split_last()) else {
            return Ok(());
        };
        let siblings = rest
            .iter()
            .map(|n| (*n, Branch::Middle))
            .chain([(*last, Branch::Last)]);
        for (node, branch) in siblings {
            visit(node, prefix, branch)?;
            let len = prefix.len();
            prefix.push_str(branch.indent());
            self.walk(Some(node.id()), prefix, visit)?;
            prefix.truncate(len);
        }
        Ok(())
    }
}

impl View<'_> {
    pub(super) fn write_mailboxes(self, out: &mut impl Write) -> Result<(), Error> {
        let styled = self.styled;
        let nodes = self.mailboxes()?;
        self.tree_header(out, ObjectType::Mailbox, nodes.len())?;
        if nodes.is_empty() {
            return Ok(());
        }
        let forest = Forest::new(&nodes, |a, b| {
            a.sort_order
                .cmp(&b.sort_order)
                .then_with(|| a.name.cmp(&b.name))
        });
        let mut meta = String::new();
        forest.walk(None, &mut String::new(), &mut |node, prefix, branch| {
            meta.clear();
            let messages = Counted::new(node.msg_count, "message", "messages");
            let _ = match &node.role {
                Some(role) => write!(meta, "({role}, {messages})"),
                None => write!(meta, "({messages})"),
            };
            writeln!(
                out,
                "{}{}  {}",
                styled.paint(Tone::Dim, format_args!("{prefix}{}", branch.connector())),
                node.name,
                styled.paint(Tone::Dim, &meta)
            )?;
            Ok(())
        })
    }

    pub(super) fn write_file_nodes(self, out: &mut impl Write) -> Result<(), Error> {
        let styled = self.styled;
        let nodes = self.file_nodes()?;
        self.tree_header(out, ObjectType::FileNode, nodes.len())?;
        if nodes.is_empty() {
            return Ok(());
        }
        let forest = Forest::new(&nodes, |a, b| {
            (!a.is_directory())
                .cmp(&!b.is_directory())
                .then_with(|| a.name.cmp(&b.name))
        });
        let mut meta = String::new();
        forest.walk(None, &mut String::new(), &mut |node, prefix, branch| {
            meta.clear();
            match node.node_type {
                Some(NodeType::Directory) => {
                    let _ = write!(
                        meta,
                        "({})",
                        Counted::new(node.child_count, "item", "items")
                    );
                }
                Some(NodeType::SymLink) => {
                    let _ = write!(meta, "-> {}", node.target.as_deref().unwrap_or("?"));
                }
                Some(NodeType::File) | None => {
                    let media = node.media_type.as_deref().unwrap_or(DEFAULT_MEDIA_TYPE);
                    let _ = match node.blob_id.map(|b| self.blob_len(b)).transpose()? {
                        Some(Some(len)) => write!(meta, "({media}, {})", ByteSize(len)),
                        Some(None) => write!(meta, "({media}, missing content)"),
                        None => write!(meta, "({media}, no content)"),
                    };
                }
            }
            let slash = if node.is_directory() { "/" } else { "" };
            writeln!(
                out,
                "{}{}{slash}  {}",
                styled.paint(Tone::Dim, format_args!("{prefix}{}", branch.connector())),
                node.name,
                styled.paint(Tone::Dim, &meta)
            )?;
            Ok(())
        })
    }

    fn tree_header(self, out: &mut impl Write, ty: ObjectType, n: usize) -> Result<(), Error> {
        writeln!(
            out,
            "{}  ({})",
            self.styled.paint(Tone::Title, ty.label()),
            Grouped(n as u64)
        )?;
        if n == 0 {
            writeln!(
                out,
                "{}",
                self.styled.paint(
                    Tone::Dim,
                    format_args!("No {} in the archive", Initial::Lower(ty.label()))
                )
            )?;
        }
        Ok(())
    }

    fn blob_len(self, blob: BlobId) -> Result<Option<u64>, Error> {
        Ok(self
            .conn
            .prepare_cached("SELECT length(data) FROM blobs WHERE id = ?1")?
            .query_row(params![blob], |r| r.get::<_, i64>(0))
            .optional()?
            .map(|len| len.max(0) as u64))
    }

    fn mailboxes(self) -> Result<Vec<MailboxNode>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.parent_id, m.name, m.role, m.sort_order,
                    (SELECT COUNT(*) FROM emails e
                     WHERE EXISTS (SELECT 1 FROM json_each(e.mailbox_ids) j
                                    WHERE CAST(j.value AS INTEGER) = m.id))
               FROM mailboxes m",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(MailboxNode {
                id: r.get(0)?,
                parent_id: r.get(1)?,
                name: r.get(2)?,
                role: r.get(3)?,
                sort_order: r.get(4)?,
                msg_count: r.get::<_, i64>(5)? as u64,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    fn file_nodes(self) -> Result<Vec<FileNode>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT f.id, f.parent_id, f.node_type, f.blob_id, f.name, f.media_type, f.target,
                    (SELECT COUNT(*) FROM file_nodes c WHERE c.parent_id = f.id)
               FROM file_nodes f",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(FileNode {
                id: r.get(0)?,
                parent_id: r.get(1)?,
                node_type: NodeType::parse(r.get_ref(2)?.as_str()?),
                blob_id: r.get(3)?,
                name: r.get(4)?,
                media_type: r.get(5)?,
                target: r.get(6)?,
                child_count: r.get::<_, i64>(7)? as u64,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Archive;
    use crate::console::Styled;
    use rusqlite::{Connection, params};

    fn insert_mailbox(
        c: &Connection,
        name: &str,
        parent: Option<i64>,
        role: Option<&str>,
        sort: i64,
    ) -> i64 {
        c.execute(
            "INSERT INTO mailboxes (name, parent_id, role, sort_order, is_subscribed)
             VALUES (?1, ?2, ?3, ?4, 1)",
            params![name, parent, role, sort],
        )
        .unwrap();
        c.last_insert_rowid()
    }

    fn insert_email_in(a: &Archive, mailbox_id: i64) {
        let blob = a
            .store()
            .blobs()
            .intern(format!("seed-{mailbox_id}").as_bytes())
            .unwrap();
        a.connection()
            .execute(
                "INSERT INTO emails (blob_id, received_at, mailbox_ids, keywords)
                 VALUES (?1, '2024-01-01T00:00:00Z', ?2, '[]')",
                params![blob, format!("[{mailbox_id}]")],
            )
            .unwrap();
    }

    #[test]
    fn mailbox_tree_draws_box_characters_and_message_counts() {
        let a = Archive::open_in_memory().unwrap();
        let c = a.connection();
        let inbox = insert_mailbox(c, "Inbox", None, Some("inbox"), 0);
        let _archived = insert_mailbox(c, "Archived", Some(inbox), None, 0);
        let _y2024 = insert_mailbox(c, "2024", Some(inbox), None, 1);
        let _sent = insert_mailbox(c, "Sent", None, Some("sent"), 1);
        insert_email_in(&a, inbox);
        insert_email_in(&a, inbox);
        let mut buf = Vec::new();
        View::new(c, Styled::Plain)
            .write_mailboxes(&mut buf)
            .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(
            s,
            "Mailboxes  (4)\n\
             ├── Inbox  (inbox, 2 messages)\n\
             │   ├── Archived  (0 messages)\n\
             │   └── 2024  (0 messages)\n\
             └── Sent  (sent, 0 messages)\n"
        );
    }

    #[test]
    fn mailbox_tree_empty_says_none() {
        let a = Archive::open_in_memory().unwrap();
        let mut buf = Vec::new();
        View::new(a.connection(), Styled::Plain)
            .write_mailboxes(&mut buf)
            .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("Mailboxes  (0)"));
        assert!(s.contains("No mailboxes in the archive"));
    }

    #[test]
    fn file_node_tree_distinguishes_dirs_files_symlinks() {
        let a = Archive::open_in_memory().unwrap();
        let c = a.connection();
        c.execute(
            "INSERT INTO file_nodes (parent_id, node_type, name, created)
             VALUES (NULL, 'directory', 'Documents', '2024-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        let docs = c.last_insert_rowid();
        let blob = a.store().blobs().intern(b"hello world").unwrap();
        c.execute(
            "INSERT INTO file_nodes (parent_id, node_type, blob_id, name, media_type, created)
             VALUES (?1, 'file', ?2, 'note.txt', 'text/plain', '2024-01-01T00:00:00Z')",
            params![docs, blob],
        )
        .unwrap();
        c.execute(
            "INSERT INTO file_nodes (parent_id, node_type, target, name, created)
             VALUES (?1, 'symlink', '[\"..\",\"etc\"]', 'shortcut', '2024-01-01T00:00:00Z')",
            params![docs],
        )
        .unwrap();
        let mut buf = Vec::new();
        View::new(c, Styled::Plain)
            .write_file_nodes(&mut buf)
            .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("File nodes  (3)"));
        assert!(s.contains("└── Documents/  (2 items)"), "got:\n{s}");
        assert!(
            s.contains("    ├── note.txt  (text/plain, 11 B)"),
            "got:\n{s}"
        );
        assert!(
            s.contains("    └── shortcut  -> [\"..\",\"etc\"]"),
            "got:\n{s}"
        );
    }
}
