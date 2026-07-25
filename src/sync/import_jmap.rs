/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

pub mod diff;
pub mod mapping;
pub mod tree;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rusqlite::Connection;
use serde_json::Value;

use self::diff::diff;
use self::mapping::{BlobIntern, LocalResolver};
use self::tree::topo_order;
use super::pool::{Pool, effective_workers};
use crate::db;
use crate::db::sources::SourceKey;
use crate::error::Error;
use crate::jmap::blobxfer;
use crate::jmap::connect::{self, Connected};
use crate::jmap::error::JmapError;
use crate::jmap::http::{Auth, HttpClient};
use crate::jmap::request::{get_all, get_changes, get_objects, get_state, query_all_ids};
use crate::jmap::session::{Limits, Session};
use crate::jmap::wire::JmapId;
use crate::jmap::wire::email::Email;
use crate::jmap::wire::file_node::{FileNode, NodeType};
use crate::jmap::wire::sieve_script::SieveScript;
use crate::logging::{LEVEL_DEFAULT, LEVEL_PROGRESS, Logger};
use crate::sync::{CommonConfig, Context, ImportConfig, Summary, TypeCounts};
use crate::types::ObjectType;

const IMPORT_ORDER: [ObjectType; 10] = [
    ObjectType::Mailbox,
    ObjectType::AddressBook,
    ObjectType::Calendar,
    ObjectType::FileNode,
    ObjectType::Identity,
    ObjectType::SieveScript,
    ObjectType::ParticipantIdentity,
    ObjectType::Email,
    ObjectType::ContactCard,
    ObjectType::CalendarEvent,
];

fn is_queryless(ty: ObjectType) -> bool {
    matches!(
        ty,
        ObjectType::Identity
            | ObjectType::AddressBook
            | ObjectType::Calendar
            | ObjectType::ParticipantIdentity
    )
}

fn get_props(ty: ObjectType) -> Option<&'static [&'static str]> {
    match ty {
        ObjectType::Mailbox => Some(&["name", "parentId", "role", "sortOrder", "isSubscribed"]),
        ObjectType::Email => Some(&["blobId", "receivedAt", "mailboxIds", "keywords"]),
        ObjectType::SieveScript => Some(&["name", "isActive", "blobId"]),
        ObjectType::FileNode => Some(&[
            "parentId",
            "nodeType",
            "blobId",
            "target",
            "name",
            "type",
            "created",
            "modified",
            "isSubscribed",
            "role",
        ]),
        _ => None,
    }
}

type GetMsg = Result<Vec<Value>, JmapError>;
type BlobRef = (String, String, String);
type DlMsg = (String, Result<Vec<u8>, JmapError>);

#[derive(Clone)]
struct Net {
    client: HttpClient,
    api: String,
    account: String,
    limits: Limits,
    session: Session,
}

struct DbResolver<'a> {
    conn: &'a Connection,
    source_id: i64,
}

impl LocalResolver for DbResolver<'_> {
    fn local(&self, ty: ObjectType, jmap_id: &str) -> Option<i64> {
        db::ids::local_for(self.conn, self.source_id, ty, jmap_id)
            .ok()
            .flatten()
    }
}

struct PrefetchedBlobs<'a> {
    conn: &'a Connection,
    bytes: &'a HashMap<String, Vec<u8>>,
}

impl BlobIntern for PrefetchedBlobs<'_> {
    fn intern(&mut self, jmap_blob_id: &str) -> Result<i64, JmapError> {
        let data = self.bytes.get(jmap_blob_id).ok_or_else(|| {
            JmapError::malformed(format!("blob {jmap_blob_id} was not prefetched"))
        })?;
        Ok(db::blobs::intern_blob(self.conn, data)?)
    }
}

fn collect_blob_ids(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(b)) = map.get("blobId") {
                out.push(b.clone());
            }
            for v in map.values() {
                collect_blob_ids(v, out);
            }
        }
        Value::Array(items) => {
            for v in items {
                collect_blob_ids(v, out);
            }
        }
        _ => {}
    }
}

fn username_of(auth: &Auth) -> String {
    match auth {
        Auth::Basic { user, .. } => user.clone(),
        Auth::Bearer { .. } => "(bearer)".to_owned(),
    }
}

pub fn run(common: CommonConfig, config: ImportConfig) -> Result<Summary, Error> {
    let logger = common.logger;
    let ctx = Context::open(common, &config.connect)?;
    let connected = connect::prepare(&ctx, &config.connect)?;

    let work = work_list(&config, &connected);
    let key = SourceKey {
        kind: "jmap".to_owned(),
        session_url: config.connect.url.clone(),
        account_id: connected.account_id.clone(),
    };

    if !ctx.dry_run()
        && let Some((url, acc)) =
            db::sources::conflicting_source(&ctx.conn, "jmap", &key.session_url, &key.account_id)
                .map_err(|e| Error::Partial(e.to_string()))?
        && !config.allow_source_change
    {
        return Err(Error::SourceChange(format!(
            "archive already records JMAP source ({url}, account {acc}); \
             pass --allow-source-change to import a different account"
        )));
    }

    let source_id = if ctx.dry_run() {
        db::sources::find_source(&ctx.conn, &key).map_err(|e| Error::Partial(e.to_string()))?
    } else {
        let account_name = match &config.connect.account {
            crate::jmap::account::AccountSelector::Name(n) => Some(n.clone()),
            crate::jmap::account::AccountSelector::Id(_) => None,
        };
        Some(
            db::sources::upsert_source(
                &ctx.conn,
                &key,
                account_name.as_deref(),
                &username_of(ctx.client.auth()),
            )
            .map_err(|e| Error::Partial(e.to_string()))?,
        )
    };

    let net = Net {
        client: ctx.client.clone(),
        api: connected.session.api_url.clone(),
        account: connected.account_id.clone(),
        limits: connected.limits,
        session: connected.session.clone(),
    };

    let mut summary = Summary::default();
    let mut dry_rows: Vec<(&'static str, u64, u64, u64)> = Vec::new();
    let threads = ctx.common.threads;

    for ty in work {
        if logger.enabled(LEVEL_DEFAULT) {
            eprintln!("import: {} ...", ty.jmap_name());
        }
        let mut counts = TypeCounts::default();
        // The object count only becomes known once the server has been
        // queried, so the phase starts without a total and reports a rate.
        crate::progress::start(ty.jmap_name(), None);
        match reconcile_type(
            &ctx,
            &net,
            ty,
            source_id,
            threads,
            &logger,
            &mut counts,
            &mut dry_rows,
        ) {
            Ok(()) => {}
            Err(e) => {
                logger.warn(&format!(
                    "type {} aborted: {e}; continuing (run will exit 5)",
                    ty.jmap_name()
                ));
                counts.failed += 1;
            }
        }
        crate::progress::finish();
        summary.per_type.push((ty.jmap_name(), counts));
    }

    if ctx.dry_run() {
        print_dry_run(&dry_rows);
        return Ok(Summary::default());
    }

    if !summary.any_failed()
        && let Err(e) = run_gc(&ctx.conn)
    {
        logger.warn(&format!("blob GC skipped: {e}"));
    }

    summary.retries_observed = ctx.client.retries_observed();
    summary.retry_after_sleeps = ctx.client.retry_after_sleeps();
    Ok(summary)
}

fn work_list(config: &ImportConfig, connected: &Connected) -> Vec<ObjectType> {
    let selected = config.objects.as_ref();
    IMPORT_ORDER
        .into_iter()
        .filter(|ty| connected.supports(*ty))
        .filter(|ty| selected.map(|s| s.contains(ty)).unwrap_or(true))
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn reconcile_type(
    ctx: &Context,
    net: &Net,
    ty: ObjectType,
    source_id: Option<i64>,
    threads: usize,
    logger: &Logger,
    counts: &mut TypeCounts,
    dry_rows: &mut Vec<(&'static str, u64, u64, u64)>,
) -> Result<(), Error> {
    let local_map: HashMap<String, i64> = match source_id {
        Some(sid) => {
            db::ids::jmap_to_local(&ctx.conn, sid, ty).map_err(|e| Error::Partial(e.to_string()))?
        }
        None => HashMap::new(),
    };
    let local_ids: HashSet<String> = local_map.keys().cloned().collect();

    let (server_ids, preloaded, enum_state): (Vec<JmapId>, Option<Vec<Value>>, Option<String>) =
        if is_queryless(ty) {
            let got = get_all::<Value>(&net.client, &net.api, &net.account, ty.jmap_name())
                .map_err(Error::from)?;
            let ids = got
                .list
                .iter()
                .filter_map(|v| {
                    v.get("id")
                        .and_then(Value::as_str)
                        .map(|s| JmapId(s.to_owned()))
                })
                .collect();
            (ids, Some(got.list), got.state)
        } else {
            let ids = query_all_ids(
                &net.client,
                &net.api,
                &net.account,
                ty.jmap_name(),
                &net.limits,
            )
            .map_err(Error::from)?;
            (ids, None, None)
        };

    let d = diff(&server_ids, &local_ids);

    if ctx.dry_run() {
        dry_rows.push((
            ty.jmap_name(),
            d.new.len() as u64,
            d.vanished.len() as u64,
            d.present.len() as u64,
        ));
        return Ok(());
    }

    let source_id = source_id.expect("source_id present outside dry-run");

    let cursor = db::sync_state_jmap::get(&ctx.conn, source_id, ty)
        .map_err(|e| Error::Partial(e.to_string()))?;
    let run_state = if !supports_changes(ty) {
        None
    } else if is_queryless(ty) {
        enum_state
    } else if cursor.is_none() {
        get_state(&net.client, &net.api, &net.account, ty.jmap_name()).map_err(Error::from)?
    } else {
        None
    };

    if !d.new.is_empty() {
        let objects = match preloaded {
            Some(list) => {
                let want: HashSet<&str> = d.new.iter().map(|i| i.0.as_str()).collect();
                list.into_iter()
                    .filter(|v| {
                        v.get("id")
                            .and_then(Value::as_str)
                            .map(|s| want.contains(s))
                            .unwrap_or(false)
                    })
                    .collect()
            }
            None => fetch_objects(net, ty, &d.new, get_props(ty), threads, logger, counts),
        };
        insert_objects(ctx, net, ty, source_id, objects, threads, logger, counts)?;
    }

    if !d.vanished.is_empty() {
        delete_vanished(&ctx.conn, ty, source_id, &d.vanished, &local_map, logger)?;
        counts.deleted += d.vanished.len() as u64;
    }

    reconcile_updates(
        ctx, net, ty, source_id, &d.present, &local_map, cursor, run_state, threads, logger, counts,
    )?;

    if logger.enabled(LEVEL_DEFAULT) {
        eprintln!(
            "import: {} done (fetched={} deleted={} skipped={} failed={})",
            ty.jmap_name(),
            counts.fetched,
            counts.deleted,
            counts.skipped,
            counts.failed
        );
    }
    Ok(())
}

fn fetch_objects(
    net: &Net,
    ty: ObjectType,
    new_ids: &[JmapId],
    props: Option<&'static [&'static str]>,
    threads: usize,
    logger: &Logger,
    counts: &mut TypeCounts,
) -> Vec<Value> {
    let chunk = net.limits.max_objects_in_get.max(1) as usize;
    let workers = effective_workers(threads, &net.limits, false);
    let net = Arc::new(net.clone());
    let pool: Pool<Vec<JmapId>, GetMsg> = Pool::new(workers, {
        let net = net.clone();
        move |ids: Vec<JmapId>| {
            get_objects::<Value>(
                &net.client,
                &net.api,
                &net.account,
                ty.jmap_name(),
                &ids,
                props,
                &net.limits,
            )
            .map(|r| r.list)
        }
    });

    let mut batches = 0u64;
    for c in new_ids.chunks(chunk) {
        pool.submit(c.to_vec());
        batches += 1;
    }
    let mut out = Vec::with_capacity(new_ids.len());
    let mut done = 0u64;
    for res in pool.finish() {
        match res {
            Ok(list) => out.extend(list),
            Err(e) => {
                logger.warn(&format!("{} /get chunk failed: {e}", ty.jmap_name()));
                counts.failed += 1;
            }
        }
        done += 1;
    }
    if logger.enabled(LEVEL_PROGRESS) {
        eprintln!(
            "import: {} fetched {}/{} chunks",
            ty.jmap_name(),
            done,
            batches
        );
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn insert_objects(
    ctx: &Context,
    net: &Net,
    ty: ObjectType,
    source_id: i64,
    objects: Vec<Value>,
    threads: usize,
    logger: &Logger,
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let existing_parents: HashSet<String> =
        if matches!(ty, ObjectType::Mailbox | ObjectType::FileNode) {
            db::ids::jmap_to_local(&ctx.conn, source_id, ty)
                .map(|m| m.into_keys().collect())
                .map_err(|e| Error::Partial(e.to_string()))?
        } else {
            HashSet::new()
        };
    let (ordered, warnings) = order_objects(ty, &objects, &existing_parents);
    for w in &warnings {
        logger.warn(w);
    }

    for batch in ordered.chunks(200) {
        let blob_refs = blob_references(ty, batch.iter().map(|&i| &objects[i]));
        let blobs = if blob_refs.is_empty() {
            HashMap::new()
        } else {
            download_blobs(net, blob_refs, threads, logger)
        };
        let tx = ctx
            .conn
            .unchecked_transaction()
            .map_err(|e| Error::Partial(e.to_string()))?;
        for &idx in batch {
            let obj = &objects[idx];
            let jmap_id = match obj.get("id").and_then(Value::as_str) {
                Some(s) => s.to_owned(),
                None => {
                    counts.failed += 1;
                    continue;
                }
            };
            match insert_one(&tx, ty, source_id, obj, &blobs) {
                Ok(local_id) => {
                    db::ids::insert(&tx, source_id, ty, &jmap_id, local_id)
                        .map_err(|e| Error::Partial(e.to_string()))?;
                    counts.fetched += 1;
                    crate::progress::advance(1);
                }
                Err(e) => {
                    logger.warn(&format!("{} {jmap_id} skipped: {e}", ty.jmap_name()));
                    counts.failed += 1;
                }
            }
        }
        tx.commit().map_err(|e| Error::Partial(e.to_string()))?;
    }
    Ok(())
}

fn blob_references<'a>(ty: ObjectType, objects: impl Iterator<Item = &'a Value>) -> Vec<BlobRef> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut refs = Vec::new();
    match ty {
        ObjectType::Email => {
            for o in objects {
                if let Some(b) = o.get("blobId").and_then(Value::as_str)
                    && seen.insert(b.to_owned())
                {
                    refs.push((
                        b.to_owned(),
                        "message/rfc822".to_owned(),
                        "email".to_owned(),
                    ));
                }
            }
        }
        ObjectType::SieveScript => {
            for o in objects {
                if let Some(b) = o.get("blobId").and_then(Value::as_str)
                    && seen.insert(b.to_owned())
                {
                    refs.push((
                        b.to_owned(),
                        "application/sieve".to_owned(),
                        "script".to_owned(),
                    ));
                }
            }
        }
        ObjectType::FileNode => {
            for o in objects {
                if let Some(b) = o.get("blobId").and_then(Value::as_str)
                    && seen.insert(b.to_owned())
                {
                    let mt = o
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("application/octet-stream")
                        .to_owned();
                    let name = o
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("file")
                        .to_owned();
                    refs.push((b.to_owned(), mt, name));
                }
            }
        }
        ObjectType::ContactCard | ObjectType::CalendarEvent => {
            for o in objects {
                let mut ids = Vec::new();
                collect_blob_ids(o, &mut ids);
                for b in ids {
                    if seen.insert(b.clone()) {
                        refs.push((b, "application/octet-stream".to_owned(), "blob".to_owned()));
                    }
                }
            }
        }
        _ => {}
    }
    refs
}

fn download_blobs(
    net: &Net,
    refs: Vec<BlobRef>,
    threads: usize,
    logger: &Logger,
) -> HashMap<String, Vec<u8>> {
    let workers = effective_workers(threads, &net.limits, true);
    let net_arc = Arc::new(net.clone());
    let pool: Pool<BlobRef, DlMsg> = Pool::new(workers, {
        let net = net_arc.clone();
        move |(blob_id, type_hint, name): (String, String, String)| {
            let r = blobxfer::download_bytes(
                &net.client,
                &net.session,
                &net.account,
                &blob_id,
                &type_hint,
                &name,
            );
            (blob_id, r)
        }
    });
    let total = refs.len();
    for r in refs {
        pool.submit(r);
    }
    let mut out = HashMap::new();
    for (blob_id, res) in pool.finish() {
        match res {
            Ok(bytes) => {
                out.insert(blob_id, bytes);
            }
            Err(e) => {
                logger.warn(&format!("blob {blob_id} download failed: {e}"));
            }
        }
    }
    if logger.enabled(LEVEL_PROGRESS) {
        eprintln!("import: downloaded {}/{} blobs", out.len(), total);
    }
    out
}

fn order_objects(
    ty: ObjectType,
    objects: &[Value],
    existing_parents: &HashSet<String>,
) -> (Vec<usize>, Vec<String>) {
    if !matches!(ty, ObjectType::Mailbox | ObjectType::FileNode) {
        return ((0..objects.len()).collect(), Vec::new());
    }
    let items: Vec<(String, Option<String>)> = objects
        .iter()
        .map(|o| {
            (
                o.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                o.get("parentId").and_then(Value::as_str).map(str::to_owned),
            )
        })
        .collect();
    let result = topo_order(&items, existing_parents);
    let mut warnings = Vec::new();
    for i in &result.orphans {
        let (id, parent) = &items[*i];
        warnings.push(format!(
            "{} {id}: parent {} not present in batch or archive; inserting with NULL parent",
            ty.jmap_name(),
            parent.as_deref().unwrap_or("<unknown>"),
        ));
    }
    for i in &result.cycle_roots {
        warnings.push(format!(
            "{} {}: parent cycle detected; broken at this node",
            ty.jmap_name(),
            items[*i].0,
        ));
    }
    (result.order, warnings)
}

fn insert_one(
    conn: &Connection,
    ty: ObjectType,
    source_id: i64,
    obj: &Value,
    blobs: &HashMap<String, Vec<u8>>,
) -> Result<i64, JmapError> {
    let resolver = DbResolver { conn, source_id };
    match ty {
        ObjectType::Mailbox => {
            let w = serde_json::from_value(obj.clone())?;
            mapping::insert_mailbox(conn, &w, &resolver)
        }
        ObjectType::Identity => {
            let w = serde_json::from_value(obj.clone())?;
            mapping::insert_identity(conn, &w)
        }
        ObjectType::AddressBook => {
            let w = serde_json::from_value(obj.clone())?;
            mapping::insert_address_book(conn, &w)
        }
        ObjectType::Calendar => {
            let w = serde_json::from_value(obj.clone())?;
            mapping::insert_calendar(conn, &w)
        }
        ObjectType::ParticipantIdentity => {
            let w = serde_json::from_value(obj.clone())?;
            mapping::insert_participant_identity(conn, &w)
        }
        ObjectType::Email => {
            let w: Email = serde_json::from_value(obj.clone())?;
            let data = blobs
                .get(&w.blob_id.0)
                .ok_or_else(|| JmapError::malformed("email blob missing"))?;
            let blob_local = db::blobs::intern_blob(conn, data)?;
            let mm = crate::sync::keys::index_to_json(
                &crate::sync::emailmeta::email_index_from_blob(data),
            );
            mapping::insert_email(conn, &w, blob_local, &mm, &resolver)
        }
        ObjectType::SieveScript => {
            let w: SieveScript = serde_json::from_value(obj.clone())?;
            let data = blobs
                .get(&w.blob_id.0)
                .ok_or_else(|| JmapError::malformed("sieve blob missing"))?;
            let blob_local = db::blobs::intern_blob(conn, data)?;
            mapping::insert_sieve_script(conn, &w, blob_local)
        }
        ObjectType::FileNode => {
            let w: FileNode = serde_json::from_value(obj.clone())?;
            let blob_local = match (&w.node_type, &w.blob_id) {
                (NodeType::File, Some(b)) => {
                    let data = blobs
                        .get(&b.0)
                        .ok_or_else(|| JmapError::malformed("file blob missing"))?;
                    Some(db::blobs::intern_blob(conn, data)?)
                }
                _ => None,
            };
            mapping::insert_file_node(conn, &w, blob_local, &resolver)
        }
        ObjectType::ContactCard => {
            let w = serde_json::from_value(obj.clone())?;
            let mut bi = PrefetchedBlobs { conn, bytes: blobs };
            mapping::insert_contact_card(conn, &w, &resolver, &mut bi)
        }
        ObjectType::CalendarEvent => {
            let w = serde_json::from_value(obj.clone())?;
            let mut bi = PrefetchedBlobs { conn, bytes: blobs };
            mapping::insert_calendar_event(conn, &w, &resolver, &mut bi)
        }
    }
}

fn supports_changes(ty: ObjectType) -> bool {
    !matches!(ty, ObjectType::SieveScript)
}

fn update_props(ty: ObjectType) -> Option<&'static [&'static str]> {
    match ty {
        ObjectType::Email => Some(&["mailboxIds", "keywords"]),
        other => get_props(other),
    }
}

#[allow(clippy::too_many_arguments)]
fn reconcile_updates(
    ctx: &Context,
    net: &Net,
    ty: ObjectType,
    source_id: i64,
    present: &[JmapId],
    local_map: &HashMap<String, i64>,
    cursor: Option<String>,
    run_state: Option<String>,
    threads: usize,
    logger: &Logger,
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    if supports_changes(ty)
        && let Some(since) = cursor
    {
        match get_changes(
            &net.client,
            &net.api,
            &net.account,
            ty.jmap_name(),
            &since,
            &net.limits,
        ) {
            Ok(ch) => {
                let present_set: HashSet<&str> = present.iter().map(|j| j.0.as_str()).collect();
                let updated: Vec<JmapId> = ch
                    .updated
                    .into_iter()
                    .filter(|j| present_set.contains(j.0.as_str()))
                    .collect();
                let clean = fetch_and_update(
                    ctx, net, ty, source_id, &updated, local_map, threads, logger, counts,
                )?;
                advance_state(ctx, ty, source_id, &ch.new_state, clean, logger)?;
            }
            Err(err)
                if matches!(
                    err,
                    JmapError::CannotCalculateChanges | JmapError::UnknownMethod
                ) =>
            {
                let reason = if matches!(err, JmapError::UnknownMethod) {
                    "server does not implement /changes"
                } else {
                    "server cannot calculate changes from stored state"
                };
                logger.warn(&format!(
                    "{}: {reason}; refreshing all present objects",
                    ty.jmap_name()
                ));
                let captured = get_state(&net.client, &net.api, &net.account, ty.jmap_name())
                    .map_err(Error::from)?;
                let clean = fetch_and_update(
                    ctx, net, ty, source_id, present, local_map, threads, logger, counts,
                )?;
                if let Some(s) = captured {
                    advance_state(ctx, ty, source_id, &s, clean, logger)?;
                }
            }
            Err(e) => return Err(Error::from(e)),
        }
        return Ok(());
    }

    let clean = fetch_and_update(
        ctx, net, ty, source_id, present, local_map, threads, logger, counts,
    )?;
    if let Some(s) = run_state {
        advance_state(ctx, ty, source_id, &s, clean, logger)?;
    }
    Ok(())
}

fn advance_state(
    ctx: &Context,
    ty: ObjectType,
    source_id: i64,
    state: &str,
    clean: bool,
    logger: &Logger,
) -> Result<(), Error> {
    if !clean {
        logger.warn(&format!(
            "{}: holding sync state; some updates failed and will be retried on the next run",
            ty.jmap_name()
        ));
        return Ok(());
    }
    db::sync_state_jmap::upsert(&ctx.conn, source_id, ty, state)
        .map_err(|e| Error::Partial(e.to_string()))
}

#[allow(clippy::too_many_arguments)]
fn fetch_and_update(
    ctx: &Context,
    net: &Net,
    ty: ObjectType,
    source_id: i64,
    ids: &[JmapId],
    local_map: &HashMap<String, i64>,
    threads: usize,
    logger: &Logger,
    counts: &mut TypeCounts,
) -> Result<bool, Error> {
    if ids.is_empty() {
        return Ok(true);
    }
    let failed_before = counts.failed;
    let objects = fetch_objects(net, ty, ids, update_props(ty), threads, logger, counts);
    update_objects(
        ctx, net, ty, source_id, objects, local_map, threads, logger, counts,
    )?;
    Ok(counts.failed == failed_before)
}

#[allow(clippy::too_many_arguments)]
fn update_objects(
    ctx: &Context,
    net: &Net,
    ty: ObjectType,
    source_id: i64,
    objects: Vec<Value>,
    local_map: &HashMap<String, i64>,
    threads: usize,
    logger: &Logger,
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    for batch in objects.chunks(200) {
        let blob_refs = blob_references(ty, batch.iter());
        let blobs = if blob_refs.is_empty() {
            HashMap::new()
        } else {
            download_blobs(net, blob_refs, threads, logger)
        };
        let tx = ctx
            .conn
            .unchecked_transaction()
            .map_err(|e| Error::Partial(e.to_string()))?;
        for obj in batch {
            let jmap_id = match obj.get("id").and_then(Value::as_str) {
                Some(s) => s.to_owned(),
                None => {
                    counts.failed += 1;
                    continue;
                }
            };
            let Some(&local_id) = local_map.get(&jmap_id) else {
                continue;
            };
            match update_one(&tx, ty, source_id, local_id, obj, &blobs) {
                Ok(true) => counts.updated += 1,
                Ok(false) => {}
                Err(e) => {
                    logger.warn(&format!("{} {jmap_id} update skipped: {e}", ty.jmap_name()));
                    counts.failed += 1;
                }
            }
        }
        tx.commit().map_err(|e| Error::Partial(e.to_string()))?;
    }
    Ok(())
}

fn update_one(
    conn: &Connection,
    ty: ObjectType,
    source_id: i64,
    local_id: i64,
    obj: &Value,
    blobs: &HashMap<String, Vec<u8>>,
) -> Result<bool, JmapError> {
    let resolver = DbResolver { conn, source_id };
    match ty {
        ObjectType::Mailbox => {
            let w = serde_json::from_value(obj.clone())?;
            mapping::update_mailbox(conn, local_id, &w, &resolver)
        }
        ObjectType::Identity => {
            let w = serde_json::from_value(obj.clone())?;
            mapping::update_identity(conn, local_id, &w)
        }
        ObjectType::AddressBook => {
            let w = serde_json::from_value(obj.clone())?;
            mapping::update_address_book(conn, local_id, &w)
        }
        ObjectType::Calendar => {
            let w = serde_json::from_value(obj.clone())?;
            mapping::update_calendar(conn, local_id, &w)
        }
        ObjectType::ParticipantIdentity => {
            let w = serde_json::from_value(obj.clone())?;
            mapping::update_participant_identity(conn, local_id, &w)
        }
        ObjectType::Email => mapping::update_email(conn, local_id, obj, &resolver),
        ObjectType::SieveScript => {
            let w: SieveScript = serde_json::from_value(obj.clone())?;
            let data = blobs
                .get(&w.blob_id.0)
                .ok_or_else(|| JmapError::malformed("sieve blob missing"))?;
            let blob_local = db::blobs::intern_blob(conn, data)?;
            mapping::update_sieve_script(conn, local_id, &w, blob_local)
        }
        ObjectType::FileNode => {
            let w: FileNode = serde_json::from_value(obj.clone())?;
            let blob_local = match (&w.node_type, &w.blob_id) {
                (NodeType::File, Some(b)) => {
                    let data = blobs
                        .get(&b.0)
                        .ok_or_else(|| JmapError::malformed("file blob missing"))?;
                    Some(db::blobs::intern_blob(conn, data)?)
                }
                _ => None,
            };
            mapping::update_file_node(conn, local_id, &w, blob_local, &resolver)
        }
        ObjectType::ContactCard => {
            let w = serde_json::from_value(obj.clone())?;
            let mut bi = PrefetchedBlobs { conn, bytes: blobs };
            mapping::update_contact_card(conn, local_id, &w, &resolver, &mut bi)
        }
        ObjectType::CalendarEvent => {
            let w = serde_json::from_value(obj.clone())?;
            let mut bi = PrefetchedBlobs { conn, bytes: blobs };
            mapping::update_calendar_event(conn, local_id, &w, &resolver, &mut bi)
        }
    }
}

fn delete_vanished(
    conn: &Connection,
    ty: ObjectType,
    source_id: i64,
    vanished: &[JmapId],
    local_map: &HashMap<String, i64>,
    logger: &Logger,
) -> Result<(), Error> {
    let locals: Vec<i64> = vanished
        .iter()
        .filter_map(|j| local_map.get(&j.0).copied())
        .collect();
    if locals.is_empty() {
        return Ok(());
    }
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| Error::Partial(e.to_string()))?;
    let vanished_set: HashSet<i64> = locals.iter().copied().collect();

    match ty {
        ObjectType::Mailbox | ObjectType::FileNode => {
            let table = if ty == ObjectType::Mailbox {
                "mailboxes"
            } else {
                "file_nodes"
            };
            let mut stmt = tx
                .prepare(&format!("SELECT id, parent_id FROM {table}"))
                .map_err(|e| Error::Partial(e.to_string()))?;
            let rows: Vec<(i64, Option<i64>)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .and_then(|m| m.collect())
                .map_err(|e| Error::Partial(e.to_string()))?;
            drop(stmt);
            for (id, parent) in &rows {
                if let Some(p) = parent
                    && vanished_set.contains(p)
                    && !vanished_set.contains(id)
                {
                    tx.execute(
                        &format!("UPDATE {table} SET parent_id = NULL WHERE id = ?1"),
                        rusqlite::params![id],
                    )
                    .map_err(|e| Error::Partial(e.to_string()))?;
                }
            }
            let by: HashMap<i64, Option<i64>> = rows.iter().copied().collect();
            let depth = |mut id: i64| -> usize {
                let mut d = 0;
                let mut seen = HashSet::new();
                while let Some(Some(p)) = by.get(&id) {
                    if !seen.insert(id) || !vanished_set.contains(p) {
                        break;
                    }
                    d += 1;
                    id = *p;
                }
                d
            };
            let depths: HashMap<i64, usize> = locals.iter().map(|id| (*id, depth(*id))).collect();
            let mut ordered = locals.clone();
            ordered.sort_by(|a, b| depths[b].cmp(&depths[a]).then(a.cmp(b)));
            for id in ordered {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE id = ?1"),
                    rusqlite::params![id],
                )
                .map_err(|e| Error::Partial(e.to_string()))?;
            }
        }
        _ => {
            let table = crate::sync::table_name(ty);
            for id in &locals {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE id = ?1"),
                    rusqlite::params![id],
                )
                .map_err(|e| Error::Partial(e.to_string()))?;
            }
        }
    }

    if let Some((table, column)) = cross_ref_target(ty) {
        prune_id_arrays(&tx, table, column, &vanished_set)
            .map_err(|e| Error::Partial(e.to_string()))?;
    }

    for j in vanished {
        db::ids::delete(&tx, source_id, ty, &j.0).map_err(|e| Error::Partial(e.to_string()))?;
    }
    tx.commit().map_err(|e| Error::Partial(e.to_string()))?;
    if logger.enabled(LEVEL_PROGRESS) {
        eprintln!(
            "import: {} deleted {} vanished",
            ty.jmap_name(),
            locals.len()
        );
    }
    Ok(())
}

fn cross_ref_target(ty: ObjectType) -> Option<(&'static str, &'static str)> {
    match ty {
        ObjectType::Mailbox => Some(("emails", "mailbox_ids")),
        ObjectType::Calendar => Some(("calendar_events", "calendar_ids")),
        ObjectType::AddressBook => Some(("contact_cards", "address_book_ids")),
        _ => None,
    }
}

fn prune_id_arrays(
    conn: &Connection,
    table: &str,
    column: &str,
    removed: &HashSet<i64>,
) -> Result<(), rusqlite::Error> {
    let mut stmt = conn.prepare(&format!("SELECT id, {column} FROM {table}"))?;
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    drop(stmt);
    for (id, json) in rows {
        let arr: Vec<i64> = serde_json::from_str(&json).unwrap_or_default();
        if arr.iter().any(|v| removed.contains(v)) {
            let kept: Vec<i64> = arr.into_iter().filter(|v| !removed.contains(v)).collect();
            let new_json = serde_json::to_string(&kept).unwrap_or_else(|_| "[]".to_owned());
            conn.execute(
                &format!("UPDATE {table} SET {column} = ?1 WHERE id = ?2"),
                rusqlite::params![new_json, id],
            )?;
        }
    }
    Ok(())
}

fn run_gc(conn: &Connection) -> Result<(), Error> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| Error::Partial(e.to_string()))?;
    db::blobs::gc_orphan_blobs(&tx).map_err(|e| Error::Partial(e.to_string()))?;
    tx.commit().map_err(|e| Error::Partial(e.to_string()))?;
    Ok(())
}

fn print_dry_run(rows: &[(&'static str, u64, u64, u64)]) {
    println!(
        "{:<22} {:>8} {:>10} {:>9}",
        "TYPE", "NEW", "VANISHED", "PRESENT"
    );
    for (ty, new, vanished, present) in rows {
        println!("{ty:<22} {new:>8} {vanished:>10} {present:>9}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init;
    use crate::db::sources::{SourceKey, upsert_source};
    use crate::jmap::wire::JmapId;
    use crate::logging::Logger;

    fn mem() -> (Connection, i64) {
        let c = Connection::open_in_memory().unwrap();
        init::apply_schema(&c).unwrap();
        let sid = upsert_source(
            &c,
            &SourceKey {
                kind: "jmap".to_owned(),
                session_url: "u".to_owned(),
                account_id: "w".to_owned(),
            },
            None,
            "u",
        )
        .unwrap();
        (c, sid)
    }

    #[test]
    fn vanished_tree_deletes_leaf_first_and_nulls_true_orphan() {
        let (c, sid) = mem();
        c.execute(
            "INSERT INTO mailboxes (id,name,parent_id) VALUES (1,'root',NULL)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO mailboxes (id,name,parent_id) VALUES (2,'mid',1)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO mailboxes (id,name,parent_id) VALUES (3,'leaf',2)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO mailboxes (id,name,parent_id) VALUES (4,'survivor',2)",
            [],
        )
        .unwrap();
        for (jid, local) in [("R", 1), ("M", 2), ("L", 3), ("S", 4)] {
            db::ids::insert(&c, sid, ObjectType::Mailbox, jid, local).unwrap();
        }
        let local_map: HashMap<String, i64> = [
            ("R".to_owned(), 1),
            ("M".to_owned(), 2),
            ("L".to_owned(), 3),
        ]
        .into_iter()
        .collect();
        let vanished = vec![
            JmapId("R".to_owned()),
            JmapId("M".to_owned()),
            JmapId("L".to_owned()),
        ];
        delete_vanished(
            &c,
            ObjectType::Mailbox,
            sid,
            &vanished,
            &local_map,
            &Logger::from_flags(true, 0),
        )
        .unwrap();
        let remaining: i64 = c
            .query_row("SELECT count(*) FROM mailboxes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1, "only the surviving child remains");
        let parent: Option<i64> = c
            .query_row("SELECT parent_id FROM mailboxes WHERE id=4", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(parent, None, "true orphan's parent was nulled");
        assert!(
            db::ids::local_for(&c, sid, ObjectType::Mailbox, "L")
                .unwrap()
                .is_none(),
            "vanished sync_id rows removed"
        );
    }

    #[test]
    fn vanished_mailbox_is_pruned_from_email_mailbox_ids() {
        let (c, sid) = mem();
        c.execute(
            "INSERT INTO mailboxes (id,name,parent_id) VALUES (1,'a',NULL)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO mailboxes (id,name,parent_id) VALUES (2,'b',NULL)",
            [],
        )
        .unwrap();
        let blob = db::blobs::intern_blob(&c, b"msg").unwrap();
        c.execute(
            "INSERT INTO emails (blob_id,received_at,mailbox_ids,keywords)
             VALUES (?1,'2020-01-01T00:00:00Z','[1,2]','[]')",
            rusqlite::params![blob],
        )
        .unwrap();
        db::ids::insert(&c, sid, ObjectType::Mailbox, "A", 1).unwrap();
        db::ids::insert(&c, sid, ObjectType::Mailbox, "B", 2).unwrap();
        let local_map: HashMap<String, i64> = [("A".to_owned(), 1)].into_iter().collect();
        delete_vanished(
            &c,
            ObjectType::Mailbox,
            sid,
            &[JmapId("A".to_owned())],
            &local_map,
            &Logger::from_flags(true, 0),
        )
        .unwrap();
        let ids: String = c
            .query_row("SELECT mailbox_ids FROM emails", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            ids, "[2]",
            "deleted mailbox id removed from referring email"
        );
    }

    #[test]
    fn order_objects_does_not_warn_when_parent_already_in_archive() {
        let objects = vec![serde_json::json!({"id":"child","parentId":"existing"})];
        let existing: HashSet<String> = ["existing".to_owned()].into_iter().collect();
        let (ordered, warnings) = order_objects(ObjectType::Mailbox, &objects, &existing);
        assert_eq!(ordered, vec![0]);
        assert!(
            warnings.is_empty(),
            "no orphan warning expected: {warnings:?}"
        );
    }

    #[test]
    fn order_objects_warns_on_orphan_parent() {
        let objects = vec![serde_json::json!({"id":"orphan","parentId":"ghost"})];
        let (ordered, warnings) = order_objects(ObjectType::Mailbox, &objects, &HashSet::new());
        assert_eq!(ordered, vec![0]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("ghost"), "msg was: {}", warnings[0]);
    }

    #[test]
    fn order_objects_warns_on_cycle() {
        let objects = vec![
            serde_json::json!({"id":"a","parentId":"b"}),
            serde_json::json!({"id":"b","parentId":"a"}),
        ];
        let (ordered, warnings) = order_objects(ObjectType::Mailbox, &objects, &HashSet::new());
        assert_eq!(ordered.len(), 2);
        assert!(
            warnings.iter().any(|w| w.contains("cycle")),
            "expected a cycle warning, got: {warnings:?}"
        );
    }
}
