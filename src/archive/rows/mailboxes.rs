/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::params;

use super::{Mailboxes, Subscribed, Write};
use crate::archive::LocalId;

#[derive(Debug, Clone, Copy)]
pub struct MailboxRecord<'r> {
    pub name: &'r str,
    pub parent: Option<LocalId>,
    pub role: Option<&'r str>,
    pub sort_order: i64,
    pub subscribed: Subscribed,
}

impl Mailboxes<'_> {
    pub fn insert(&self, m: &MailboxRecord<'_>) -> rusqlite::Result<LocalId> {
        let role = self.unique_role(m.role, None)?;
        self.conn.execute(
            "INSERT INTO mailboxes (name, parent_id, role, sort_order, is_subscribed)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![m.name, m.parent, role, m.sort_order, m.subscribed],
        )?;
        Ok(LocalId(self.conn.last_insert_rowid()))
    }

    pub fn update(&self, id: LocalId, m: &MailboxRecord<'_>) -> rusqlite::Result<Write> {
        let role = self.unique_role(m.role, Some(id))?;
        self.conn
            .execute(
                "UPDATE mailboxes SET name = ?1, parent_id = ?2, role = ?3, sort_order = ?4,
                    is_subscribed = ?5
                 WHERE id = ?6 AND (name IS NOT ?1 OR parent_id IS NOT ?2 OR role IS NOT ?3
                    OR sort_order IS NOT ?4 OR is_subscribed IS NOT ?5)",
                params![m.name, m.parent, role, m.sort_order, m.subscribed, id],
            )
            .map(Write::from_rows)
    }

    pub fn unique_role<'r>(
        &self,
        role: Option<&'r str>,
        exclude: Option<LocalId>,
    ) -> rusqlite::Result<Option<&'r str>> {
        let Some(r) = role else {
            return Ok(None);
        };
        let taken: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM mailboxes WHERE role = ?1 AND (?2 IS NULL OR id != ?2))",
            params![r, exclude],
            |row| row.get(0),
        )?;
        Ok((!taken).then_some(r))
    }

    pub fn detach_children(&self, id: LocalId) -> rusqlite::Result<usize> {
        self.conn.execute(
            "UPDATE mailboxes SET parent_id = NULL WHERE parent_id = ?1",
            [id],
        )
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::params;

    use super::*;
    use crate::archive::Archive;

    fn record<'r>(name: &'r str, role: Option<&'r str>) -> MailboxRecord<'r> {
        MailboxRecord {
            name,
            parent: None,
            role,
            sort_order: 0,
            subscribed: Subscribed::Yes,
        }
    }

    #[test]
    fn insert_drops_a_role_already_taken() {
        let archive = Archive::open_in_memory().unwrap();
        let boxes = archive.store().mailboxes();
        let first = boxes.insert(&record("Sent", Some("sent"))).unwrap();
        let second = boxes.insert(&record("Sent 2", Some("sent"))).unwrap();
        let role = |id: LocalId| -> Option<String> {
            archive
                .connection()
                .query_row("SELECT role FROM mailboxes WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .unwrap()
        };
        assert_eq!(role(first).as_deref(), Some("sent"));
        assert_eq!(role(second), None);
    }

    #[test]
    fn update_reports_whether_the_row_changed() {
        let archive = Archive::open_in_memory().unwrap();
        let boxes = archive.store().mailboxes();
        let id = boxes.insert(&record("Inbox", Some("inbox"))).unwrap();
        assert_eq!(
            boxes.update(id, &record("Inbox", Some("inbox"))).unwrap(),
            Write::Unchanged
        );
        assert_eq!(
            boxes.update(id, &record("INBOX", Some("inbox"))).unwrap(),
            Write::Changed
        );
    }

    #[test]
    fn detach_children_then_delete_parent() {
        let archive = Archive::open_in_memory().unwrap();
        let boxes = archive.store().mailboxes();
        let parent = boxes.insert(&record("Parent", None)).unwrap();
        let child = MailboxRecord {
            parent: Some(parent),
            ..record("Child", None)
        };
        boxes.insert(&child).unwrap();
        assert!(boxes.delete(parent).is_err());
        assert_eq!(boxes.detach_children(parent).unwrap(), 1);
        boxes.delete(parent).unwrap();
    }

    fn insert_raw(archive: &Archive, name: &str, role: Option<&str>) -> LocalId {
        let conn = archive.connection();
        conn.execute(
            "INSERT INTO mailboxes (name, parent_id, role, sort_order, is_subscribed)
             VALUES (?1, NULL, ?2, 0, 1)",
            params![name, role],
        )
        .unwrap();
        LocalId::from_raw(conn.last_insert_rowid())
    }

    #[test]
    fn first_claimant_keeps_role_second_is_dropped() {
        let archive = Archive::open_in_memory().unwrap();
        let mailboxes = archive.store().mailboxes();
        let first = mailboxes.unique_role(Some("sent"), None).unwrap();
        assert_eq!(first, Some("sent"));
        insert_raw(&archive, "Sent", first);

        assert_eq!(mailboxes.unique_role(Some("sent"), None).unwrap(), None);
    }

    #[test]
    fn null_role_stays_null() {
        let archive = Archive::open_in_memory().unwrap();
        let mailboxes = archive.store().mailboxes();
        assert_eq!(mailboxes.unique_role(None, None).unwrap(), None);
    }

    #[test]
    fn distinct_roles_are_independent() {
        let archive = Archive::open_in_memory().unwrap();
        insert_raw(&archive, "Sent", Some("sent"));
        let mailboxes = archive.store().mailboxes();
        assert_eq!(
            mailboxes.unique_role(Some("trash"), None).unwrap(),
            Some("trash")
        );
    }

    #[test]
    fn update_excludes_own_row() {
        let archive = Archive::open_in_memory().unwrap();
        let id = insert_raw(&archive, "Sent", Some("sent"));
        let mailboxes = archive.store().mailboxes();
        assert_eq!(
            mailboxes.unique_role(Some("sent"), Some(id)).unwrap(),
            Some("sent")
        );
    }

    #[test]
    fn update_detects_other_holder() {
        let archive = Archive::open_in_memory().unwrap();
        insert_raw(&archive, "Sent", Some("sent"));
        let other = insert_raw(&archive, "Envoyés", None);
        let mailboxes = archive.store().mailboxes();
        assert_eq!(
            mailboxes.unique_role(Some("sent"), Some(other)).unwrap(),
            None
        );
    }
}
