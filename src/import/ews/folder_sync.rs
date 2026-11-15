/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;
use std::fmt::Write as _;
use std::mem;

use crate::archive::LocalId;
use crate::console::{Counted, Grouped};
use crate::error::Error;
use crate::exchange::ews::parse::ResponseMessage;
use crate::exchange::ews::types::{FolderId, ResponseCode};
use crate::exchange::ews::xml::ItemShape;
use crate::logging::Level;
use crate::sync::TypeCounts;
use crate::types::ObjectType;

use super::coordinator::EwsCoordinator;
use super::folders::{ClassifiedFolder, FolderPlan};
use super::items::{EnumerationMode, EnumerationOutcome};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Message,
    Contact,
    Event,
}

pub struct Placement<'p> {
    pub folder: LocalId,
    pub folder_id: &'p str,
    pub existing: &'p HashMap<String, LocalId>,
}

impl ItemKind {
    pub const ALL: [ItemKind; 3] = [ItemKind::Message, ItemKind::Contact, ItemKind::Event];

    pub fn container(self) -> ObjectType {
        match self {
            ItemKind::Message => ObjectType::Mailbox,
            ItemKind::Contact => ObjectType::AddressBook,
            ItemKind::Event => ObjectType::Calendar,
        }
    }

    pub fn item(self) -> ObjectType {
        match self {
            ItemKind::Message => ObjectType::Email,
            ItemKind::Contact => ObjectType::ContactCard,
            ItemKind::Event => ObjectType::CalendarEvent,
        }
    }

    fn shape(self) -> ItemShape {
        match self {
            ItemKind::Message => ItemShape::Message,
            ItemKind::Contact => ItemShape::Contact,
            ItemKind::Event => ItemShape::CalendarItem,
        }
    }

    pub fn folders(self, plan: &FolderPlan) -> &[ClassifiedFolder] {
        match self {
            ItemKind::Message => &plan.mail,
            ItemKind::Contact => &plan.contacts,
            ItemKind::Event => &plan.calendar,
        }
    }
}

impl EwsCoordinator<'_> {
    pub fn reconcile_items(
        &self,
        kind: ItemKind,
        plan: &FolderPlan,
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        for folder in kind.folders(plan) {
            let entry = &folder.folder;
            let Some(local) = self
                .ids()
                .local_id(kind.container(), &entry.folder_id.id)
                .map_err(Error::partial)?
            else {
                continue;
            };
            if let Err(e) =
                self.reconcile_folder(kind, &entry.folder_id, &entry.display_name, local, counts)
            {
                self.items.logger.warn(format_args!(
                    "{} \"{}\" failed: {e}",
                    kind.container().noun(),
                    entry.display_name
                ));
                counts.failed += 1;
            }
        }
        Ok(())
    }

    fn reconcile_folder(
        &self,
        kind: ItemKind,
        folder: &FolderId,
        folder_name: &str,
        local_folder: LocalId,
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        let ids = self.ids();
        let prior = ids
            .sync_state(kind.container(), &folder.id)
            .map_err(Error::partial)?;
        let mut outcome = self.items.enumerate(folder, prior.as_deref())?;
        match kind {
            ItemKind::Message => self.skip_non_mail(&mut outcome, folder_name, counts),
            ItemKind::Event => outcome.items.retain(|s| {
                hashify::set_ignore_case!(s.element.as_bytes(), "calendaritem", "item")
            }),
            ItemKind::Contact => {}
        }
        let local = ids
            .items_in(kind.item(), &folder.id)
            .map_err(Error::partial)?;
        let mut plan = outcome.plan(&local);
        let logger = self.items.logger;
        match kind {
            ItemKind::Message => logger.progress(format_args!(
                "{} in \"{folder_name}\": {} new, {} changed, {} vanished, {} unchanged",
                kind.item().label(),
                Grouped(plan.new.len() as u64),
                Grouped(plan.present_changed.len() as u64),
                Grouped(plan.vanished.len() as u64),
                Grouped(plan.present_unchanged as u64)
            )),
            ItemKind::Contact | ItemKind::Event => logger.progress(format_args!(
                "{} in \"{folder_name}\": {} new, {} changed, {} vanished",
                kind.item().label(),
                Grouped(plan.new.len() as u64),
                Grouped(plan.present_changed.len() as u64),
                Grouped(plan.vanished.len() as u64)
            )),
        }
        let vanished = mem::take(&mut plan.vanished);
        let (to_fetch, existing) = plan.into_fetch();
        if !to_fetch.is_empty() {
            let at = Placement {
                folder: local_folder,
                folder_id: &folder.id,
                existing: &existing,
            };
            let failed_items =
                self.items
                    .for_each_fetched_item(kind.shape(), &to_fetch, |msg| {
                        self.import_item(kind, msg, &at, counts)
                    })?;
            counts.failed += failed_items;
        }
        self.sweep(kind.item(), &vanished, counts)?;
        if let EnumerationMode::Delta { new_sync_state, .. } = &outcome.mode {
            ids.set_sync_state(kind.container(), &folder.id, new_sync_state)
                .map_err(Error::partial)?;
        }
        Ok(())
    }

    fn import_item(
        &self,
        kind: ItemKind,
        msg: ResponseMessage,
        at: &Placement<'_>,
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        if !msg.success {
            if msg.response_code == ResponseCode::ItemNotFound {
                counts.skipped += 1;
                return Ok(());
            }
            counts.failed += 1;
            let logger = self.items.logger;
            let noun = kind.item().noun();
            match kind {
                ItemKind::Message => logger.warn(format_args!(
                    "GetItem ({noun}) failed: {} {}",
                    msg.response_code, msg.message_text
                )),
                ItemKind::Contact | ItemKind::Event => logger.warn(format_args!(
                    "GetItem ({noun}) failed: {}",
                    msg.response_code
                )),
            }
            return Ok(());
        }
        match kind {
            ItemKind::Message => self.import_message(&msg.inner_xml, at, counts),
            ItemKind::Contact => self.import_contact(&msg.inner_xml, at, counts),
            ItemKind::Event => self.import_event(&msg.inner_xml, at, counts),
        }
    }

    fn skip_non_mail(
        &self,
        outcome: &mut EnumerationOutcome,
        folder_name: &str,
        counts: &mut TypeCounts,
    ) {
        let skipped = outcome.drop_non_mail();
        if skipped.is_empty() {
            return;
        }
        let total: u64 = skipped.iter().map(|(_, n)| n).sum();
        counts.skipped += total;
        let logger = self.items.logger;
        if logger.enabled(Level::Default) {
            let mut summary = String::new();
            for (class, n) in &skipped {
                if !summary.is_empty() {
                    summary.push_str(", ");
                }
                let _ = write!(summary, "{} {class}", Grouped(*n));
            }
            logger.info(format_args!(
                "Skipped {} in \"{folder_name}\" ({summary})",
                Counted::new(total, "non-mail item", "non-mail items")
            ));
        }
    }
}
