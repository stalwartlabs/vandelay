/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;

use rusqlite::params;
use serde::Deserialize;
use serde_json::Value;

use super::prune::{DestroyOrder, TargetObj};
use super::target::{ClientId, Maps, TargetJson};
use super::uploader::Uploader;
use super::{Exporter, Plan};
use crate::archive::LocalId;
use crate::error::Error;
use crate::jmap::wire::JmapId;
use crate::jmap::wire::calendar_event::CalendarEvent;
use crate::jmap::wire::contact_card::ContactCard;
use crate::sync::TypeCounts;
use crate::types::ObjectType;

#[derive(Deserialize)]
struct EventUid<'d> {
    #[serde(borrow)]
    uid: Option<Cow<'d, str>>,
}

struct ItemLabel<'u> {
    ty: ObjectType,
    local: LocalId,
    uid: &'u str,
}

impl fmt::Display for ItemLabel<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} local {}", self.ty.jmap_name(), self.local)?;
        if !self.uid.is_empty() {
            write!(f, " (uid {})", self.uid)?;
        }
        Ok(())
    }
}

impl Exporter<'_> {
    pub(super) fn reconcile_uid_type(
        &self,
        ty: ObjectType,
        maps: &mut Maps,
        counts: &mut TypeCounts,
    ) -> Result<Plan, Error> {
        let targets = self.query_get(ty, None)?;
        let mut by_uid: HashMap<&str, &str> = HashMap::with_capacity(targets.len());
        for t in &targets {
            let t = TargetJson(t);
            if let (Some(uid), Some(id)) = (t.text("uid"), t.id()) {
                by_uid.entry(uid).or_insert(id);
            }
        }

        let rows = self.uid_locals(ty)?;
        let mut matched_uids: HashSet<&str> = HashSet::new();
        let uploader = Uploader::new(self);
        for (local, uid) in &rows {
            let label = ItemLabel {
                ty,
                local: *local,
                uid,
            };
            if let Some((target_uid, tid)) = by_uid.get_key_value(uid.as_str()) {
                maps.insert(ty, *local, JmapId((*tid).to_owned()));
                matched_uids.insert(target_uid);
                counts.skipped += 1;
                continue;
            }
            let wire = match self.uid_create(ty, *local, maps, &uploader) {
                Ok(w) => w,
                Err(e) if e.aborts_run() => return Err(e),
                Err(e) => {
                    self.logger.warn(format_args!("{label} skipped: {e}"));
                    counts.failed += 1;
                    continue;
                }
            };
            let outcome = match self.create(ty, vec![(ClientId::object(*local), wire)]) {
                Ok(o) => o,
                Err(e) if e.aborts_run() => return Err(e),
                Err(e) => {
                    self.logger.warn(format_args!("{label} not created: {e}"));
                    counts.failed += 1;
                    continue;
                }
            };
            maps.record_created(ty, &outcome, counts);
            for (cid, err) in &outcome.not_created {
                self.warn_not_created(ty, cid, err, counts);
            }
        }

        let objs: Vec<TargetObj<'_>> = targets
            .iter()
            .filter_map(|t| {
                let t = TargetJson(t);
                Some(TargetObj {
                    id: t.id()?,
                    matched: t.text("uid").is_some_and(|u| matched_uids.contains(u)),
                    protected: false,
                    may_delete: true,
                    parent: None,
                })
            })
            .collect();
        Ok(Plan::prune(TargetObj::candidates(
            &objs,
            DestroyOrder::ById,
        )))
    }

    fn uid_locals(&self, ty: ObjectType) -> Result<Vec<(LocalId, String)>, Error> {
        let select = if ty == ObjectType::ContactCard {
            ContactCard::SELECT
        } else {
            CalendarEvent::SELECT
        };
        let mut stmt = self
            .store
            .connection()
            .prepare(select)
            .map_err(Error::partial)?;
        stmt.query_map([], |r| {
            let uid = if ty == ObjectType::ContactCard {
                r.get::<_, String>(1)?
            } else {
                let data = r.get_ref(4)?.as_str()?;
                serde_json::from_str::<EventUid<'_>>(data)
                    .ok()
                    .and_then(|e| e.uid)
                    .map(Cow::into_owned)
                    .unwrap_or_default()
            };
            Ok((r.get(0)?, uid))
        })
        .and_then(Iterator::collect)
        .map_err(Error::partial)
    }

    fn uid_create(
        &self,
        ty: ObjectType,
        local: LocalId,
        maps: &Maps,
        uploader: &Uploader<'_>,
    ) -> Result<Value, Error> {
        let conn = self.store.connection();
        let resolve = |ty, id| maps.target(ty, id);
        let bytes = |blob| uploader.bytes(blob);
        if ty == ObjectType::ContactCard {
            let (uid, books, data): (String, String, String) = conn
                .query_row(
                    &format!("{} WHERE id = ?1", ContactCard::SELECT),
                    params![local],
                    |r| Ok((r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .map_err(Error::partial)?;
            return Ok(ContactCard::export_json(
                &uid, &books, &data, &resolve, &bytes,
            )?);
        }
        let (calendars, is_draft, use_default_alerts, data): (String, i64, i64, String) = conn
            .query_row(
                &format!("{} AND id = ?1", CalendarEvent::SELECT),
                params![local],
                |r| Ok((r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .map_err(Error::partial)?;
        Ok(CalendarEvent::export_json(
            &calendars,
            is_draft != 0,
            use_default_alerts != 0,
            &data,
            &resolve,
            &bytes,
        )?)
    }
}
