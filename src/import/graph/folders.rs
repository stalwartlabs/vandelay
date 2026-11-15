/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::archive::LocalId;
use crate::archive::rows::{AddressBookRecord, CalendarRecord, MailboxRecord, Subscribed};
use crate::console::Counted;
use crate::error::Error;
use crate::exchange::graph::calendar_map::CalendarColor;
use crate::exchange::graph::types::MailboxKind;
use crate::exchange::tz::ZoneId;
use crate::sync::TypeCounts;
use crate::sync::diff::IdDiff;
use crate::sync::tree::{DanglingParent, ParentMap};
use crate::types::ObjectType;

use super::coordinator::{CHUNK_SIZE, GraphCoordinator, GraphImport};

const WELL_KNOWN_ROLES: [(&str, &str); 6] = [
    ("inbox", "inbox"),
    ("drafts", "drafts"),
    ("sentitems", "sent"),
    ("deleteditems", "trash"),
    ("junkemail", "junk"),
    ("archive", "archive"),
];

#[derive(Debug, Clone)]
pub struct GraphFolder {
    pub graph_id: String,
    pub local_id: LocalId,
}

#[derive(Clone, Copy)]
struct FolderValue<'v>(&'v Value);

impl<'v> FolderValue<'v> {
    fn str(self, key: &str) -> Option<&'v str> {
        self.0.get(key).and_then(Value::as_str)
    }

    fn id(self) -> Option<&'v str> {
        self.str("id")
    }

    fn flag(self, key: &str) -> bool {
        self.0.get(key).and_then(Value::as_bool).unwrap_or(false)
    }
}

impl GraphCoordinator<'_> {
    pub fn reconcile_mail_folders(
        &self,
        mailbox_kind: MailboxKind,
        counts: &mut TypeCounts,
    ) -> Result<Vec<GraphFolder>, Error> {
        let server = GraphImport::enumerate_mail_folders(
            self.client,
            self.endpoints,
            mailbox_kind,
            self.top,
        )?;
        self.logger.progress(format_args!(
            "Found {}",
            Counted::new(server.len() as u64, "mailbox", "mailboxes")
        ));
        let well_known = match mailbox_kind {
            MailboxKind::Primary => self.well_known_roles(),
            MailboxKind::Archive => HashMap::new(),
        };
        let local = self.known(ObjectType::Mailbox)?;

        let mut tree = ParentMap::with_capacity(DanglingParent::Ignored, server.len());
        let mut entries: Vec<(String, Value)> = Vec::with_capacity(server.len());
        for value in server {
            let folder = FolderValue(&value);
            let Some(id) = folder.id().map(str::to_owned) else {
                continue;
            };
            tree.insert(id.clone(), folder.str("parentFolderId").map(str::to_owned));
            entries.push((id, value));
        }
        tree.parents_first(&mut entries, |(id, _)| id);
        let mut by_id: HashMap<&str, LocalId> = HashMap::with_capacity(entries.len());
        let mut out = Vec::with_capacity(entries.len());

        for chunk in entries.chunks(CHUNK_SIZE) {
            let tx = self.store.begin()?;
            let store = tx.store();
            let mailboxes = store.mailboxes();
            for (graph_id, value) in chunk {
                let folder = FolderValue(value);
                let record = MailboxRecord {
                    name: folder.str("displayName").unwrap_or("Unnamed"),
                    parent: folder
                        .str("parentFolderId")
                        .and_then(|parent| by_id.get(parent))
                        .copied(),
                    role: well_known.get(graph_id.as_str()).copied(),
                    sort_order: 0,
                    subscribed: if folder.flag("isHidden") {
                        Subscribed::No
                    } else {
                        Subscribed::Yes
                    },
                };
                let local_id = match local.get(graph_id) {
                    Some(&id) => {
                        mailboxes.update(id, &record)?;
                        counts.fetched += 1;
                        id
                    }
                    None => {
                        let id = mailboxes.insert(&record)?;
                        self.ids_in(store)
                            .insert(ObjectType::Mailbox, graph_id, id)?;
                        counts.created += 1;
                        id
                    }
                };
                by_id.insert(graph_id, local_id);
                out.push(GraphFolder {
                    graph_id: graph_id.clone(),
                    local_id,
                });
            }
            tx.commit()?;
        }

        let mut vanished = IdDiff::compute(out.iter().map(|f| f.graph_id.clone()), &local).vanished;
        self.parents(ObjectType::Mailbox)?
            .children_first(&mut vanished, |(_, id)| id);
        self.sweep(ObjectType::Mailbox, &vanished, counts)?;
        Ok(out)
    }

    pub fn reconcile_calendars(&self, counts: &mut TypeCounts) -> Result<Vec<GraphFolder>, Error> {
        let server = self
            .client
            .collect_all_values(&self.endpoints.calendars(self.top), &[])?;
        let time_zone = self.mailbox_time_zone();
        let local = self.known(ObjectType::Calendar)?;
        let records: Vec<(&str, CalendarRecord<'_>)> = server
            .iter()
            .filter_map(|value| {
                let folder = FolderValue(value);
                let graph_id = folder.id()?;
                Some((
                    graph_id,
                    CalendarRecord {
                        name: folder.str("name").unwrap_or("Calendar"),
                        description: None,
                        color: folder
                            .str("hexColor")
                            .or_else(|| folder.str("color").and_then(|c| CalendarColor(c).hex())),
                        sort_order: 0,
                        subscribed: Subscribed::Yes,
                        is_visible: true,
                        is_default: folder.flag("isDefaultCalendar"),
                        include_in_availability: "all",
                        default_alerts_with_time: None,
                        default_alerts_without_time: None,
                        time_zone: time_zone.as_deref(),
                    },
                ))
            })
            .collect();
        let vanished =
            IdDiff::compute(records.iter().map(|(id, _)| (*id).to_owned()), &local).vanished;
        self.sweep(ObjectType::Calendar, &vanished, counts)?;
        let demoted = |(graph_id, record): &&(&str, CalendarRecord<'_>)| {
            !record.is_default && local.contains_key(*graph_id)
        };
        let write_order: Vec<&(&str, CalendarRecord<'_>)> = records
            .iter()
            .filter(demoted)
            .chain(records.iter().filter(|r| !demoted(r)))
            .collect();
        let mut inserted: HashMap<&str, LocalId> = HashMap::new();

        for chunk in write_order.chunks(CHUNK_SIZE) {
            let tx = self.store.begin()?;
            let store = tx.store();
            let calendars = store.calendars();
            for &&(graph_id, ref record) in chunk {
                match local.get(graph_id) {
                    Some(&id) => {
                        calendars.update(id, record)?;
                        counts.fetched += 1;
                    }
                    None => {
                        let id = calendars.insert(record)?;
                        self.ids_in(store)
                            .insert(ObjectType::Calendar, graph_id, id)?;
                        counts.created += 1;
                        inserted.insert(graph_id, id);
                    }
                }
            }
            tx.commit()?;
        }

        let out: Vec<GraphFolder> = records
            .iter()
            .filter_map(|&(graph_id, _)| {
                let local_id = local
                    .get(graph_id)
                    .or_else(|| inserted.get(graph_id))
                    .copied()?;
                Some(GraphFolder {
                    graph_id: graph_id.to_owned(),
                    local_id,
                })
            })
            .collect();
        Ok(out)
    }

    pub fn reconcile_address_books(
        &self,
        counts: &mut TypeCounts,
    ) -> Result<Vec<GraphFolder>, Error> {
        let mut server = self
            .client
            .collect_all_values(&self.endpoints.contact_folders(self.top), &[])?;
        let mut seen: HashSet<String> = server
            .iter()
            .filter_map(|value| FolderValue(value).id())
            .map(str::to_owned)
            .collect();
        let mut default_id = None;
        if let Some(default) = self.default_contact_folder()
            && let Some(id) = FolderValue(&default).id()
        {
            default_id = Some(id.to_owned());
            if seen.insert(id.to_owned()) {
                server.insert(0, default);
            }
        }
        let mut frontier: Vec<String> = seen.iter().cloned().collect();
        while let Some(parent) = frontier.pop() {
            let url = self.endpoints.contact_folder_children(&parent, self.top);
            for child in self.client.collect_all_values(&url, &[])? {
                let Some(id) = FolderValue(&child).id() else {
                    continue;
                };
                if seen.insert(id.to_owned()) {
                    frontier.push(id.to_owned());
                    server.push(child);
                }
            }
        }

        let local = self.known(ObjectType::AddressBook)?;
        let mut out = Vec::with_capacity(server.len());
        for chunk in server.chunks(CHUNK_SIZE) {
            let tx = self.store.begin()?;
            let store = tx.store();
            let books = store.address_books();
            for value in chunk {
                let folder = FolderValue(value);
                let Some(graph_id) = folder.id() else {
                    continue;
                };
                let record = AddressBookRecord {
                    name: folder.str("displayName").unwrap_or("Contacts"),
                    description: None,
                    sort_order: 0,
                    is_default: default_id.as_deref() == Some(graph_id),
                    subscribed: Subscribed::Yes,
                };
                let local_id = match local.get(graph_id) {
                    Some(&id) => {
                        books.update(id, &record)?;
                        counts.fetched += 1;
                        id
                    }
                    None => {
                        let id = books.insert(&record)?;
                        self.ids_in(store)
                            .insert(ObjectType::AddressBook, graph_id, id)?;
                        counts.created += 1;
                        id
                    }
                };
                out.push(GraphFolder {
                    graph_id: graph_id.to_owned(),
                    local_id,
                });
            }
            tx.commit()?;
        }

        let vanished = IdDiff::compute(out.iter().map(|f| f.graph_id.clone()), &local).vanished;
        self.sweep(ObjectType::AddressBook, &vanished, counts)?;
        Ok(out)
    }

    fn default_contact_folder(&self) -> Option<Value> {
        match self
            .client
            .get_json(&self.endpoints.default_contact_folder())
        {
            Ok(value) if FolderValue(&value).id().is_some() => return Some(value),
            Ok(_) => {}
            Err(e) => self.logger.warn(format_args!(
                "default contact folder lookup failed ({e}); \
                 deriving it from an existing contact instead"
            )),
        }
        let probe = self
            .client
            .get_json(&self.endpoints.any_contact_parent())
            .ok()?;
        let parent = FolderValue(probe.get("value")?.as_array()?.first()?).str("parentFolderId")?;
        self.client
            .get_json(&self.endpoints.contact_folder(parent))
            .ok()
    }

    fn mailbox_time_zone(&self) -> Option<String> {
        let value = self
            .client
            .get_json(&self.endpoints.mailbox_settings_timezone())
            .ok()?;
        ZoneId(FolderValue(&value).str("timeZone")?).iana()
    }

    fn well_known_roles(&self) -> HashMap<String, &'static str> {
        let mut out = HashMap::with_capacity(WELL_KNOWN_ROLES.len());
        for (short_name, role) in WELL_KNOWN_ROLES {
            let url = self.endpoints.well_known_folder(short_name);
            if let Ok(value) = self.client.get_json(&url)
                && let Some(id) = FolderValue(&value).id()
            {
                out.insert(id.to_owned(), role);
            }
        }
        out
    }
}
