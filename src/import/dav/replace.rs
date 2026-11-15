/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::mem;

use crate::archive::sources::SourceKey;
use crate::archive::{SourceId, Store};
use crate::error::Error;
use crate::logging::Logger;
use crate::sync::{Summary, SummaryType};
use crate::types::ObjectType;

const PURGE_ORDER: [ObjectType; 5] = [
    ObjectType::CalendarEvent,
    ObjectType::ContactCard,
    ObjectType::Calendar,
    ObjectType::AddressBook,
    ObjectType::FileNode,
];

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Removed {
    pub calendar_events: u64,
    pub contact_cards: u64,
    pub calendars: u64,
    pub address_books: u64,
    pub file_nodes: u64,
}

impl Removed {
    pub fn other_sources(
        store: Store<'_>,
        key: &SourceKey<'_>,
        logger: Logger,
    ) -> Result<Removed, Error> {
        let others = store.sources().others(key)?;
        let mut removed = Removed::default();
        if others.is_empty() {
            return Ok(removed);
        }
        let tx = store.begin()?;
        for other in &others {
            removed.remove_source(tx.store(), other.id)?;
            logger.info(format_args!(
                "Removed the {} source {} (account {})",
                key.kind.label(),
                other.url,
                other.account_id
            ));
        }
        tx.commit()?;
        Ok(removed)
    }

    pub fn add_to(mut self, summary: &mut Summary) {
        for (ty, counts) in &mut summary.per_type {
            if let SummaryType::Object(ty) = *ty
                && let Some(n) = self.slot(ty)
            {
                counts.deleted += mem::take(n);
            }
        }
    }

    fn remove_source(&mut self, store: Store<'_>, source: SourceId) -> rusqlite::Result<()> {
        let ids = store.dav_ids(source);
        for ty in PURGE_ORDER {
            let n = ids.purge(ty)?;
            if let Some(slot) = self.slot(ty) {
                *slot += n;
            }
        }
        store.sources().delete(source)
    }

    fn slot(&mut self, ty: ObjectType) -> Option<&mut u64> {
        match ty {
            ObjectType::CalendarEvent => Some(&mut self.calendar_events),
            ObjectType::ContactCard => Some(&mut self.contact_cards),
            ObjectType::Calendar => Some(&mut self.calendars),
            ObjectType::AddressBook => Some(&mut self.address_books),
            ObjectType::FileNode => Some(&mut self.file_nodes),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::rows::{
        CalendarEventRecord, CalendarRecord, EventDataType, FileNodeRecord, NodeType, Subscribed,
    };
    use crate::archive::sources::SourceKind;
    use crate::archive::{Archive, LocalId};
    use crate::logging::Level;

    fn source<'k>(
        archive: &Archive,
        kind: SourceKind,
        url: &'k str,
        account_id: &'k str,
    ) -> (SourceKey<'k>, SourceId) {
        let key = SourceKey {
            kind,
            url,
            account_id,
        };
        let id = archive.store().sources().upsert(&key, None, "u").unwrap();
        (key, id)
    }

    fn file_node(
        archive: &Archive,
        source: SourceId,
        parent: Option<LocalId>,
        name: &str,
        href: &str,
    ) -> LocalId {
        let store = archive.store();
        let id = store
            .file_nodes()
            .insert(&FileNodeRecord {
                parent,
                node_type: NodeType::Directory,
                blob: None,
                target: None,
                name,
                media_type: None,
                created: "2026-01-01T00:00:00Z",
                modified: None,
                subscribed: Subscribed::Yes,
                role: None,
                executable: false,
            })
            .unwrap();
        store
            .dav_ids(source)
            .insert(ObjectType::FileNode, href, href, "", id)
            .unwrap();
        id
    }

    fn calendar_with_event(archive: &Archive, source: SourceId) {
        let store = archive.store();
        let calendar = store
            .calendars()
            .insert(&CalendarRecord {
                name: "Work",
                description: None,
                color: None,
                sort_order: 0,
                subscribed: Subscribed::Yes,
                is_visible: true,
                is_default: false,
                include_in_availability: "all",
                default_alerts_with_time: None,
                default_alerts_without_time: None,
                time_zone: None,
            })
            .unwrap();
        let ids = store.dav_ids(source);
        ids.insert(ObjectType::Calendar, "/c/", "/c/", "", calendar)
            .unwrap();
        let event = store
            .calendar_events()
            .insert(&CalendarEventRecord {
                calendars: &[calendar],
                is_draft: false,
                use_default_alerts: false,
                data_type: EventDataType::Event,
                data: "{}",
            })
            .unwrap();
        ids.insert(ObjectType::CalendarEvent, "/c/", "/c/1.ics", "", event)
            .unwrap();
    }

    fn count(archive: &Archive, table: &str) -> i64 {
        archive
            .connection()
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    fn quiet() -> Logger {
        Logger::new(Level::Quiet)
    }

    #[test]
    fn replacing_a_webdav_source_drops_the_old_tree_leaf_first() {
        let archive = Archive::open_in_memory().unwrap();
        let (_, old) = source(
            &archive,
            SourceKind::WebDav,
            "https://old",
            "https://old/u/",
        );
        let a = file_node(&archive, old, None, "a", "/a/");
        let b = file_node(&archive, old, Some(a), "b", "/a/b/");
        file_node(&archive, old, Some(b), "c", "/a/b/c/");
        let (new_key, new) = source(
            &archive,
            SourceKind::WebDav,
            "https://new",
            "https://new/u/",
        );
        file_node(&archive, new, None, "a", "/a/");

        let removed = Removed::other_sources(archive.store(), &new_key, quiet()).unwrap();

        assert_eq!(removed.file_nodes, 3);
        assert_eq!(count(&archive, "file_nodes"), 1);
        assert_eq!(count(&archive, "sources"), 1);
        assert_eq!(count(&archive, "sync_id_dav"), 1);
        let kept: SourceId = archive
            .connection()
            .query_row("SELECT source_id FROM sync_id_dav", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept, new);
    }

    #[test]
    fn replacing_leaves_other_kinds_untouched() {
        let archive = Archive::open_in_memory().unwrap();
        let (_, cal) = source(
            &archive,
            SourceKind::CalDav,
            "https://old",
            "https://old/u/",
        );
        calendar_with_event(&archive, cal);
        let (_, old_web) = source(
            &archive,
            SourceKind::WebDav,
            "https://old",
            "https://old/u/",
        );
        file_node(&archive, old_web, None, "a", "/a/");
        let (new_web, _) = source(
            &archive,
            SourceKind::WebDav,
            "https://new",
            "https://new/u/",
        );

        let removed = Removed::other_sources(archive.store(), &new_web, quiet()).unwrap();

        assert_eq!(removed.file_nodes, 1);
        assert_eq!(removed.calendars + removed.calendar_events, 0);
        assert_eq!(count(&archive, "calendars"), 1);
        assert_eq!(count(&archive, "calendar_events"), 1);
        assert_eq!(count(&archive, "file_nodes"), 0);
    }

    #[test]
    fn replacing_a_caldav_source_drops_its_calendars_and_events() {
        let archive = Archive::open_in_memory().unwrap();
        let (_, old) = source(
            &archive,
            SourceKind::CalDav,
            "https://old",
            "https://old/u/",
        );
        calendar_with_event(&archive, old);
        let (new_key, _) = source(
            &archive,
            SourceKind::CalDav,
            "https://new",
            "https://new/u/",
        );

        let removed = Removed::other_sources(archive.store(), &new_key, quiet()).unwrap();

        assert_eq!((removed.calendars, removed.calendar_events), (1, 1));
        assert_eq!(count(&archive, "calendars"), 0);
        assert_eq!(count(&archive, "calendar_events"), 0);
        assert_eq!(count(&archive, "sync_id_dav"), 0);
    }

    #[test]
    fn no_other_source_is_a_no_op() {
        let archive = Archive::open_in_memory().unwrap();
        let (key, _) = source(
            &archive,
            SourceKind::WebDav,
            "https://only",
            "https://only/u/",
        );
        let removed = Removed::other_sources(archive.store(), &key, quiet()).unwrap();
        assert_eq!(removed, Removed::default());
        assert_eq!(count(&archive, "sources"), 1);
    }

    #[test]
    fn removed_counts_land_on_matching_summary_rows() {
        let mut summary = Summary::default();
        summary.counts(ObjectType::FileNode);
        Removed {
            file_nodes: 4,
            calendars: 9,
            ..Removed::default()
        }
        .add_to(&mut summary);
        assert_eq!(summary.per_type.len(), 1);
        assert_eq!(
            summary.get(ObjectType::FileNode).map(|c| c.deleted),
            Some(4)
        );
    }
}
