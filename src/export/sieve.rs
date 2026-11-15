/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

use super::target::{ClientId, TargetJson};
use super::uploader::Uploader;
use super::{Exporter, Plan};
use crate::archive::LocalId;
use crate::error::Error;
use crate::jmap::request::{Request, URN_VACATION_RESPONSE};
use crate::jmap::wire::vacation_response::VacationResponse;
use crate::sync::TypeCounts;
use crate::sync::jmap_map::SieveRow;
use crate::types::ObjectType;

const SIEVE_MEDIA_TYPE: &str = "application/sieve";

impl Exporter<'_> {
    pub(super) fn reconcile_sieve(&self, counts: &mut TypeCounts) -> Result<Plan, Error> {
        let ty = ObjectType::SieveScript;
        let targets = self.get_all(ty)?;
        let mut target_by_name: HashMap<String, String> = HashMap::with_capacity(targets.len());
        for t in &targets {
            let t = TargetJson(t);
            if let (Some(id), Some(name)) = (t.id(), t.text("name")) {
                target_by_name.insert(name.to_owned(), id.to_owned());
            }
        }

        let target_has_vacation = self
            .account
            .session()
            .may_support(self.account.account_id(), URN_VACATION_RESPONSE);
        let vacation_exported =
            target_has_vacation && self.store.vacation().exists().map_err(Error::partial)?;

        let mut locals = self.sieve_locals()?;
        if vacation_exported {
            let before = locals.len();
            locals.retain(|(_, row)| {
                !row.name
                    .as_deref()
                    .is_some_and(VacationResponse::is_reserved_script_name)
            });
            counts.skipped += (before - locals.len()) as u64;
        }

        let mut active_target: Option<String> = None;
        let mut uploader = Uploader::new(self);
        for (local, row) in &locals {
            let matched = row
                .name
                .as_ref()
                .and_then(|n| target_by_name.get(n))
                .cloned();
            let target_id = if let Some(id) = matched {
                counts.skipped += 1;
                id
            } else {
                let cid = ClientId::object(*local);
                let build = |up: &mut Uploader<'_>| -> Result<Value, Error> {
                    let blob_id = up.upload(row.blob, SIEVE_MEDIA_TYPE)?;
                    let mut obj = Map::new();
                    if let Some(n) = &row.name {
                        obj.insert("name".to_owned(), Value::String(n.clone()));
                    }
                    obj.insert("blobId".to_owned(), Value::String(blob_id.0));
                    Ok(Value::Object(obj))
                };
                uploader.clear_touched();
                let wire = build(&mut uploader)?;
                let outcome = self.create(ty, vec![(cid.clone(), wire)])?;
                let outcome =
                    self.retry_if_blob_missing(ty, &cid, &mut uploader, outcome, build)?;
                match outcome
                    .created
                    .first()
                    .and_then(|(_, v)| TargetJson(v).id())
                {
                    Some(id) => {
                        counts.created += 1;
                        if let Some(n) = &row.name {
                            target_by_name.insert(n.clone(), id.to_owned());
                        }
                        id.to_owned()
                    }
                    None => {
                        for (cid, err) in &outcome.not_created {
                            self.logger
                                .warn(format_args!("sieve script {cid} not created: {err}"));
                        }
                        counts.failed += 1;
                        continue;
                    }
                }
            };
            if row.is_active {
                active_target = Some(target_id);
            }
        }

        let deactivate = active_target.is_none() && locals.iter().all(|(_, row)| !row.is_active);
        if !self.is_dry_run() {
            let account_id = self.account.account_id();
            let args = if let Some(id) = &active_target {
                json!({ "accountId": account_id, "onSuccessActivateScript": id })
            } else if deactivate {
                json!({ "accountId": account_id, "onSuccessDeactivateScript": true })
            } else {
                json!({ "accountId": account_id })
            };
            let mut req = Request::new();
            req.call("SieveScript/set", args, "a");
            if let Err(e) = self.account.send(&req) {
                self.logger
                    .warn(format_args!("sieve script activation failed: {e}"));
            }
        }

        let local_names: HashSet<&str> = locals
            .iter()
            .filter_map(|(_, row)| row.name.as_deref())
            .collect();
        let mut prune_candidates: Vec<String> = target_by_name
            .iter()
            .filter(|(name, _)| !local_names.contains(name.as_str()))
            .filter(|(name, _)| {
                !(target_has_vacation && VacationResponse::is_reserved_script_name(name))
            })
            .map(|(_, id)| id.clone())
            .collect();
        prune_candidates.sort_unstable();

        Ok(Plan {
            prune_candidates,
            active_sieve_target: active_target,
        })
    }

    fn sieve_locals(&self) -> Result<Vec<(LocalId, SieveRow)>, Error> {
        let mut stmt = self
            .store
            .connection()
            .prepare(SieveRow::SELECT)
            .map_err(Error::partial)?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, SieveRow::from_row(row))))
            .map_err(Error::partial)?;
        let rows: Vec<_> = rows.collect::<Result<_, _>>().map_err(Error::partial)?;
        let mut out = Vec::with_capacity(rows.len());
        for (id, sieve) in rows {
            out.push((id, sieve?));
        }
        Ok(out)
    }
}
