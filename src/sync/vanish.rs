/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::params;

use crate::archive::{LocalId, Store};
use crate::sync::TypeCounts;
use crate::types::ObjectType;

const SAVEPOINT: &str = "SAVEPOINT vanished_row";
const RELEASE: &str = "RELEASE vanished_row";
const ROLLBACK: &str = "ROLLBACK TO vanished_row; RELEASE vanished_row";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnRowError {
    Abort,
    Count,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Children {
    Restrict,
    DetachSurvivors,
    RefuseSubfolders,
}

#[derive(Debug)]
pub enum RowFailure<'e> {
    Sql(&'e rusqlite::Error),
    LiveChildren,
}

enum RowOutcome {
    Removed,
    LiveChildren,
}

struct Statements {
    delete: String,
    children: Option<String>,
}

#[derive(Clone, Copy)]
pub struct VanishedSweep<'a> {
    store: Store<'a>,
    on_row_error: OnRowError,
    children: Children,
}

impl<'a> VanishedSweep<'a> {
    pub fn new(
        store: Store<'a>,
        on_row_error: OnRowError,
        children: Children,
    ) -> VanishedSweep<'a> {
        VanishedSweep {
            store,
            on_row_error,
            children,
        }
    }

    pub fn run<K>(
        &self,
        ty: ObjectType,
        vanished: &[(K, LocalId)],
        counts: &mut TypeCounts,
        forget: impl FnMut(Store<'_>, &K, LocalId) -> rusqlite::Result<()>,
        report: impl FnMut(&K, LocalId, RowFailure<'_>),
    ) -> rusqlite::Result<()> {
        self.run_then(ty, vanished, counts, forget, report, |_| Ok(()))
    }

    pub fn run_then<K>(
        &self,
        ty: ObjectType,
        vanished: &[(K, LocalId)],
        counts: &mut TypeCounts,
        mut forget: impl FnMut(Store<'_>, &K, LocalId) -> rusqlite::Result<()>,
        mut report: impl FnMut(&K, LocalId, RowFailure<'_>),
        epilogue: impl FnOnce(Store<'_>) -> rusqlite::Result<()>,
    ) -> rusqlite::Result<()> {
        if vanished.is_empty() {
            return Ok(());
        }
        let table = ty.table();
        let statements = Statements {
            delete: format!("DELETE FROM {table} WHERE id = ?1"),
            children: match self.children {
                Children::Restrict => None,
                Children::DetachSurvivors => Some(format!(
                    "UPDATE {table} SET parent_id = NULL WHERE parent_id = ?1"
                )),
                Children::RefuseSubfolders => Some(format!(
                    "SELECT EXISTS (SELECT 1 FROM {table} WHERE parent_id = ?1)"
                )),
            },
        };
        let tx = self.store.begin()?;
        let store = tx.store();
        let conn = store.connection();
        for (key, id) in vanished {
            let outcome = match self.on_row_error {
                OnRowError::Abort => self.remove_row(store, &statements, key, *id, &mut forget)?,
                OnRowError::Count => {
                    conn.execute_batch(SAVEPOINT)?;
                    match self.remove_row(store, &statements, key, *id, &mut forget) {
                        Ok(outcome) => {
                            conn.execute_batch(RELEASE)?;
                            outcome
                        }
                        Err(e) => {
                            conn.execute_batch(ROLLBACK)?;
                            counts.failed += 1;
                            report(key, *id, RowFailure::Sql(&e));
                            continue;
                        }
                    }
                }
            };
            match outcome {
                RowOutcome::Removed => counts.deleted += 1,
                RowOutcome::LiveChildren => {
                    counts.failed += 1;
                    report(key, *id, RowFailure::LiveChildren);
                }
            }
        }
        epilogue(store)?;
        tx.commit()
    }

    fn remove_row<K>(
        &self,
        store: Store<'_>,
        statements: &Statements,
        key: &K,
        id: LocalId,
        forget: &mut impl FnMut(Store<'_>, &K, LocalId) -> rusqlite::Result<()>,
    ) -> rusqlite::Result<RowOutcome> {
        let conn = store.connection();
        if let Some(sql) = &statements.children {
            match self.children {
                Children::RefuseSubfolders => {
                    let live: bool = conn
                        .prepare_cached(sql)?
                        .query_row(params![id], |row| row.get(0))?;
                    if live {
                        return Ok(RowOutcome::LiveChildren);
                    }
                }
                Children::DetachSurvivors => {
                    conn.prepare_cached(sql)?.execute(params![id])?;
                }
                Children::Restrict => {}
            }
        }
        forget(store, key, id)?;
        conn.prepare_cached(&statements.delete)?
            .execute(params![id])?;
        Ok(RowOutcome::Removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Archive;
    use crate::archive::rows::{MailboxRecord, Subscribed};

    fn mailbox(archive: &Archive, name: &str, parent: Option<LocalId>) -> LocalId {
        archive
            .store()
            .mailboxes()
            .insert(&MailboxRecord {
                name,
                parent,
                role: None,
                sort_order: 0,
                subscribed: Subscribed::Yes,
            })
            .unwrap()
    }

    fn names(archive: &Archive) -> Vec<String> {
        let conn = archive.connection();
        let mut stmt = conn
            .prepare("SELECT name FROM mailboxes ORDER BY name")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn sweep(archive: &Archive, on: OnRowError, children: Children) -> VanishedSweep<'_> {
        VanishedSweep::new(archive.store(), on, children)
    }

    #[test]
    fn restrict_counts_a_parent_with_live_children_as_failed() {
        let archive = Archive::open_in_memory().unwrap();
        let parent = mailbox(&archive, "A", None);
        mailbox(&archive, "B", Some(parent));
        let lone = mailbox(&archive, "C", None);
        let mut counts = TypeCounts::default();
        let mut forgotten = Vec::new();
        let mut failures = Vec::new();
        sweep(&archive, OnRowError::Count, Children::Restrict)
            .run(
                ObjectType::Mailbox,
                &[("a", parent), ("c", lone)],
                &mut counts,
                |_, key, _| {
                    forgotten.push(*key);
                    Ok(())
                },
                |key, _, failure| failures.push((*key, matches!(failure, RowFailure::Sql(_)))),
            )
            .unwrap();
        assert_eq!((counts.deleted, counts.failed), (1, 1));
        assert_eq!(failures, [("a", true)]);
        assert_eq!(names(&archive), ["A", "B"]);
        assert_eq!(forgotten, ["a", "c"]);
    }

    #[test]
    fn a_failed_row_rolls_back_its_forget() {
        let archive = Archive::open_in_memory().unwrap();
        let parent = mailbox(&archive, "A", None);
        mailbox(&archive, "B", Some(parent));
        let mut counts = TypeCounts::default();
        sweep(&archive, OnRowError::Count, Children::Restrict)
            .run(
                ObjectType::Mailbox,
                &[("a", parent)],
                &mut counts,
                |store, _, _| {
                    store
                        .connection()
                        .execute(
                            "INSERT INTO identities (name, email) VALUES ('x', 'x@x')",
                            [],
                        )
                        .map(drop)
                },
                |_, _, _| {},
            )
            .unwrap();
        let n: i64 = archive
            .connection()
            .query_row("SELECT count(*) FROM identities", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn detach_survivors_keeps_children_as_roots() {
        let archive = Archive::open_in_memory().unwrap();
        let parent = mailbox(&archive, "A", None);
        let child = mailbox(&archive, "B", Some(parent));
        let mut counts = TypeCounts::default();
        sweep(&archive, OnRowError::Abort, Children::DetachSurvivors)
            .run(
                ObjectType::Mailbox,
                &[("a", parent)],
                &mut counts,
                |_, _, _| Ok(()),
                |_, _, _| {},
            )
            .unwrap();
        assert_eq!(counts.deleted, 1);
        let parent_of_child: Option<LocalId> = archive
            .connection()
            .query_row(
                "SELECT parent_id FROM mailboxes WHERE id = ?1",
                [child],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(parent_of_child, None);
    }

    #[test]
    fn refuse_subfolders_skips_and_continues() {
        let archive = Archive::open_in_memory().unwrap();
        let parent = mailbox(&archive, "A", None);
        mailbox(&archive, "B", Some(parent));
        let lone = mailbox(&archive, "C", None);
        let mut counts = TypeCounts::default();
        let mut refused = Vec::new();
        sweep(&archive, OnRowError::Abort, Children::RefuseSubfolders)
            .run(
                ObjectType::Mailbox,
                &[("a", parent), ("c", lone)],
                &mut counts,
                |_, _, _| Ok(()),
                |key, _, failure| refused.push((*key, matches!(failure, RowFailure::LiveChildren))),
            )
            .unwrap();
        assert_eq!((counts.deleted, counts.failed), (1, 1));
        assert_eq!(refused, [("a", true)]);
        assert_eq!(names(&archive), ["A", "B"]);
    }

    #[test]
    fn abort_propagates_and_rolls_back_everything() {
        let archive = Archive::open_in_memory().unwrap();
        let lone = mailbox(&archive, "C", None);
        let parent = mailbox(&archive, "A", None);
        mailbox(&archive, "B", Some(parent));
        let mut counts = TypeCounts::default();
        let result = sweep(&archive, OnRowError::Abort, Children::Restrict).run(
            ObjectType::Mailbox,
            &[("c", lone), ("a", parent)],
            &mut counts,
            |_, _, _| Ok(()),
            |_, _, _| {},
        );
        assert!(result.is_err());
        assert_eq!(names(&archive), ["A", "B", "C"]);
    }
}
