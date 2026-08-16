/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashSet;
use std::fmt::Write as _;

use serde_json::Value;

use super::common::{create_batch, jid, target_query_get};
use super::{Maps, Net, Plan, Uploader};
use crate::error::Error;
use crate::logging::Logger;
use crate::sync::import_jmap::mapping::{
    CALENDAR_EVENT_SELECT, CONTACT_CARD_SELECT, calendar_event_to_wire, contact_card_to_wire,
};
use crate::sync::prune::{TargetObj, candidates};
use crate::sync::{Context, TypeCounts};
use crate::types::ObjectType;

fn target_uid(v: &Value) -> Option<String> {
    v.get("uid").and_then(Value::as_str).map(str::to_owned)
}

fn describe(ty: ObjectType, local: i64, uid: &str) -> String {
    let mut out = String::new();
    let _ = write!(out, "{} local {local}", ty.jmap_name());
    if !uid.is_empty() {
        let _ = write!(out, " (uid {uid})");
    }
    out
}

pub fn reconcile(
    ctx: &Context,
    net: &Net,
    ty: ObjectType,
    maps: &mut Maps,
    counts: &mut TypeCounts,
    logger: &Logger,
) -> Result<Plan, Error> {
    let targets = target_query_get(net, ty, None).map_err(Error::from)?;
    let mut by_uid: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for t in &targets {
        if let (Some(uid), Some(id)) = (target_uid(t), jid(t)) {
            by_uid.entry(uid).or_insert(id);
        }
    }

    let select = if ty == ObjectType::ContactCard {
        CONTACT_CARD_SELECT
    } else {
        CALENDAR_EVENT_SELECT
    };
    let rows: Vec<(i64, String)> = {
        let mut stmt = ctx
            .conn
            .prepare(select)
            .map_err(|e| Error::Partial(e.to_string()))?;
        stmt.query_map([], |r| {
            if ty == ObjectType::ContactCard {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            } else {
                let data: String = r.get(4)?;
                let uid = serde_json::from_str::<Value>(&data)
                    .ok()
                    .and_then(|v| v.get("uid").and_then(Value::as_str).map(str::to_owned))
                    .unwrap_or_default();
                Ok((r.get::<_, i64>(0)?, uid))
            }
        })
        .and_then(|m| m.collect::<Result<Vec<_>, _>>())
        .map_err(|e| Error::Partial(e.to_string()))?
    };

    let mut matched_uids: HashSet<String> = HashSet::new();
    let blobs = Uploader::new(net, &ctx.conn);
    for (local, uid) in &rows {
        if let Some(tid) = by_uid.get(uid) {
            maps.insert(ty, *local, crate::jmap::wire::JmapId(tid.clone()));
            matched_uids.insert(uid.clone());
            counts.skipped += 1;
            continue;
        }
        let cid = format!("c{local}");
        let wire = match build_wire(ctx, ty, *local, maps, &blobs) {
            Ok(w) => w,
            Err(e) if e.aborts_run() => return Err(e),
            Err(e) => {
                logger.warn(&format!("{} skipped: {e}", describe(ty, *local, uid)));
                counts.failed += 1;
                continue;
            }
        };
        let outcome = match create_batch(net, ty, vec![(cid.clone(), wire)]) {
            Ok(o) => o,
            Err(e) => {
                let mapped = Error::from(e);
                if mapped.aborts_run() {
                    return Err(mapped);
                }
                logger.warn(&format!(
                    "{} not created: {mapped}",
                    describe(ty, *local, uid)
                ));
                counts.failed += 1;
                continue;
            }
        };
        for (cid, v) in &outcome.created {
            if let Some(parsed) = cid.strip_prefix('c').and_then(|s| s.parse::<i64>().ok())
                && let Some(id) = jid(v)
            {
                maps.insert(ty, parsed, crate::jmap::wire::JmapId(id));
                counts.created += 1;
            }
        }
        for (cid, err) in &outcome.not_created {
            logger.warn(&format!("{} {cid} not created: {err}", ty.jmap_name()));
            counts.failed += 1;
        }
    }

    let objs: Vec<TargetObj> = targets
        .iter()
        .filter_map(|t| {
            let id = jid(t)?;
            let uid = target_uid(t);
            Some(TargetObj {
                id: id.clone(),
                matched: uid.map(|u| matched_uids.contains(&u)).unwrap_or(false),
                protected: false,
                may_delete: true,
                parent: None,
            })
        })
        .collect();
    Ok(Plan {
        prune_candidates: candidates(&objs, false),
        active_sieve_target: None,
    })
}

fn build_wire(
    ctx: &Context,
    ty: ObjectType,
    local: i64,
    maps: &Maps,
    blobs: &Uploader<'_>,
) -> Result<Value, Error> {
    if ty == ObjectType::ContactCard {
        let (uid, abids, data): (String, String, String) = ctx
            .conn
            .query_row(
                &format!("{CONTACT_CARD_SELECT} WHERE id = ?1"),
                rusqlite::params![local],
                |r| Ok((r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .map_err(|e| Error::Partial(e.to_string()))?;
        contact_card_to_wire(&uid, &abids, &data, maps, blobs).map_err(Error::from)
    } else {
        let (cal, dr, ud, data): (String, i64, i64, String) = ctx
            .conn
            .query_row(
                &format!("{CALENDAR_EVENT_SELECT} AND id = ?1"),
                rusqlite::params![local],
                |r| Ok((r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .map_err(|e| Error::Partial(e.to_string()))?;
        calendar_event_to_wire(&cal, dr != 0, ud != 0, &data, maps, blobs).map_err(Error::from)
    }
}
