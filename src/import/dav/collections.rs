/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::borrow::Cow;
use std::collections::HashSet;

use rusqlite::{OptionalExtension, params};

use crate::archive::rows::{AddressBookRecord, CalendarRecord, Subscribed};
use crate::archive::{LocalId, SourceId, Store};
use crate::dav::discover::DiscoveredCollection;
use crate::dav::href::Href;
use crate::error::Error;
use crate::logging::Logger;
use crate::sync::TypeCounts;
use crate::sync::vanish::{Children, OnRowError, RowFailure, VanishedSweep};
use crate::types::ObjectType;

const DELETE_CALENDAR_MEMBERS: &str = "DELETE FROM calendar_events
     WHERE EXISTS (SELECT 1 FROM json_each(calendar_ids) WHERE value = ?1)";
const DELETE_ADDRESS_BOOK_MEMBERS: &str = "DELETE FROM contact_cards
     WHERE EXISTS (SELECT 1 FROM json_each(address_book_ids) WHERE value = ?1)";
const UNTITLED: &str = "Untitled";
const ALL_AVAILABILITY: &str = "all";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CollectionKind {
    Calendar,
    AddressBook,
}

impl CollectionKind {
    pub(super) const fn container(self) -> ObjectType {
        match self {
            CollectionKind::Calendar => ObjectType::Calendar,
            CollectionKind::AddressBook => ObjectType::AddressBook,
        }
    }

    pub(super) const fn item(self) -> ObjectType {
        match self {
            CollectionKind::Calendar => ObjectType::CalendarEvent,
            CollectionKind::AddressBook => ObjectType::ContactCard,
        }
    }

    fn upsert(
        self,
        store: Store<'_>,
        collection: CollectionMeta<'_>,
        existing: Option<LocalId>,
    ) -> rusqlite::Result<LocalId> {
        let name = collection.name();
        let props = &collection.0.props;
        match self {
            CollectionKind::Calendar => {
                let kept = match existing {
                    Some(id) => StoredCollection(store).calendar(id)?,
                    None => None,
                }
                .unwrap_or_default();
                let record = CalendarRecord {
                    name: &name,
                    description: props.calendar_description.as_deref(),
                    color: props.calendar_color.as_deref(),
                    sort_order: props.calendar_order.unwrap_or(0).max(0),
                    subscribed: kept.subscribed,
                    is_visible: kept.is_visible,
                    is_default: kept.is_default,
                    include_in_availability: &kept.include_in_availability,
                    default_alerts_with_time: kept.default_alerts_with_time.as_deref(),
                    default_alerts_without_time: kept.default_alerts_without_time.as_deref(),
                    time_zone: collection.time_zone(),
                };
                let calendars = store.calendars();
                match existing {
                    Some(id) => calendars.update(id, &record).map(|_| id),
                    None => calendars.insert(&record),
                }
            }
            CollectionKind::AddressBook => {
                let kept = match existing {
                    Some(id) => StoredCollection(store).address_book(id)?,
                    None => None,
                }
                .unwrap_or_default();
                let record = AddressBookRecord {
                    name: &name,
                    description: props.addressbook_description.as_deref(),
                    sort_order: kept.sort_order,
                    is_default: kept.is_default,
                    subscribed: kept.subscribed,
                };
                let books = store.address_books();
                match existing {
                    Some(id) => books.update(id, &record).map(|_| id),
                    None => books.insert(&record),
                }
            }
        }
    }

    fn delete_members(self, store: Store<'_>, container: LocalId) -> rusqlite::Result<()> {
        let sql = match self {
            CollectionKind::Calendar => DELETE_CALENDAR_MEMBERS,
            CollectionKind::AddressBook => DELETE_ADDRESS_BOOK_MEMBERS,
        };
        store.connection().execute(sql, params![container])?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct CollectionMeta<'c>(&'c DiscoveredCollection);

impl<'c> CollectionMeta<'c> {
    fn name(self) -> Cow<'c, str> {
        if let Some(name) = self.0.props.displayname.as_deref()
            && !name.trim().is_empty()
        {
            return Cow::Borrowed(name);
        }
        let component = self.0.href.last_component();
        if component.is_empty() {
            Cow::Borrowed(UNTITLED)
        } else {
            Cow::Owned(component)
        }
    }

    fn time_zone(self) -> Option<&'c str> {
        self.0
            .props
            .calendar_timezone
            .as_deref()?
            .lines()
            .filter_map(|line| line.trim().strip_prefix("TZID:"))
            .map(str::trim)
            .find(|tzid| !tzid.is_empty())
    }
}

#[derive(Clone, Copy)]
pub(super) struct CollectionSync<'a> {
    pub(super) store: Store<'a>,
    pub(super) source: SourceId,
    pub(super) logger: Logger,
}

impl CollectionSync<'_> {
    pub(super) fn reconcile<'d>(
        &self,
        kind: CollectionKind,
        discovered: &'d [DiscoveredCollection],
        counts: &mut TypeCounts,
    ) -> Result<Vec<(&'d Href, LocalId)>, Error> {
        let container = kind.container();
        let mut existing = self
            .store
            .dav_ids(self.source)
            .all(container)
            .map_err(Error::partial)?;
        let mut upserted = Vec::with_capacity(discovered.len());
        for collection in discovered {
            let href = collection.href.as_str();
            let known = existing.get(href).copied();
            let tx = self.store.begin().map_err(Error::partial)?;
            match kind.upsert(tx.store(), CollectionMeta(collection), known) {
                Ok(local) => {
                    if known.is_none() {
                        tx.store()
                            .dav_ids(self.source)
                            .insert(container, href, href, "", local)
                            .map_err(Error::partial)?;
                        counts.created += 1;
                    } else {
                        counts.fetched += 1;
                    }
                    tx.commit().map_err(Error::partial)?;
                    upserted.push((&collection.href, local));
                    self.logger
                        .progress(format_args!("Collection {href} stored as {local}"));
                }
                Err(e) => {
                    drop(tx);
                    self.logger
                        .warn(format_args!("cannot store collection {href}: {e}"));
                    counts.failed += 1;
                }
            }
        }
        let server: HashSet<&str> = discovered.iter().map(|c| c.href.as_str()).collect();
        existing.retain(|href, _| !server.contains(href.as_str()));
        let mut vanished: Vec<(String, LocalId)> = existing.into_iter().collect();
        vanished.sort_unstable();
        self.sweep(kind, &vanished, counts)?;
        Ok(upserted)
    }

    fn sweep(
        &self,
        kind: CollectionKind,
        vanished: &[(String, LocalId)],
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        let source = self.source;
        let logger = self.logger;
        VanishedSweep::new(self.store, OnRowError::Count, Children::Restrict)
            .run(
                kind.container(),
                vanished,
                counts,
                |store, href, local| {
                    let ids = store.dav_ids(source);
                    ids.delete_collection(kind.item(), href)?;
                    ids.delete_collection(kind.container(), href)?;
                    kind.delete_members(store, local)
                },
                |href, _, failure| {
                    if let RowFailure::Sql(e) = failure {
                        logger.warn(format_args!(
                            "cannot delete vanished collection {href}: {e}"
                        ));
                    }
                },
            )
            .map_err(Error::partial)
    }
}

struct KeptCalendar {
    subscribed: Subscribed,
    is_visible: bool,
    is_default: bool,
    include_in_availability: String,
    default_alerts_with_time: Option<String>,
    default_alerts_without_time: Option<String>,
}

impl Default for KeptCalendar {
    fn default() -> KeptCalendar {
        KeptCalendar {
            subscribed: Subscribed::Yes,
            is_visible: true,
            is_default: false,
            include_in_availability: ALL_AVAILABILITY.to_owned(),
            default_alerts_with_time: None,
            default_alerts_without_time: None,
        }
    }
}

struct KeptAddressBook {
    sort_order: i64,
    is_default: bool,
    subscribed: Subscribed,
}

impl Default for KeptAddressBook {
    fn default() -> KeptAddressBook {
        KeptAddressBook {
            sort_order: 0,
            is_default: false,
            subscribed: Subscribed::Yes,
        }
    }
}

#[derive(Clone, Copy)]
struct StoredCollection<'a>(Store<'a>);

impl StoredCollection<'_> {
    fn calendar(self, id: LocalId) -> rusqlite::Result<Option<KeptCalendar>> {
        self.0
            .connection()
            .prepare_cached(
                "SELECT is_subscribed, is_visible, is_default, include_in_availability,
                        default_alerts_with_time, default_alerts_without_time
                 FROM calendars WHERE id = ?1",
            )?
            .query_row([id], |row| {
                Ok(KeptCalendar {
                    subscribed: row.get(0)?,
                    is_visible: row.get(1)?,
                    is_default: row.get(2)?,
                    include_in_availability: row.get(3)?,
                    default_alerts_with_time: row.get(4)?,
                    default_alerts_without_time: row.get(5)?,
                })
            })
            .optional()
    }

    fn address_book(self, id: LocalId) -> rusqlite::Result<Option<KeptAddressBook>> {
        self.0
            .connection()
            .prepare_cached(
                "SELECT sort_order, is_default, is_subscribed FROM address_books WHERE id = ?1",
            )?
            .query_row([id], |row| {
                Ok(KeptAddressBook {
                    sort_order: row.get(0)?,
                    is_default: row.get(1)?,
                    subscribed: row.get(2)?,
                })
            })
            .optional()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Archive;
    use crate::archive::rows::{CalendarEventRecord, EventDataType};
    use crate::archive::sources::{SourceKey, SourceKind};
    use crate::dav::parse::ResourceProps;
    use crate::logging::Level;

    fn collection(href: &str, props: ResourceProps) -> DiscoveredCollection {
        DiscoveredCollection {
            url: format!("https://x{href}"),
            href: Href::from_normalised(href.to_owned()),
            props,
        }
    }

    fn count(archive: &Archive, table: &str) -> i64 {
        archive
            .connection()
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn time_zone_is_the_first_tzid() {
        let c = collection(
            "/cal/",
            ResourceProps {
                calendar_timezone: Some(
                    "BEGIN:VTIMEZONE\r\nTZID:Europe/Berlin\r\nEND:VTIMEZONE\r\n".to_owned(),
                ),
                ..Default::default()
            },
        );
        assert_eq!(CollectionMeta(&c).time_zone(), Some("Europe/Berlin"));
    }

    #[test]
    fn time_zone_is_none_when_absent() {
        let mut c = collection("/cal/", ResourceProps::default());
        assert!(CollectionMeta(&c).time_zone().is_none());
        c.props.calendar_timezone = Some("nothing here".to_owned());
        assert!(CollectionMeta(&c).time_zone().is_none());
    }

    #[test]
    fn name_falls_back_to_the_href_when_displayname_is_empty() {
        let c = collection("/dav/cal/u/work/", ResourceProps::default());
        assert_eq!(CollectionMeta(&c).name(), "work");
    }

    #[test]
    fn name_uses_the_displayname_when_present() {
        let c = collection(
            "/dav/cal/u/default/",
            ResourceProps {
                displayname: Some("Default".to_owned()),
                ..Default::default()
            },
        );
        assert_eq!(CollectionMeta(&c).name(), "Default");
    }

    #[test]
    fn vanished_calendar_cascades_events_and_sync_id_rows() {
        let archive = Archive::open_in_memory().unwrap();
        let store = archive.store();
        let source = store
            .sources()
            .upsert(
                &SourceKey {
                    kind: SourceKind::CalDav,
                    url: "https://x",
                    account_id: "u",
                },
                Some("u"),
                "u",
            )
            .unwrap();
        let sync = CollectionSync {
            store,
            source,
            logger: Logger::new(Level::Quiet),
        };
        let discovered = [collection("/cal/work/", ResourceProps::default())];
        let mut counts = TypeCounts::default();
        let upserted = sync
            .reconcile(CollectionKind::Calendar, &discovered, &mut counts)
            .unwrap();
        let [(_, calendar)] = upserted.as_slice() else {
            panic!("one calendar expected");
        };
        let event = store
            .calendar_events()
            .insert(&CalendarEventRecord {
                calendars: &[*calendar],
                is_draft: false,
                use_default_alerts: false,
                data_type: EventDataType::Event,
                data: "{}",
            })
            .unwrap();
        store
            .dav_ids(source)
            .insert(
                ObjectType::CalendarEvent,
                "/cal/work/",
                "/cal/work/e.ics",
                "\"v\"",
                event,
            )
            .unwrap();

        let mut counts = TypeCounts::default();
        sync.reconcile(CollectionKind::Calendar, &[], &mut counts)
            .unwrap();

        assert_eq!((counts.deleted, counts.failed), (1, 0));
        assert_eq!(count(&archive, "calendars"), 0, "calendar row gone");
        assert_eq!(count(&archive, "calendar_events"), 0, "event rows cascaded");
        assert_eq!(count(&archive, "sync_id_dav"), 0, "both id types cleared");
    }

    #[test]
    fn calendar_update_keeps_the_columns_dav_never_rewrites() {
        let archive = Archive::open_in_memory().unwrap();
        let store = archive.store();
        let source = store
            .sources()
            .upsert(
                &SourceKey {
                    kind: SourceKind::CalDav,
                    url: "https://x",
                    account_id: "u",
                },
                Some("u"),
                "u",
            )
            .unwrap();
        let sync = CollectionSync {
            store,
            source,
            logger: Logger::new(Level::Quiet),
        };
        let discovered = [collection("/cal/work/", ResourceProps::default())];
        sync.reconcile(
            CollectionKind::Calendar,
            &discovered,
            &mut TypeCounts::default(),
        )
        .unwrap();
        archive
            .connection()
            .execute_batch(
                "UPDATE calendars SET is_visible = 0, is_subscribed = 0,
                    include_in_availability = 'none', default_alerts_with_time = '{}'",
            )
            .unwrap();

        sync.reconcile(
            CollectionKind::Calendar,
            &discovered,
            &mut TypeCounts::default(),
        )
        .unwrap();

        let kept: (bool, bool, String, Option<String>) = archive
            .connection()
            .query_row(
                "SELECT is_visible, is_subscribed, include_in_availability,
                        default_alerts_with_time FROM calendars",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            kept,
            (false, false, "none".to_owned(), Some("{}".to_owned()))
        );
    }
}
