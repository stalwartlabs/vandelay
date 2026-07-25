/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};

use rusqlite::params;
use serde_json::Value;

use super::common::{create_batch, jid, retry_if_blob_missing, target_query_get};
use super::{Net, Plan, Uploader};
use crate::error::Error;
use crate::jmap::wire::JmapId;
use crate::logging::Logger;
use crate::sync::import_jmap::mapping::{
    FILE_NODE_SELECT, MAILBOX_SELECT, TargetResolver, row_to_file_node, row_to_mailbox,
};
use crate::sync::keys::fold_name;
use crate::sync::{Context, TypeCounts};
use crate::types::ObjectType;

use super::Maps;

struct LocalNode {
    local: i64,
    parent: Option<i64>,
    name: String,
    role: Option<String>,
}

struct TargetNode {
    id: String,
    parent: Option<String>,
    name: String,
    role: Option<String>,
    may_delete: bool,
}

fn load_local(ctx: &Context, ty: ObjectType) -> Result<Vec<LocalNode>, Error> {
    let table = crate::sync::table_name(ty);
    let mut stmt = ctx
        .conn
        .prepare(&format!("SELECT id, parent_id, name, role FROM {table}"))
        .map_err(|e| Error::Partial(e.to_string()))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(LocalNode {
                local: r.get(0)?,
                parent: r.get(1)?,
                name: r.get(2)?,
                role: r.get(3)?,
            })
        })
        .and_then(|m| m.collect::<Result<Vec<_>, _>>())
        .map_err(|e| Error::Partial(e.to_string()))?;
    Ok(rows)
}

fn load_target(net: &Net, ty: ObjectType) -> Result<Vec<TargetNode>, Error> {
    let props: &[&str] = &["role", "name", "parentId", "myRights"];
    let list = target_query_get(net, ty, Some(props)).map_err(Error::from)?;
    Ok(list
        .iter()
        .filter_map(|v| {
            Some(TargetNode {
                id: jid(v)?,
                parent: v.get("parentId").and_then(Value::as_str).map(str::to_owned),
                name: v.get("name").and_then(Value::as_str)?.to_owned(),
                role: v.get("role").and_then(Value::as_str).map(str::to_owned),
                may_delete: v
                    .get("myRights")
                    .and_then(|r| r.get("mayDelete"))
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            })
        })
        .collect())
}

fn depth(local: i64, by: &HashMap<i64, Option<i64>>) -> usize {
    let mut d = 0;
    let mut cur = by.get(&local).copied().flatten();
    let mut seen = HashSet::new();
    while let Some(p) = cur {
        if !seen.insert(p) {
            break;
        }
        d += 1;
        cur = by.get(&p).copied().flatten();
    }
    d
}

fn already_exists_id(err: &Value) -> Option<String> {
    if err.get("type").and_then(Value::as_str) == Some("alreadyExists") {
        err.get("existingId")
            .and_then(Value::as_str)
            .map(str::to_owned)
    } else {
        None
    }
}

fn find_name_collision(
    targets: &[TargetNode],
    parent_target: Option<&str>,
    name: &str,
) -> Option<String> {
    targets
        .iter()
        .find(|t| t.parent.as_deref() == parent_target && fold_name(&t.name) == fold_name(name))
        .map(|t| t.id.clone())
}

fn record_merge(
    ty: ObjectType,
    local: i64,
    target_id: String,
    reason: &str,
    maps: &mut Maps,
    tmatched: &mut HashSet<String>,
    logger: &Logger,
) {
    logger.warn(&format!(
        "{} local {local} merged into existing target {target_id}: {reason}",
        ty.jmap_name()
    ));
    maps.insert(ty, local, JmapId(target_id.clone()));
    tmatched.insert(target_id);
}

pub fn reconcile(
    ctx: &Context,
    net: &Net,
    ty: ObjectType,
    maps: &mut Maps,
    counts: &mut TypeCounts,
    logger: &Logger,
) -> Result<Plan, Error> {
    let locals = load_local(ctx, ty)?;
    let targets = load_target(net, ty)?;

    let mut matched: HashMap<i64, String> = HashMap::new();
    let mut tmatched: HashSet<String> = HashSet::new();

    if ty == ObjectType::Mailbox {
        for t in &targets {
            if let Some(r) = &t.role
                && let Some(l) = locals.iter().find(|l| {
                    l.role.as_deref() == Some(r.as_str()) && !matched.contains_key(&l.local)
                })
            {
                matched.insert(l.local, t.id.clone());
                tmatched.insert(t.id.clone());
            }
        }
    }

    let mut stack: Vec<(Option<i64>, Option<String>)> = vec![(None, None)];
    for (l, tid) in matched.clone() {
        stack.push((Some(l), Some(tid)));
    }
    while let Some((lp, tp)) = stack.pop() {
        let lchildren: Vec<&LocalNode> = locals
            .iter()
            .filter(|n| n.parent == lp && !matched.contains_key(&n.local))
            .collect();
        for ln in lchildren {
            if let Some(tn) = targets.iter().find(|t| {
                t.parent.as_deref() == tp.as_deref()
                    && !tmatched.contains(&t.id)
                    && fold_name(&t.name) == fold_name(&ln.name)
            }) {
                matched.insert(ln.local, tn.id.clone());
                tmatched.insert(tn.id.clone());
                stack.push((Some(ln.local), Some(tn.id.clone())));
            }
        }
    }

    for (l, tid) in &matched {
        maps.insert(ty, *l, JmapId(tid.clone()));
    }
    counts.skipped += matched.len() as u64;
    crate::progress::advance(matched.len() as u64);

    let by: HashMap<i64, Option<i64>> = locals.iter().map(|n| (n.local, n.parent)).collect();
    let mut to_create: Vec<&LocalNode> = locals
        .iter()
        .filter(|n| !matched.contains_key(&n.local))
        .collect();
    to_create.sort_by_key(|n| depth(n.local, &by));
    let max_depth = to_create
        .iter()
        .map(|n| depth(n.local, &by))
        .max()
        .unwrap_or(0);

    let mut uploader = Uploader::new(net, &ctx.conn);
    let mut taken_roles: HashSet<String> = targets.iter().filter_map(|t| t.role.clone()).collect();
    let interleave = ty == ObjectType::FileNode;
    for d in 0..=max_depth {
        let level: Vec<&LocalNode> = to_create
            .iter()
            .copied()
            .filter(|n| depth(n.local, &by) == d)
            .collect();
        if interleave {
            for n in &level {
                let parent_target = match n.parent {
                    None => None,
                    Some(p) => match maps.target(ty, p) {
                        Some(t) => Some(t.0),
                        None => {
                            logger.warn(&format!(
                                "{} local {} skipped: parent {} not created",
                                ty.jmap_name(),
                                n.local,
                                p
                            ));
                            counts.failed += 1;
                            continue;
                        }
                    },
                };
                if let Some(tid) = find_name_collision(&targets, parent_target.as_deref(), &n.name)
                {
                    record_merge(
                        ty,
                        n.local,
                        tid,
                        "name already present on target",
                        maps,
                        &mut tmatched,
                        logger,
                    );
                    counts.skipped += 1;
                    continue;
                }
                let cid = format!("c{}", n.local);
                let _ = uploader.take_touched();
                let obj = match build_create(ctx, ty, n.local, maps, &mut uploader) {
                    Ok(o) => o,
                    Err(e) => {
                        logger.warn(&format!(
                            "{} local {} skipped: {e}",
                            ty.jmap_name(),
                            n.local
                        ));
                        counts.failed += 1;
                        continue;
                    }
                };
                let touched = uploader.take_touched();
                let outcome =
                    create_batch(net, ty, vec![(cid.clone(), obj)]).map_err(Error::from)?;
                let outcome = match retry_if_blob_missing(
                    net,
                    ty,
                    &cid,
                    &mut uploader,
                    touched,
                    outcome,
                    |up| build_create(ctx, ty, n.local, maps, up),
                ) {
                    Ok(o) => o,
                    Err(e) => {
                        logger.warn(&format!(
                            "{} local {} skipped: {e}",
                            ty.jmap_name(),
                            n.local
                        ));
                        counts.failed += 1;
                        continue;
                    }
                };
                for (cid, v) in &outcome.created {
                    if let Some(local) = cid.strip_prefix('c').and_then(|s| s.parse::<i64>().ok())
                        && let Some(id) = jid(v)
                    {
                        maps.insert(ty, local, JmapId(id));
                        counts.created += 1;
                    }
                }
                for (cid, err) in &outcome.not_created {
                    let local = cid.strip_prefix('c').and_then(|s| s.parse::<i64>().ok());
                    if let (Some(local), Some(existing)) = (local, already_exists_id(err)) {
                        record_merge(
                            ty,
                            local,
                            existing,
                            "alreadyExists: reusing existing target",
                            maps,
                            &mut tmatched,
                            logger,
                        );
                        counts.skipped += 1;
                        continue;
                    }
                    logger.warn(&format!("{} {cid} not created: {err}", ty.jmap_name()));
                    counts.failed += 1;
                }
            }
            continue;
        }
        let mut batch: Vec<(String, Value)> = Vec::new();
        for n in &level {
            let parent_target = match n.parent {
                None => None,
                Some(p) => match maps.target(ty, p) {
                    Some(t) => Some(t.0),
                    None => {
                        logger.warn(&format!(
                            "{} local {} skipped: parent {} not created",
                            ty.jmap_name(),
                            n.local,
                            p
                        ));
                        counts.failed += 1;
                        continue;
                    }
                },
            };
            if let Some(tid) = find_name_collision(&targets, parent_target.as_deref(), &n.name) {
                record_merge(
                    ty,
                    n.local,
                    tid,
                    "name already present on target",
                    maps,
                    &mut tmatched,
                    logger,
                );
                counts.skipped += 1;
                continue;
            }
            match build_create(ctx, ty, n.local, maps, &mut uploader) {
                Ok(mut obj) => {
                    if ty == ObjectType::Mailbox
                        && let Some(r) = n.role.as_deref()
                    {
                        if taken_roles.contains(r) {
                            if let Value::Object(m) = &mut obj {
                                m.remove("role");
                            }
                            logger.warn(&format!(
                                "Mailbox local {}: role '{r}' already present on target, creating as a plain folder",
                                n.local
                            ));
                        } else {
                            taken_roles.insert(r.to_owned());
                        }
                    }
                    batch.push((format!("c{}", n.local), obj));
                }
                Err(e) => {
                    logger.warn(&format!(
                        "{} local {} skipped: {e}",
                        ty.jmap_name(),
                        n.local
                    ));
                    counts.failed += 1;
                }
            }
        }
        if batch.is_empty() {
            continue;
        }
        let outcome = create_batch(net, ty, batch).map_err(Error::from)?;
        for (cid, v) in &outcome.created {
            if let Some(local) = cid.strip_prefix('c').and_then(|s| s.parse::<i64>().ok())
                && let Some(id) = jid(v)
            {
                maps.insert(ty, local, JmapId(id));
                counts.created += 1;
            }
        }
        for (cid, err) in &outcome.not_created {
            let local = cid.strip_prefix('c').and_then(|s| s.parse::<i64>().ok());
            if let (Some(local), Some(existing)) = (local, already_exists_id(err)) {
                record_merge(
                    ty,
                    local,
                    existing,
                    "alreadyExists: reusing existing target",
                    maps,
                    &mut tmatched,
                    logger,
                );
                counts.skipped += 1;
                continue;
            }
            logger.warn(&format!("{} {cid} not created: {err}", ty.jmap_name()));
            counts.failed += 1;
        }
    }

    let objs: Vec<crate::sync::prune::TargetObj> = targets
        .iter()
        .map(|t| crate::sync::prune::TargetObj {
            id: t.id.clone(),
            matched: tmatched.contains(&t.id),
            protected: t.role.is_some(),
            may_delete: t.may_delete,
            parent: t.parent.clone(),
        })
        .collect();
    Ok(Plan {
        prune_candidates: crate::sync::prune::candidates(&objs, true),
        active_sieve_target: None,
    })
}

fn build_create(
    ctx: &Context,
    ty: ObjectType,
    local: i64,
    maps: &Maps,
    uploader: &mut Uploader<'_>,
) -> Result<Value, Error> {
    if ty == ObjectType::Mailbox {
        let wire = ctx
            .conn
            .query_row(
                &format!("{MAILBOX_SELECT} WHERE id = ?1"),
                params![local],
                |row| Ok(row_to_mailbox(row, maps)),
            )
            .map_err(|e| Error::Partial(e.to_string()))?
            .map_err(Error::from)?;
        return serde_json::to_value(&wire).map_err(|e| Error::Partial(e.to_string()));
    }
    let fnrow = ctx
        .conn
        .query_row(
            &format!("{FILE_NODE_SELECT} WHERE id = ?1"),
            params![local],
            |row| Ok(row_to_file_node(row, maps)),
        )
        .map_err(|e| Error::Partial(e.to_string()))?
        .map_err(Error::from)?;
    let mut wire = fnrow.wire;
    if let (crate::jmap::wire::file_node::NodeType::File, Some(blob_local)) =
        (&wire.node_type, fnrow.blob_local_id)
    {
        let ct = wire
            .media_type
            .clone()
            .unwrap_or_else(|| "application/octet-stream".to_owned());
        let id = uploader.upload_with(blob_local, &ct).map_err(Error::from)?;
        wire.blob_id = Some(id);
    }
    let mut obj = serde_json::to_value(&wire).map_err(|e| Error::Partial(e.to_string()))?;
    if let Value::Object(m) = &mut obj {
        m.remove("created");
        m.remove("modified");
        m.remove("isSubscribed");
    }
    Ok(obj)
}
