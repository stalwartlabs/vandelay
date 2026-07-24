/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;
use std::io::{IsTerminal, Write};

use rusqlite::Connection;
use serde_json::{Map, Value, json};

use crate::db;
use crate::error::Error;
use crate::jmap::blobxfer;
use crate::jmap::connect::{self, Connected};
use crate::jmap::error::JmapError;
use crate::jmap::http::HttpClient;
use crate::jmap::request::{Request, SetRequest, get_all, get_objects, query_all_ids, set_call};
use crate::jmap::session::{Limits, Session};
use crate::jmap::wire::JmapId;
use crate::logging::{LEVEL_DEFAULT, Logger};
use crate::sync::import_jmap::mapping::{BlobUpload, TargetResolver};
use crate::sync::{CommonConfig, Context, ExportConfig, Summary, TypeCounts};
use crate::types::ObjectType;

const EXPORT_ORDER: [ObjectType; 10] = [
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

type IdMap = HashMap<i64, JmapId>;

#[derive(Default)]
struct Maps {
    m: HashMap<ObjectType, IdMap>,
}

impl Maps {
    fn insert(&mut self, ty: ObjectType, local: i64, target: JmapId) {
        self.m.entry(ty).or_default().insert(local, target);
    }
}

impl TargetResolver for Maps {
    fn target(&self, ty: ObjectType, local_id: i64) -> Option<JmapId> {
        self.m.get(&ty)?.get(&local_id).cloned()
    }
}

struct Uploader<'a> {
    net: &'a Net,
    conn: &'a Connection,
    cache: HashMap<i64, JmapId>,
    touched: Vec<i64>,
}

impl<'a> Uploader<'a> {
    fn new(net: &'a Net, conn: &'a Connection) -> Uploader<'a> {
        Uploader {
            net,
            conn,
            cache: HashMap::new(),
            touched: Vec::new(),
        }
    }

    fn upload_with(&mut self, local_id: i64, content_type: &str) -> Result<JmapId, JmapError> {
        self.touched.push(local_id);
        if let Some(id) = self.cache.get(&local_id) {
            return Ok(id.clone());
        }
        let id = if self.net.dry_run {
            let _exists = db::blobs::blob_bytes(self.conn, local_id)?
                .ok_or_else(|| JmapError::malformed(format!("blob local id {local_id} missing")))?;
            JmapId(format!("dryrun-blob-{local_id}"))
        } else {
            let bytes = db::blobs::blob_bytes(self.conn, local_id)?
                .ok_or_else(|| JmapError::malformed(format!("blob local id {local_id} missing")))?;
            blobxfer::upload_bytes(
                &self.net.client,
                &self.net.session,
                &self.net.account,
                content_type,
                &bytes,
            )?
        };
        self.cache.insert(local_id, id.clone());
        Ok(id)
    }

    fn invalidate(&mut self, local_id: i64) {
        self.cache.remove(&local_id);
    }

    fn take_touched(&mut self) -> Vec<i64> {
        std::mem::take(&mut self.touched)
    }
}

impl BlobUpload for Uploader<'_> {
    fn upload(&mut self, local_id: i64) -> Result<JmapId, JmapError> {
        self.upload_with(local_id, "application/octet-stream")
    }
}

#[derive(Clone)]
struct Net {
    client: HttpClient,
    api: String,
    account: String,
    limits: Limits,
    session: Session,
    dry_run: bool,
}

fn count_rows(conn: &Connection, ty: ObjectType) -> Option<u64> {
    let table = crate::sync::table_name(ty);
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| {
        r.get::<_, i64>(0)
    })
    .ok()
    .map(|n| n as u64)
}

fn has_rows(conn: &Connection, ty: ObjectType) -> bool {
    let table = crate::sync::table_name(ty);
    conn.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table})"), [], |r| {
        r.get::<_, i64>(0)
    })
    .map(|n| n != 0)
    .unwrap_or(false)
}

pub fn run(common: CommonConfig, config: ExportConfig) -> Result<Summary, Error> {
    let logger = common.logger;
    let ctx = Context::open(common, &config.connect)?;
    let connected = connect::prepare(&ctx, &config.connect)?;

    let net = Net {
        client: ctx.client.clone(),
        api: connected.session.api_url.clone(),
        account: connected.account_id.clone(),
        limits: connected.limits,
        session: connected.session.clone(),
        dry_run: ctx.dry_run(),
    };

    let work = work_list(&ctx.conn, &config, &connected, &logger);
    let mut maps = Maps::default();
    let mut summary = Summary::default();
    let mut dry_rows: Vec<(&'static str, u64, u64, u64)> = Vec::new();
    let mut plans: HashMap<ObjectType, Plan> = HashMap::new();
    let mut counts_per_type: HashMap<ObjectType, TypeCounts> = HashMap::new();

    for ty in &work {
        if logger.enabled(LEVEL_DEFAULT) {
            eprintln!("export: {} ...", ty.jmap_name());
        }
        let mut counts = TypeCounts::default();
        crate::progress::start(ty.jmap_name(), count_rows(&ctx.conn, *ty));
        let res = reconcile_type(
            &ctx,
            &net,
            *ty,
            &mut maps,
            &logger,
            &mut counts,
            &mut dry_rows,
        );
        let plan = match res {
            Ok(p) => p,
            Err(e) => {
                logger.warn(&format!("type {} aborted: {e}", ty.jmap_name()));
                counts.failed += 1;
                Plan::default()
            }
        };
        crate::progress::finish();
        plans.insert(*ty, plan);
        counts_per_type.insert(*ty, counts);
    }

    if config.prune {
        prune_phase(
            &ctx,
            &net,
            &work,
            &plans,
            &config,
            &logger,
            &mut counts_per_type,
        )?;
    }

    for ty in &work {
        if let Some(counts) = counts_per_type.remove(ty) {
            summary.per_type.push((ty.jmap_name(), counts));
        }
    }

    if ctx.dry_run() {
        print_dry_run(&dry_rows, config.prune);
        return Ok(Summary::default());
    }
    summary.retries_observed = ctx.client.retries_observed();
    summary.retry_after_sleeps = ctx.client.retry_after_sleeps();
    Ok(summary)
}

fn prune_phase(
    ctx: &Context,
    net: &Net,
    work: &[ObjectType],
    plans: &HashMap<ObjectType, Plan>,
    config: &ExportConfig,
    logger: &Logger,
    counts_per_type: &mut HashMap<ObjectType, TypeCounts>,
) -> Result<(), Error> {
    let totals: Vec<(ObjectType, &Plan)> = work
        .iter()
        .filter_map(|ty| plans.get(ty).map(|p| (*ty, p)))
        .filter(|(_, p)| !p.prune_candidates.is_empty())
        .collect();
    if totals.is_empty() {
        return Ok(());
    }
    eprintln!("prune plan:");
    let total: usize = totals.iter().map(|(_, p)| p.prune_candidates.len()).sum();
    for (ty, p) in &totals {
        eprintln!(
            "  {:<22} {:>6} candidate(s); sample: {}",
            ty.jmap_name(),
            p.prune_candidates.len(),
            sample(&p.prune_candidates),
        );
    }
    eprintln!("  {:<22} {:>6} total", "(all types)", total);
    if ctx.dry_run() {
        return Ok(());
    }
    if !config.yes && std::io::stdin().is_terminal() {
        eprint!("destroy all {total} objects across all types? [y/N] ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| Error::Partial(e.to_string()))?;
        if !matches!(line.trim(), "y" | "Y" | "yes") {
            return Err(Error::PruneAborted);
        }
    }
    for ty in work.iter().rev() {
        if let Some(plan) = plans.get(ty)
            && !plan.prune_candidates.is_empty()
            && let Some(counts) = counts_per_type.get_mut(ty)
        {
            do_destroy(net, *ty, plan, logger, counts);
        }
    }
    Ok(())
}

fn work_list(
    conn: &Connection,
    config: &ExportConfig,
    connected: &Connected,
    logger: &Logger,
) -> Vec<ObjectType> {
    let selected = config.objects.as_ref();
    EXPORT_ORDER
        .into_iter()
        .filter(|ty| selected.map(|s| s.contains(ty)).unwrap_or(true))
        .filter(|ty| has_rows(conn, *ty))
        .filter(|ty| {
            if connected.supports(*ty) {
                true
            } else {
                logger.warn(&format!(
                    "target does not support {}; skipping",
                    ty.jmap_name()
                ));
                false
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn reconcile_type(
    ctx: &Context,
    net: &Net,
    ty: ObjectType,
    maps: &mut Maps,
    logger: &Logger,
    counts: &mut TypeCounts,
    dry_rows: &mut Vec<(&'static str, u64, u64, u64)>,
) -> Result<Plan, Error> {
    let plan = match ty {
        ObjectType::Mailbox | ObjectType::FileNode => {
            tree::reconcile(ctx, net, ty, maps, counts, logger)
        }
        ObjectType::AddressBook | ObjectType::Calendar => {
            flat::reconcile(ctx, net, ty, maps, counts, logger)
        }
        ObjectType::Identity => keyed::reconcile_identity(ctx, net, maps, counts, logger),
        ObjectType::ParticipantIdentity => {
            keyed::reconcile_participant(ctx, net, maps, counts, logger)
        }
        ObjectType::SieveScript => sieve::reconcile(ctx, net, maps, counts, logger),
        ObjectType::ContactCard | ObjectType::CalendarEvent => {
            uidtype::reconcile(ctx, net, ty, maps, counts, logger)
        }
        ObjectType::Email => email::reconcile(ctx, net, maps, counts, logger),
    }?;

    if ctx.dry_run() {
        dry_rows.push((
            ty.jmap_name(),
            counts.created,
            counts.skipped,
            plan.prune_candidates.len() as u64,
        ));
    }

    Ok(plan)
}

#[derive(Default)]
pub struct Plan {
    pub prune_candidates: Vec<String>,
    pub active_sieve_target: Option<String>,
}

fn do_destroy(net: &Net, ty: ObjectType, plan: &Plan, logger: &Logger, counts: &mut TypeCounts) {
    if ty == ObjectType::SieveScript {
        deactivate_active_sieve_script(net, logger);
    }
    let destroy = Value::Array(
        plan.prune_candidates
            .iter()
            .map(|s| Value::String(s.clone()))
            .collect(),
    );
    let extra = destroy_contents_arg(ty);
    match set_call(
        &net.client,
        &net.api,
        &net.account,
        ty.jmap_name(),
        SetRequest {
            destroy: Some(destroy),
            extra_args: &extra,
            ..Default::default()
        },
        &net.limits,
    ) {
        Ok(outcome) => {
            counts.deleted += outcome.destroyed.len() as u64;
            for (id, err) in &outcome.not_destroyed {
                logger.warn(&format!(
                    "prune: {} {id} not destroyed: {err}",
                    ty.jmap_name()
                ));
                counts.skipped += 1;
            }
        }
        Err(e) => {
            logger.warn(&format!(
                "prune {}: destroy request failed: {e}",
                ty.jmap_name()
            ));
            counts.failed += plan.prune_candidates.len() as u64;
        }
    }
}

fn deactivate_active_sieve_script(net: &Net, logger: &Logger) {
    let mut req = Request::new();
    req.call(
        "SieveScript/set",
        json!({ "accountId": net.account, "onSuccessDeactivateScript": true }),
        "d",
    );
    let outcome = req.send(&net.client, &net.api).and_then(|resp| {
        let mr = resp.first()?;
        crate::jmap::request::check_method_error(mr)
    });
    if let Err(e) = outcome {
        logger.warn(&format!(
            "prune: SieveScript deactivation failed before destroy: {e}"
        ));
    }
}

fn destroy_contents_arg(ty: ObjectType) -> Vec<(&'static str, Value)> {
    match ty {
        ObjectType::AddressBook => vec![("onDestroyRemoveContents", Value::Bool(false))],
        ObjectType::Calendar => vec![("onDestroyRemoveEvents", Value::Bool(false))],
        ObjectType::FileNode => vec![("onDestroyRemoveChildren", Value::Bool(false))],
        _ => Vec::new(),
    }
}

fn sample(ids: &[String]) -> String {
    let n = ids.len().min(5);
    ids[..n].join(", ")
}

fn print_dry_run(rows: &[(&'static str, u64, u64, u64)], prune: bool) {
    if prune {
        println!(
            "{:<22} {:>10} {:>10} {:>12}",
            "TYPE", "CREATE", "MATCHED", "WOULD-DESTROY"
        );
        for (ty, c, m, d) in rows {
            println!("{ty:<22} {c:>10} {m:>10} {d:>12}");
        }
    } else {
        println!("{:<22} {:>10} {:>10}", "TYPE", "CREATE", "MATCHED");
        for (ty, c, m, _) in rows {
            println!("{ty:<22} {c:>10} {m:>10}");
        }
    }
}

mod tree;

mod flat;

mod keyed;

mod sieve;

mod uidtype;

mod email;

mod common {
    use super::*;

    pub fn target_query_get(
        net: &Net,
        ty: ObjectType,
        props: Option<&[&str]>,
    ) -> Result<Vec<Value>, JmapError> {
        let ids = query_all_ids(
            &net.client,
            &net.api,
            &net.account,
            ty.jmap_name(),
            &net.limits,
        )?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let got = get_objects::<Value>(
            &net.client,
            &net.api,
            &net.account,
            ty.jmap_name(),
            &ids,
            props,
            &net.limits,
        )?;
        Ok(got.list)
    }

    pub fn target_get_all(net: &Net, ty: ObjectType) -> Result<Vec<Value>, JmapError> {
        Ok(get_all::<Value>(&net.client, &net.api, &net.account, ty.jmap_name())?.list)
    }

    pub fn jid(v: &Value) -> Option<String> {
        v.get("id").and_then(Value::as_str).map(str::to_owned)
    }

    pub fn create_batch(
        net: &Net,
        ty: ObjectType,
        creates: Vec<(String, Value)>,
    ) -> Result<crate::jmap::request::SetOutcome, JmapError> {
        if net.dry_run {
            return Ok(synthesize_dry_run_outcome(ty, &creates));
        }
        let mut map = Map::new();
        for (cid, obj) in creates {
            map.insert(cid, obj);
        }
        set_call(
            &net.client,
            &net.api,
            &net.account,
            ty.jmap_name(),
            SetRequest {
                create: Some(Value::Object(map)),
                ..Default::default()
            },
            &net.limits,
        )
    }

    fn blob_not_found(outcome: &crate::jmap::request::SetOutcome, cid: &str) -> bool {
        outcome.not_created.iter().any(|(c, err)| {
            c == cid && err.get("type").and_then(Value::as_str) == Some("blobNotFound")
        })
    }

    pub fn retry_if_blob_missing<F>(
        net: &Net,
        ty: ObjectType,
        cid: &str,
        uploader: &mut Uploader<'_>,
        touched: Vec<i64>,
        outcome: crate::jmap::request::SetOutcome,
        mut rebuild: F,
    ) -> Result<crate::jmap::request::SetOutcome, Error>
    where
        F: FnMut(&mut Uploader<'_>) -> Result<Value, Error>,
    {
        if !blob_not_found(&outcome, cid) {
            return Ok(outcome);
        }
        for id in &touched {
            uploader.invalidate(*id);
        }
        let _ = uploader.take_touched();
        let wire = rebuild(uploader)?;
        let _ = uploader.take_touched();
        create_batch(net, ty, vec![(cid.to_owned(), wire)]).map_err(Error::from)
    }

    fn synthesize_dry_run_outcome(
        ty: ObjectType,
        creates: &[(String, Value)],
    ) -> crate::jmap::request::SetOutcome {
        let mut outcome = crate::jmap::request::SetOutcome::default();
        for (cid, _) in creates {
            let synthetic = serde_json::json!({
                "id": format!("dryrun-{}-{cid}", ty.jmap_name())
            });
            outcome.created.push((cid.clone(), synthetic));
        }
        outcome
    }
}
