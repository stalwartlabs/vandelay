/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::{Connection, params};

use crate::db::dav_ids::{ADDRESS_BOOK, CALENDAR, CALENDAR_EVENT, CONTACT_CARD, FILE_NODE};
use crate::db::sources::{self, SourceKey};
use crate::error::Error;
use crate::logging::{LEVEL_DEFAULT, Logger};
use crate::sync::Summary;

const DELETE_MAPPED: [(&str, &str); 4] = [
    (
        CALENDAR_EVENT,
        "DELETE FROM calendar_events WHERE id IN
         (SELECT local_id FROM sync_id_dav WHERE source_id = ?1 AND type_name = ?2)",
    ),
    (
        CONTACT_CARD,
        "DELETE FROM contact_cards WHERE id IN
         (SELECT local_id FROM sync_id_dav WHERE source_id = ?1 AND type_name = ?2)",
    ),
    (
        CALENDAR,
        "DELETE FROM calendars WHERE id IN
         (SELECT local_id FROM sync_id_dav WHERE source_id = ?1 AND type_name = ?2)",
    ),
    (
        ADDRESS_BOOK,
        "DELETE FROM address_books WHERE id IN
         (SELECT local_id FROM sync_id_dav WHERE source_id = ?1 AND type_name = ?2)",
    ),
];

const DELETE_LEAF_FILE_NODES: &str = "DELETE FROM file_nodes WHERE id IN
     (SELECT local_id FROM sync_id_dav WHERE source_id = ?1 AND type_name = ?2)
     AND NOT EXISTS (SELECT 1 FROM file_nodes AS child WHERE child.parent_id = file_nodes.id)";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Removed {
    pub calendar_events: u64,
    pub contact_cards: u64,
    pub calendars: u64,
    pub address_books: u64,
    pub file_nodes: u64,
}

impl Removed {
    fn slot(&mut self, type_name: &str) -> Option<&mut u64> {
        match type_name {
            CALENDAR_EVENT => Some(&mut self.calendar_events),
            CONTACT_CARD => Some(&mut self.contact_cards),
            CALENDAR => Some(&mut self.calendars),
            ADDRESS_BOOK => Some(&mut self.address_books),
            FILE_NODE => Some(&mut self.file_nodes),
            _ => None,
        }
    }

    pub fn add_to(mut self, summary: &mut Summary) {
        for (label, counts) in &mut summary.per_type {
            if let Some(n) = self.slot(label) {
                counts.deleted += std::mem::take(n);
            }
        }
    }
}

pub fn remove_other_sources(
    conn: &Connection,
    key: &SourceKey,
    logger: Logger,
) -> Result<Removed, Error> {
    let others = sources::other_sources(conn, &key.kind, &key.session_url, &key.account_id)?;
    let mut removed = Removed::default();
    if others.is_empty() {
        return Ok(removed);
    }
    let tx = conn.unchecked_transaction()?;
    for (source_id, session_url, account_id) in &others {
        remove_source(&tx, *source_id, &mut removed)?;
        if logger.enabled(LEVEL_DEFAULT) {
            eprintln!(
                "import: removed {} source {session_url} (account {account_id})",
                key.kind
            );
        }
    }
    tx.commit()?;
    Ok(removed)
}

fn remove_source(
    conn: &Connection,
    source_id: i64,
    removed: &mut Removed,
) -> Result<(), rusqlite::Error> {
    for (type_name, sql) in DELETE_MAPPED {
        let n = conn.execute(sql, params![source_id, type_name])?;
        if let Some(slot) = removed.slot(type_name) {
            *slot += n as u64;
        }
    }
    let mut leaves = conn.prepare(DELETE_LEAF_FILE_NODES)?;
    loop {
        let n = leaves.execute(params![source_id, FILE_NODE])?;
        if n == 0 {
            break;
        }
        removed.file_nodes += n as u64;
    }
    sources::delete_source(conn, source_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{dav_ids, init};
    use crate::sync::TypeCounts;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        init::apply_schema(&c).unwrap();
        c.pragma_update(None, "foreign_keys", "ON").unwrap();
        c
    }

    fn source(c: &Connection, kind: &str, url: &str) -> (SourceKey, i64) {
        let key = SourceKey {
            kind: kind.to_owned(),
            session_url: url.to_owned(),
            account_id: format!("{url}/u/"),
        };
        let id = sources::upsert_source(c, &key, None, "u").unwrap();
        (key, id)
    }

    fn file_node(c: &Connection, sid: i64, parent: Option<i64>, name: &str, href: &str) -> i64 {
        c.execute(
            "INSERT INTO file_nodes (parent_id, node_type, name, created)
             VALUES (?1, 'directory', ?2, '2026-01-01T00:00:00Z')",
            params![parent, name],
        )
        .unwrap();
        let id = c.last_insert_rowid();
        dav_ids::insert(c, sid, FILE_NODE, href, href, "", id).unwrap();
        id
    }

    fn count(c: &Connection, table: &str) -> i64 {
        c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn replacing_a_webdav_source_drops_the_old_tree_leaf_first() {
        let c = mem();
        let (_, old) = source(&c, "webdav", "https://old");
        let a = file_node(&c, old, None, "a", "/a/");
        let b = file_node(&c, old, Some(a), "b", "/a/b/");
        file_node(&c, old, Some(b), "c", "/a/b/c/");
        let (new_key, new) = source(&c, "webdav", "https://new");
        file_node(&c, new, None, "a", "/a/");

        let removed = remove_other_sources(&c, &new_key, Logger::from_flags(true, 0)).unwrap();

        assert_eq!(removed.file_nodes, 3);
        assert_eq!(count(&c, "file_nodes"), 1);
        assert_eq!(count(&c, "sources"), 1);
        assert_eq!(count(&c, "sync_id_dav"), 1);
        let kept: i64 = c
            .query_row("SELECT source_id FROM sync_id_dav", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept, new);
    }

    #[test]
    fn replacing_leaves_other_kinds_untouched() {
        let c = mem();
        let (_, cal) = source(&c, "caldav", "https://old");
        c.execute("INSERT INTO calendars (name) VALUES ('Work')", [])
            .unwrap();
        let cal_local = c.last_insert_rowid();
        dav_ids::insert(&c, cal, CALENDAR, "/c/", "/c/", "", cal_local).unwrap();
        c.execute(
            "INSERT INTO calendar_events (calendar_ids, data) VALUES (?1, '{}')",
            params![format!("[{cal_local}]")],
        )
        .unwrap();
        let ev = c.last_insert_rowid();
        dav_ids::insert(&c, cal, CALENDAR_EVENT, "/c/", "/c/1.ics", "", ev).unwrap();
        let (_, old_web) = source(&c, "webdav", "https://old");
        file_node(&c, old_web, None, "a", "/a/");
        let (new_web, _) = source(&c, "webdav", "https://new");

        let removed = remove_other_sources(&c, &new_web, Logger::from_flags(true, 0)).unwrap();

        assert_eq!(removed.file_nodes, 1);
        assert_eq!(removed.calendars + removed.calendar_events, 0);
        assert_eq!(count(&c, "calendars"), 1);
        assert_eq!(count(&c, "calendar_events"), 1);
        assert_eq!(count(&c, "file_nodes"), 0);
    }

    #[test]
    fn replacing_a_caldav_source_drops_its_calendars_and_events() {
        let c = mem();
        let (_, old) = source(&c, "caldav", "https://old");
        c.execute("INSERT INTO calendars (name) VALUES ('Work')", [])
            .unwrap();
        let cal_local = c.last_insert_rowid();
        dav_ids::insert(&c, old, CALENDAR, "/c/", "/c/", "", cal_local).unwrap();
        c.execute(
            "INSERT INTO calendar_events (calendar_ids, data) VALUES (?1, '{}')",
            params![format!("[{cal_local}]")],
        )
        .unwrap();
        let ev = c.last_insert_rowid();
        dav_ids::insert(&c, old, CALENDAR_EVENT, "/c/", "/c/1.ics", "", ev).unwrap();
        let (new_key, _) = source(&c, "caldav", "https://new");

        let removed = remove_other_sources(&c, &new_key, Logger::from_flags(true, 0)).unwrap();

        assert_eq!((removed.calendars, removed.calendar_events), (1, 1));
        assert_eq!(count(&c, "calendars"), 0);
        assert_eq!(count(&c, "calendar_events"), 0);
        assert_eq!(count(&c, "sync_id_dav"), 0);
    }

    #[test]
    fn no_other_source_is_a_no_op() {
        let c = mem();
        let (key, _) = source(&c, "webdav", "https://only");
        let removed = remove_other_sources(&c, &key, Logger::from_flags(true, 0)).unwrap();
        assert_eq!(removed, Removed::default());
        assert_eq!(count(&c, "sources"), 1);
    }

    #[test]
    fn removed_counts_land_on_matching_summary_rows() {
        let mut summary = Summary::default();
        summary.per_type.push((FILE_NODE, TypeCounts::default()));
        Removed {
            file_nodes: 4,
            calendars: 9,
            ..Removed::default()
        }
        .add_to(&mut summary);
        assert_eq!(summary.per_type.len(), 1);
        assert_eq!(summary.per_type[0].1.deleted, 4);
    }
}
