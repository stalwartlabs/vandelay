/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::Value;

use super::common::{create_batch, jid, retry_if_blob_missing, target_query_get};
use super::{Maps, Net, Plan, Uploader};
use crate::error::Error;
use crate::jmap::error::JmapError;
use crate::jmap::request::SetOutcome;
use crate::logging::Logger;
use crate::sync::import_jmap::mapping::{
    CALENDAR_EVENT_SELECT, CONTACT_CARD_SELECT, calendar_event_to_wire, contact_card_to_wire,
};
use crate::sync::pool::{Pool, effective_workers};
use crate::sync::prune::{TargetObj, candidates};
use crate::sync::{Context, TypeCounts};
use crate::types::ObjectType;

fn target_uid(v: &Value) -> Option<String> {
    v.get("uid").and_then(Value::as_str).map(str::to_owned)
}

struct CreateState<'a, 'c> {
    uploader: Uploader<'c>,
    touched_by_cid: HashMap<String, (i64, Vec<i64>)>,
    first_err: Option<Error>,
    counts: &'a mut TypeCounts,
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

    let workers = effective_workers(ctx.common.threads, &net.limits, false);
    let pool: Pool<(String, Value), (String, Result<SetOutcome, JmapError>)> =
        Pool::new(workers, {
            let net = Arc::new(net.clone());
            move |(cid, wire): (String, Value)| {
                let res = create_batch(&net, ty, vec![(cid.clone(), wire)]);
                (cid, res)
            }
        });
    let window = workers * 2;
    let mut in_flight = 0usize;

    let mut matched_uids: HashSet<String> = HashSet::new();
    let mut state = CreateState {
        uploader: Uploader::new(net, &ctx.conn),
        touched_by_cid: HashMap::new(),
        first_err: None,
        counts,
    };
    for (local, uid) in &rows {
        if state.first_err.is_some() {
            break;
        }
        if let Some(tid) = by_uid.get(uid) {
            maps.insert(ty, *local, crate::jmap::wire::JmapId(tid.clone()));
            matched_uids.insert(uid.clone());
            state.counts.skipped += 1;
            crate::progress::advance(1);
            continue;
        }
        let cid = format!("c{local}");
        let _ = state.uploader.take_touched();
        let wire = match build_wire(ctx, ty, *local, maps, &mut state.uploader) {
            Ok(w) => w,
            Err(e) => {
                logger.warn(&format!("{} local {local} skipped: {e}", ty.jmap_name()));
                state.counts.failed += 1;
                crate::progress::advance(1);
                continue;
            }
        };
        let touched = state.uploader.take_touched();
        state.touched_by_cid.insert(cid.clone(), (*local, touched));
        pool.submit((cid, wire));
        in_flight += 1;
        if in_flight >= window
            && let Ok(res) = pool.results().recv()
        {
            in_flight -= 1;
            handle_result(ctx, net, ty, res, maps, &mut state, logger);
        }
    }
    for res in pool.finish() {
        handle_result(ctx, net, ty, res, maps, &mut state, logger);
    }
    if let Some(e) = state.first_err {
        return Err(e);
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

fn handle_result(
    ctx: &Context,
    net: &Net,
    ty: ObjectType,
    res: (String, Result<SetOutcome, JmapError>),
    maps: &mut Maps,
    state: &mut CreateState<'_, '_>,
    logger: &Logger,
) {
    let (cid, res) = res;
    let outcome = match res {
        Ok(o) => o,
        Err(e) => {
            if state.first_err.is_none() {
                state.first_err = Some(Error::from(e));
            }
            return;
        }
    };
    let Some((local, touched)) = state.touched_by_cid.remove(&cid) else {
        logger.warn(&format!(
            "{} {cid}: internal error: no pending create for this id",
            ty.jmap_name()
        ));
        state.counts.failed += 1;
        crate::progress::advance(1);
        return;
    };
    let outcome = match retry_if_blob_missing(
        net,
        ty,
        &cid,
        &mut state.uploader,
        touched,
        outcome,
        |up| build_wire(ctx, ty, local, maps, up),
    ) {
        Ok(o) => o,
        Err(e) => {
            logger.warn(&format!("{} local {local} skipped: {e}", ty.jmap_name()));
            state.counts.failed += 1;
            crate::progress::advance(1);
            return;
        }
    };
    for (cid, v) in &outcome.created {
        if let Some(parsed) = cid.strip_prefix('c').and_then(|s| s.parse::<i64>().ok())
            && let Some(id) = jid(v)
        {
            maps.insert(ty, parsed, crate::jmap::wire::JmapId(id));
            state.counts.created += 1;
        }
        crate::progress::advance(1);
    }
    for (cid, err) in &outcome.not_created {
        logger.warn(&format!("{} {cid} not created: {err}", ty.jmap_name()));
        state.counts.failed += 1;
        crate::progress::advance(1);
    }
}

fn build_wire(
    ctx: &Context,
    ty: ObjectType,
    local: i64,
    maps: &Maps,
    up: &mut Uploader<'_>,
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
        contact_card_to_wire(&uid, &abids, &data, maps, up).map_err(Error::from)
    } else {
        let (cal, dr, ud, data): (String, i64, i64, String) = ctx
            .conn
            .query_row(
                &format!("{CALENDAR_EVENT_SELECT} AND id = ?1"),
                rusqlite::params![local],
                |r| Ok((r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .map_err(|e| Error::Partial(e.to_string()))?;
        calendar_event_to_wire(&cal, dr != 0, ud != 0, &data, maps, up).map_err(Error::from)
    }
}
