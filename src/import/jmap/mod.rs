/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod blobs;
mod kinds;
mod reconcile;
mod tree;

use std::mem;
use std::sync::Arc;

use serde_json::Value;

use self::kinds::IMPORT_ORDER;
use crate::archive::sources::{SourceChange, SourceKey, SourceKind};
use crate::archive::{Archive, SourceId, Store};
use crate::console::Initial;
use crate::error::Error;
use crate::jmap::account::AccountSelector;
use crate::jmap::connect::{ConnectConfig, JmapAccount};
use crate::jmap::http::JmapClient;
use crate::jmap::request::URN_VACATION_RESPONSE;
use crate::jmap::wire::JmapId;
use crate::jmap::wire::vacation_response::{self, VacationResponse};
use crate::logging::Logger;
use crate::net::auth::Auth;
use crate::sync::diff::IdDiff;
use crate::sync::run::{GcFailure, ImportRun, SourceClaim, SourceLabel};
use crate::sync::{PlanCounts, RunOptions, RunOutcome, Summary, SummaryType, TypeCounts};
use crate::types::{ObjectType, ObjectTypes};

const BEARER_USERNAME: &str = "(bearer)";

pub struct JmapImport {
    pub connect: ConnectConfig,
    pub objects: Option<ObjectTypes>,
    pub source_change: SourceChange,
}

impl JmapImport {
    pub fn run(self, options: RunOptions) -> RunOutcome {
        let archive = match Archive::open(&options.archive) {
            Ok(archive) => archive,
            Err(e) => return RunOutcome::failed(Error::from(e)),
        };
        let client = JmapClient::new(self.connect.auth.clone(), options.client_options());
        let account = match JmapAccount::connect(client, &self.connect) {
            Ok(account) => Arc::new(account),
            Err(e) => return RunOutcome::failed(e),
        };
        let key = SourceKey {
            kind: SourceKind::Jmap,
            url: &self.connect.url,
            account_id: account.account_id(),
        };
        let label = SourceLabel {
            name: match &self.connect.account {
                AccountSelector::Name(name) => Some(name),
                AccountSelector::Id(_) => None,
            },
            username: match account.client().auth() {
                Auth::Basic { user, .. } => user,
                Auth::Bearer { .. } => BEARER_USERNAME,
            },
        };
        let mut run = match ImportRun::start_on(archive, options, &key, label, self.source_change) {
            Ok(run) => run,
            Err(e) => return RunOutcome::failed(e),
        };

        let mut summary = Summary::default();
        let result = self.import_types(&run, &account, &mut summary);
        let stats = account.client().stats();
        let plan = mem::take(&mut summary.plan);
        *run.summary() = summary;
        match result {
            Err(e) => run.abort(e, stats),
            Ok(()) if matches!(run.claim(), SourceClaim::DryRun(_)) => RunOutcome {
                summary: Summary {
                    per_type: mem::take(&mut run.summary().per_type),
                    plan,
                    ..Summary::default()
                },
                error: None,
            },
            Ok(()) => run.finish(stats, GcFailure::WarnAsPartial),
        }
    }

    fn work_list<'a>(&'a self, account: &'a JmapAccount) -> impl Iterator<Item = ObjectType> + 'a {
        IMPORT_ORDER.into_iter().filter(move |ty| {
            account.supports(*ty) && self.objects.as_ref().is_none_or(|s| s.contains(ty))
        })
    }

    fn import_types(
        &self,
        run: &ImportRun,
        account: &Arc<JmapAccount>,
        summary: &mut Summary,
    ) -> Result<(), Error> {
        let logger = run.options().logger;
        let store = run.store();
        match *run.claim() {
            SourceClaim::DryRun(found) => {
                for ty in self.work_list(account) {
                    logger.info(format_args!("Importing {}", Initial::Lower(ty.label())));
                    let mut counts = TypeCounts::default();
                    let result = Listing::plan(store, found, account, ty)
                        .map(|plan| *summary.plan(ty) = plan);
                    JmapImport::settle(logger, ty.into(), result, &mut counts)?;
                    *summary.counts(ty) = counts;
                }
                Ok(())
            }
            SourceClaim::Registered(source) => {
                let importer = JmapImporter {
                    store,
                    source,
                    account,
                    threads: run.options().threads,
                    logger,
                };
                for ty in self.work_list(account) {
                    logger.info(format_args!("Importing {}", Initial::Lower(ty.label())));
                    let mut counts = TypeCounts::default();
                    let result = importer.reconcile(ty, &mut counts);
                    JmapImport::settle(logger, ty.into(), result, &mut counts)?;
                    *summary.counts(ty) = counts;
                }
                if self
                    .objects
                    .as_ref()
                    .is_none_or(|s| s.contains(&ObjectType::SieveScript))
                    && account
                        .session()
                        .may_support(account.account_id(), URN_VACATION_RESPONSE)
                {
                    logger.info(format_args!("Importing the vacation response"));
                    let mut counts = TypeCounts::default();
                    let result = importer
                        .import_vacation_response()
                        .map(|fetched| counts.fetched = fetched);
                    JmapImport::settle(logger, SummaryType::VacationResponse, result, &mut counts)?;
                    *summary.counts(SummaryType::VacationResponse) = counts;
                }
                Ok(())
            }
        }
    }

    fn settle(
        logger: Logger,
        ty: SummaryType,
        result: Result<(), Error>,
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        match result {
            Ok(()) => Ok(()),
            Err(e) if e.aborts_run() => Err(e),
            Err(e) => {
                logger.warn(format_args!(
                    "{} aborted: {e}; continuing, the run will exit 5",
                    Initial::Lower(ty.label())
                ));
                counts.failed += 1;
                Ok(())
            }
        }
    }
}

struct Listing {
    ids: Vec<String>,
    preloaded: Option<Vec<Value>>,
    state: Option<String>,
}

impl Listing {
    fn plan(
        store: Store<'_>,
        source: Option<SourceId>,
        account: &JmapAccount,
        ty: ObjectType,
    ) -> Result<PlanCounts, Error> {
        let local = match source {
            Some(source) => store.jmap_ids(source).all(ty).map_err(Error::partial)?,
            None => Default::default(),
        };
        let listing = Listing::fetch(account, ty)?;
        Ok(IdDiff::compute(listing.ids, &local).plan())
    }

    fn fetch(account: &JmapAccount, ty: ObjectType) -> Result<Listing, Error> {
        if ty.is_queryless() {
            let got = account.get_all::<Value>(ty.jmap_name())?;
            let ids = got
                .list
                .iter()
                .filter_map(|v| v.get("id").and_then(Value::as_str).map(str::to_owned))
                .collect();
            Ok(Listing {
                ids,
                preloaded: Some(got.list),
                state: got.state,
            })
        } else {
            let ids = account.query_all_ids(ty.jmap_name())?;
            Ok(Listing {
                ids: ids.into_iter().map(|JmapId(id)| id).collect(),
                preloaded: None,
                state: None,
            })
        }
    }
}

struct JmapImporter<'a> {
    store: Store<'a>,
    source: SourceId,
    account: &'a Arc<JmapAccount>,
    threads: usize,
    logger: Logger,
}

impl JmapImporter<'_> {
    fn import_vacation_response(&self) -> Result<u64, Error> {
        let got = self
            .account
            .get_all::<VacationResponse>(vacation_response::TYPE_NAME)?;
        let vacation = self.store.vacation();
        match got.list.iter().find(|v| !v.is_unset()) {
            Some(found) => {
                vacation
                    .store(self.source, &found.record())
                    .map_err(Error::partial)?;
                Ok(1)
            }
            None => {
                vacation.clear(self.source).map_err(Error::partial)?;
                Ok(0)
            }
        }
    }
}
