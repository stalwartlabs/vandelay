/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashSet;

use rusqlite::params;
use serde_json::{Value, json};

use super::prune::{DestroyOrder, TargetObj};
use super::target::{ClientId, Maps, TargetJson};
use super::{Exporter, Plan};
use crate::archive::LocalId;
use crate::error::Error;
use crate::jmap::request::Request;
use crate::jmap::wire::JmapId;
use crate::jmap::wire::address_book::AddressBook;
use crate::jmap::wire::calendar::Calendar;
use crate::sync::TypeCounts;
use crate::sync::keys::FoldedName;
use crate::types::ObjectType;

struct FlatLocal {
    local: LocalId,
    name: FoldedName,
    is_default: bool,
}

impl Exporter<'_> {
    pub(super) fn reconcile_flat(
        &self,
        ty: ObjectType,
        maps: &mut Maps,
        counts: &mut TypeCounts,
    ) -> Result<Plan, Error> {
        let locals = self.flat_locals(ty)?;
        let targets = self.get_all(ty)?;
        let mut default_claimed = targets
            .iter()
            .any(|t| TargetJson(t).flag("isDefault").unwrap_or(false));
        let named: Vec<(&str, FoldedName)> = targets
            .iter()
            .filter_map(|t| {
                let t = TargetJson(t);
                Some((t.id()?, FoldedName::new(t.text("name")?)))
            })
            .collect();
        let mut tmatched: HashSet<&str> = HashSet::new();

        let mut to_create: Vec<&FlatLocal> = Vec::new();
        for local in &locals {
            match named
                .iter()
                .find(|(id, name)| *name == local.name && !tmatched.contains(id))
            {
                Some((tid, _)) => {
                    tmatched.insert(tid);
                    maps.insert(ty, local.local, JmapId((*tid).to_owned()));
                    counts.skipped += 1;
                }
                None => to_create.push(local),
            }
        }

        if !to_create.is_empty() {
            let mut batch = Vec::with_capacity(to_create.len());
            for local in &to_create {
                batch.push((
                    ClientId::object(local.local),
                    self.flat_create(ty, local.local)?,
                ));
            }
            let outcome = self.create(ty, batch)?;
            maps.record_created(ty, &outcome, counts);
            let defaults: Vec<LocalId> = to_create
                .iter()
                .filter(|l| l.is_default)
                .map(|l| l.local)
                .collect();
            for (cid, v) in &outcome.created {
                if let Some(local) = ClientId::parse(cid)
                    && let Some(id) = TargetJson(v).id()
                    && defaults.contains(&local)
                {
                    if default_claimed {
                        self.logger.warn(format_args!(
                            "{} {id}: the target account already has a default, leaving it alone",
                            ty.noun()
                        ));
                    } else if self.claim_default(ty, id) {
                        default_claimed = true;
                    }
                }
            }
            for (cid, err) in &outcome.not_created {
                self.warn_not_created(ty, cid, err, counts);
            }
        }

        let objs: Vec<TargetObj<'_>> = targets
            .iter()
            .filter_map(|t| {
                let t = TargetJson(t);
                let id = t.id()?;
                Some(TargetObj {
                    id,
                    matched: tmatched.contains(id),
                    protected: t.flag("isDefault").unwrap_or(false),
                    may_delete: t.may_delete().unwrap_or(true),
                    parent: None,
                })
            })
            .collect();
        Ok(Plan::prune(TargetObj::candidates(
            &objs,
            DestroyOrder::ById,
        )))
    }

    fn flat_locals(&self, ty: ObjectType) -> Result<Vec<FlatLocal>, Error> {
        let table = ty.table();
        let mut stmt = self
            .store
            .connection()
            .prepare(&format!("SELECT id, name, is_default FROM {table}"))
            .map_err(Error::partial)?;
        stmt.query_map([], |r| {
            Ok(FlatLocal {
                local: r.get(0)?,
                name: FoldedName::new(&r.get::<_, String>(1)?),
                is_default: r.get::<_, i64>(2)? != 0,
            })
        })
        .and_then(Iterator::collect)
        .map_err(Error::partial)
    }

    fn flat_create(&self, ty: ObjectType, local: LocalId) -> Result<Value, Error> {
        let select = if ty == ObjectType::AddressBook {
            AddressBook::SELECT
        } else {
            Calendar::SELECT
        };
        let mut wire = self
            .store
            .connection()
            .query_row(&format!("{select} WHERE id = ?1"), params![local], |row| {
                Ok(if ty == ObjectType::AddressBook {
                    AddressBook::from_row(row).map(|w| serde_json::to_value(&w))
                } else {
                    Calendar::from_row(row).map(|mut w| {
                        if let Some(dropped) = w.normalize_color() {
                            self.logger.progress(format_args!(
                                "calendar {local}: dropping color {dropped:?}, not a CSS color name or #RGB/#RRGGBB value"
                            ));
                        }
                        serde_json::to_value(&w)
                    })
                })
            })
            .map_err(Error::partial)??
            .map_err(Error::partial)?;
        if let Value::Object(m) = &mut wire {
            m.remove("isDefault");
        }
        Ok(wire)
    }

    fn claim_default(&self, ty: ObjectType, id: &str) -> bool {
        if self.is_dry_run() {
            return true;
        }
        let mut req = Request::new();
        req.call(
            format!("{}/set", ty.jmap_name()),
            json!({ "accountId": self.account.account_id(), "onSuccessSetIsDefault": id }),
            "d",
        );
        match self.account.send(&req) {
            Ok(_) => true,
            Err(e) => {
                self.logger
                    .warn(format_args!("{} isDefault not set: {e}", ty.noun()));
                false
            }
        }
    }
}
