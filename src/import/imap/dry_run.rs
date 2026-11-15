/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};

use crate::archive::{LocalId, SourceId};
use crate::error::Error;
use crate::sync::PlanCounts;
use crate::sync::diff::IdDiff;
use crate::types::ObjectType;

use super::folders::ResolvedFolder;
use super::run::ImapRun;

impl ImapRun {
    pub(super) fn dry_run(
        &mut self,
        source: Option<SourceId>,
        folders: &[ResolvedFolder],
    ) -> Result<(), Error> {
        let logger = self.run.options().logger;
        let store = self.run.store();
        let ids = source.map(|sid| store.imap_ids(sid));
        let state = source.map(|sid| store.imap_folder_state(sid));
        let failed = &mut self.counts.email.failed;
        let mut mailbox = PlanCounts::default();
        let mut email = PlanCounts::default();

        let local_mailboxes = match ids {
            Some(ids) => ids.folders()?,
            None => HashMap::new(),
        };
        let server: HashSet<&str> = folders.iter().map(|f| f.name.as_str()).collect();
        let vanished: Vec<&str> = local_mailboxes
            .keys()
            .map(String::as_str)
            .filter(|n| !server.contains(n))
            .collect();
        mailbox.new = folders
            .iter()
            .filter(|f| !local_mailboxes.contains_key(&f.name))
            .count() as u64;
        mailbox.present = folders.len() as u64 - mailbox.new;
        mailbox.vanished = vanished.len() as u64;

        for folder in folders.iter().filter(|f| f.selectable) {
            let selected = match self.control.select(folder) {
                Ok(selected) => selected,
                Err(e) => {
                    logger.note(format_args!(
                        "cannot select folder \"{}\": {e}",
                        folder.name
                    ));
                    *failed += 1;
                    continue;
                }
            };
            let server_uidvalidity = selected.uidvalidity;
            let server_uids = match self.control.uids() {
                Ok(uids) => uids,
                Err(e) => {
                    logger.note(format_args!(
                        "cannot list the UIDs of folder \"{}\": {e}",
                        folder.name
                    ));
                    *failed += 1;
                    continue;
                }
            };
            let prev = match state {
                Some(state) => state.get(&folder.name)?,
                None => None,
            };
            let changed = prev
                .as_ref()
                .filter(|p| p.uidvalidity != server_uidvalidity && server_uidvalidity != 0);
            let local: HashMap<u32, LocalId> = match ids {
                Some(ids) if changed.is_none() => ids
                    .items_in(&folder.name)?
                    .into_iter()
                    .filter(|((v, _), _)| server_uidvalidity == 0 || *v == server_uidvalidity)
                    .map(|((_, uid), id)| (uid, id))
                    .collect(),
                _ => HashMap::new(),
            };
            let plan = IdDiff::compute(server_uids, &local).plan();
            email.new += plan.new;
            email.vanished += plan.vanished;
            email.present += plan.present;
            if let Some(prev) = changed {
                logger.progress(format_args!(
                    "UIDVALIDITY of folder \"{}\" changed from {} to {server_uidvalidity}; every message counts as new",
                    folder.name, prev.uidvalidity
                ));
            }
        }
        if let Some(ids) = ids {
            for name in &vanished {
                email.vanished += ids.items_in(name)?.len() as u64;
            }
        }
        let summary = self.run.summary();
        *summary.plan(ObjectType::Mailbox) = mailbox;
        *summary.plan(ObjectType::Email) = email;

        self.control.logout();
        Ok(())
    }
}
