/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod blobpool;
mod email;
mod flat;
mod keyed;
mod prune;
mod run;
mod sieve;
mod target;
mod tree;
mod uidtype;
mod uploader;
mod vacation;

use crate::archive::{Archive, Store};
use crate::console::Initial;
use crate::error::Error;
use crate::jmap::connect::{ConnectConfig, JmapAccount};
use crate::jmap::http::JmapClient;
use crate::logging::Logger;
use crate::sync::{RunMode, RunOptions, RunOutcome, TypeCounts};
use crate::types::{ObjectType, ObjectTypes};

use self::run::ExportRun;
use self::target::Maps;

const EXPORT_ORDER: [ObjectType; 9] = [
    ObjectType::Mailbox,
    ObjectType::AddressBook,
    ObjectType::Calendar,
    ObjectType::FileNode,
    ObjectType::Identity,
    ObjectType::SieveScript,
    ObjectType::Email,
    ObjectType::ContactCard,
    ObjectType::CalendarEvent,
];

pub struct Export {
    pub connect: ConnectConfig,
    pub objects: Option<ObjectTypes>,
    pub prune: Prune,
    pub confirm: Confirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prune {
    Keep,
    Destroy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    Prompt,
    Assume,
}

impl Export {
    pub fn run(self, options: RunOptions) -> RunOutcome {
        let client = JmapClient::new(self.connect.auth.clone(), options.client_options());
        let archive = match Archive::open(&options.archive) {
            Ok(archive) => archive,
            Err(e) => return RunOutcome::failed(Error::from(e)),
        };
        let account = match JmapAccount::connect(client, &self.connect) {
            Ok(account) => account,
            Err(e) => return RunOutcome::failed(e),
        };
        let exporter = Exporter {
            store: archive.store(),
            account: &account,
            mode: options.mode,
            threads: options.threads,
            logger: options.logger,
        };
        ExportRun::execute(&exporter, &self)
    }

    fn selects(&self, ty: ObjectType) -> bool {
        self.objects.as_ref().is_none_or(|s| s.contains(&ty))
    }
}

#[derive(Default)]
struct Plan {
    prune_candidates: Vec<String>,
    active_sieve_target: Option<String>,
}

impl Plan {
    fn prune(prune_candidates: Vec<String>) -> Plan {
        Plan {
            prune_candidates,
            active_sieve_target: None,
        }
    }
}

#[derive(Clone, Copy)]
struct Exporter<'a> {
    store: Store<'a>,
    account: &'a JmapAccount,
    mode: RunMode,
    threads: usize,
    logger: Logger,
}

impl Exporter<'_> {
    fn is_dry_run(&self) -> bool {
        self.mode == RunMode::DryRun
    }

    fn has_rows(&self, ty: ObjectType) -> bool {
        let table = ty.table();
        self.store
            .connection()
            .query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table})"), [], |r| {
                r.get::<_, i64>(0)
            })
            .map(|n| n != 0)
            .unwrap_or(false)
    }

    fn work_list(&self, config: &Export) -> Vec<ObjectType> {
        let participant = ObjectType::ParticipantIdentity;
        if config.selects(participant) && self.has_rows(participant) {
            self.logger.info(format_args!(
                "{} are not exported; the target creates them from the account's addresses",
                participant.label()
            ));
        }
        EXPORT_ORDER
            .into_iter()
            .filter(|ty| config.selects(*ty))
            .filter(|ty| self.has_rows(*ty))
            .filter(|ty| {
                let supported = self.account.supports(*ty);
                if !supported {
                    self.logger.warn(format_args!(
                        "the target does not support {}; skipping them",
                        Initial::Lower(ty.label())
                    ));
                }
                supported
            })
            .collect()
    }

    fn reconcile(
        &self,
        ty: ObjectType,
        maps: &mut Maps,
        counts: &mut TypeCounts,
    ) -> Result<Plan, Error> {
        match ty {
            ObjectType::Mailbox | ObjectType::FileNode => self.reconcile_tree(ty, maps, counts),
            ObjectType::AddressBook | ObjectType::Calendar => self.reconcile_flat(ty, maps, counts),
            ObjectType::Identity => self.reconcile_identities(counts),
            ObjectType::ParticipantIdentity => Ok(Plan::default()),
            ObjectType::SieveScript => self.reconcile_sieve(counts),
            ObjectType::ContactCard | ObjectType::CalendarEvent => {
                self.reconcile_uid_type(ty, maps, counts)
            }
            ObjectType::Email => self.reconcile_emails(maps, counts),
        }
    }
}
