/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashSet;

use serde_json::Value;

use super::common::{chunk_size, create_batch, target_get_all};
use super::{Maps, Net, Plan};
use crate::error::Error;
use crate::logging::Logger;
use crate::sync::import_jmap::mapping::{
    IDENTITY_SELECT, PARTICIPANT_IDENTITY_SELECT, row_to_identity, row_to_participant_identity,
};
use crate::sync::keys::{identity_key, participant_identity_key};
use crate::sync::{Context, TypeCounts};
use crate::types::ObjectType;

fn run(
    net: &Net,
    ty: ObjectType,
    counts: &mut TypeCounts,
    logger: &Logger,
    target_key: impl Fn(&Value) -> Option<[u8; 32]>,
    local_rows: Vec<(i64, [u8; 32], Value)>,
) -> Result<Plan, Error> {
    let targets = target_get_all(net, ty).map_err(Error::from)?;
    let mut target_keys: HashSet<[u8; 32]> = HashSet::new();
    for t in &targets {
        if let Some(k) = target_key(t) {
            target_keys.insert(k);
        }
    }

    let mut seen: HashSet<[u8; 32]> = HashSet::new();
    let mut batch = Vec::new();
    for (local, key, wire) in local_rows {
        if target_keys.contains(&key) || !seen.insert(key) {
            counts.skipped += 1;
            crate::progress::advance(1);
            continue;
        }
        batch.push((format!("c{local}"), wire));
    }
    for chunk in batch.chunks(chunk_size(&net.limits)) {
        let outcome = create_batch(net, ty, chunk.to_vec()).map_err(Error::from)?;
        counts.created += outcome.created.len() as u64;
        crate::progress::advance(outcome.created.len() as u64);
        for (cid, err) in &outcome.not_created {
            logger.warn(&format!(
                "{} {cid} not created (expected for non-owned addresses): {err}",
                ty.jmap_name()
            ));
            counts.skipped += 1;
            crate::progress::advance(1);
        }
    }
    Ok(Plan::default())
}

pub fn reconcile_identity(
    ctx: &Context,
    net: &Net,
    _maps: &mut Maps,
    counts: &mut TypeCounts,
    logger: &Logger,
) -> Result<Plan, Error> {
    let mut stmt = ctx
        .conn
        .prepare(IDENTITY_SELECT)
        .map_err(|e| Error::Partial(e.to_string()))?;
    let rows: Vec<(i64, [u8; 32], Value)> = stmt
        .query_map([], |row| {
            let id: i64 = row.get(0)?;
            Ok((id, row_to_identity(row)))
        })
        .and_then(|m| m.collect::<Result<Vec<_>, _>>())
        .map_err(|e| Error::Partial(e.to_string()))?
        .into_iter()
        .map(|(id, w)| {
            let w = w.map_err(Error::from)?;
            let key = identity_key(&w.name, &w.email);
            let v = serde_json::to_value(&w).map_err(|e| Error::Partial(e.to_string()))?;
            Ok((id, key, v))
        })
        .collect::<Result<_, Error>>()?;
    drop(stmt);
    run(
        net,
        ObjectType::Identity,
        counts,
        logger,
        |t| {
            Some(identity_key(
                t.get("name").and_then(Value::as_str).unwrap_or(""),
                t.get("email").and_then(Value::as_str).unwrap_or(""),
            ))
        },
        rows,
    )
}

pub fn reconcile_participant(
    ctx: &Context,
    net: &Net,
    _maps: &mut Maps,
    counts: &mut TypeCounts,
    logger: &Logger,
) -> Result<Plan, Error> {
    let mut stmt = ctx
        .conn
        .prepare(PARTICIPANT_IDENTITY_SELECT)
        .map_err(|e| Error::Partial(e.to_string()))?;
    let rows: Vec<(i64, [u8; 32], Value)> = stmt
        .query_map([], |row| {
            let id: i64 = row.get(0)?;
            Ok((id, row_to_participant_identity(row)))
        })
        .and_then(|m| m.collect::<Result<Vec<_>, _>>())
        .map_err(|e| Error::Partial(e.to_string()))?
        .into_iter()
        .map(|(id, w)| {
            let w = w.map_err(Error::from)?;
            let key = participant_identity_key(&w.calendar_address, &w.name);
            let v = serde_json::to_value(&w).map_err(|e| Error::Partial(e.to_string()))?;
            Ok((id, key, v))
        })
        .collect::<Result<_, Error>>()?;
    drop(stmt);
    run(
        net,
        ObjectType::ParticipantIdentity,
        counts,
        logger,
        |t| {
            Some(participant_identity_key(
                t.get("calendarAddress")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                t.get("name").and_then(Value::as_str).unwrap_or(""),
            ))
        },
        rows,
    )
}
