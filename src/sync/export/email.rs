/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};

use super::common::{jid, target_query_get};
use super::{Maps, Net, Plan};
use crate::db;
use crate::error::Error;
use crate::jmap::blobxfer;
use crate::jmap::error::JmapError;
use crate::jmap::request::{Request, check_method_error, get_objects};
use crate::jmap::session::Limits;
use crate::jmap::wire::JmapId;
use crate::logging::Logger;
use crate::sync::import_jmap::mapping::{EMAIL_SELECT, EmailRow, TargetResolver, row_to_email};
use crate::sync::import_jmap::pool::{Pool, effective_workers};
use crate::sync::keys::{EmailIndex, EmailKey, email_index, email_keys, index_from_json};
use crate::sync::{Context, TypeCounts};
use crate::types::ObjectType;

fn server_index(v: &Value) -> EmailIndex {
    let arr = |k: &str| {
        v.get(k)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.get("email").and_then(Value::as_str).map(str::to_owned))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let mids: Vec<String> = v
        .get("messageId")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    email_index(
        &mids,
        &arr("from"),
        v.get("subject").and_then(Value::as_str).unwrap_or(""),
        v.get("sentAt").and_then(Value::as_str).unwrap_or(""),
        &arr("to"),
    )
}

fn import_workers(threads: usize, limits: &Limits) -> usize {
    effective_workers(threads, limits, true).min(effective_workers(threads, limits, false))
}

struct ImportJob {
    cid: String,
    blob_local_id: i64,
    bytes: Vec<u8>,
    mids: Map<String, Value>,
    keywords: Map<String, Value>,
    received_at: String,
    hint: String,
}

struct ImportResult {
    cid: String,
    hint: String,
    outcome: Result<SingleImport, JmapError>,
}

type BlobCache = Mutex<HashMap<i64, JmapId>>;

pub fn reconcile(
    ctx: &Context,
    net: &Net,
    maps: &mut Maps,
    counts: &mut TypeCounts,
    logger: &Logger,
) -> Result<Plan, Error> {
    let ty = ObjectType::Email;

    let target_min = target_query_get(net, ty, Some(&["messageId"])).map_err(Error::from)?;
    let mut indices: Vec<EmailIndex> = target_min.iter().map(server_index).collect();

    let fallback_ids: Vec<JmapId> = target_min
        .iter()
        .zip(indices.iter())
        .filter(|(_, i)| i.mids.is_empty())
        .filter_map(|(v, _)| jid(v).map(JmapId))
        .collect();
    if !fallback_ids.is_empty() {
        let got = get_objects::<Value>(
            &net.client,
            &net.api,
            &net.account,
            ty.jmap_name(),
            &fallback_ids,
            Some(&["messageId", "from", "subject", "sentAt", "to"]),
            &net.limits,
        )
        .map_err(Error::from)?;
        let by_id: HashMap<String, &Value> = got
            .list
            .iter()
            .filter_map(|v| jid(v).map(|i| (i, v)))
            .collect();
        for (v, slot) in target_min.iter().zip(indices.iter_mut()) {
            if let Some(full) = jid(v).and_then(|i| by_id.get(&i)) {
                *slot = server_index(full);
            }
        }
    }
    let target_keys: HashSet<EmailKey> = email_keys(&indices).into_iter().collect();

    let local: Vec<(i64, EmailRow)> = {
        let mut stmt = ctx
            .conn
            .prepare(EMAIL_SELECT)
            .map_err(|e| Error::Partial(e.to_string()))?;
        stmt.query_map([], |row| {
            let id: i64 = row.get(0)?;
            Ok((id, row_to_email(row)))
        })
        .and_then(|m| m.collect::<Result<Vec<_>, _>>())
        .map_err(|e| Error::Partial(e.to_string()))?
        .into_iter()
        .map(|(id, r)| Ok((id, r.map_err(Error::from)?)))
        .collect::<Result<_, Error>>()?
    };

    let local_indices: Vec<EmailIndex> = local
        .iter()
        .map(|(_, r)| index_from_json(&r.message_match))
        .collect();
    let local_keys = email_keys(&local_indices);

    let workers = import_workers(ctx.common.threads, &net.limits);
    let cache: Arc<BlobCache> = Arc::new(Mutex::new(HashMap::new()));
    let pool: Pool<ImportJob, ImportResult> = Pool::new(workers, {
        let net = Arc::new(net.clone());
        let cache = cache.clone();
        move |job: ImportJob| {
            let outcome = run_import(&net, &cache, &job);
            ImportResult {
                cid: job.cid,
                hint: job.hint,
                outcome,
            }
        }
    });
    let window = workers * 2;
    let mut in_flight = 0usize;

    for (i, key) in local_keys.iter().enumerate() {
        if target_keys.contains(key) {
            counts.skipped += 1;
            crate::progress::advance(1);
            continue;
        }
        let (local_id, row) = &local[i];
        let job = match prepare_job(ctx, maps, *local_id, row, counts, logger) {
            Some(j) => j,
            None => {
                crate::progress::advance(1);
                continue;
            }
        };
        if net.dry_run {
            counts.created += 1;
            crate::progress::advance(1);
            continue;
        }
        pool.submit(job);
        in_flight += 1;
        if in_flight >= window
            && let Ok(res) = pool.results().recv()
        {
            in_flight -= 1;
            account(res, counts, logger);
        }
    }
    for res in pool.finish() {
        account(res, counts, logger);
    }

    Ok(Plan::default())
}

fn prepare_job(
    ctx: &Context,
    maps: &Maps,
    local_id: i64,
    row: &EmailRow,
    counts: &mut TypeCounts,
    logger: &Logger,
) -> Option<ImportJob> {
    let cid = format!("e{local_id}");
    let mids = match build_mailbox_ids(row, maps) {
        Some(m) => m,
        None => {
            logger.warn(&format!(
                "Email/import {cid} ({}) skipped: mailbox not on target",
                blob_hint(row, None)
            ));
            counts.failed += 1;
            return None;
        }
    };
    let bytes = match db::blobs::blob_bytes(&ctx.conn, row.blob_local_id) {
        Ok(Some(b)) => b,
        Ok(None) => {
            logger.warn(&format!(
                "Email/import {cid} ({}) blob upload failed: blob local id {} missing",
                blob_hint(row, None),
                row.blob_local_id
            ));
            counts.failed += 1;
            return None;
        }
        Err(e) => {
            logger.warn(&format!(
                "Email/import {cid} ({}) blob read failed: {e}",
                blob_hint(row, None)
            ));
            counts.failed += 1;
            return None;
        }
    };
    let hint = blob_hint(row, Some(bytes.len() as u64));
    Some(ImportJob {
        cid,
        blob_local_id: row.blob_local_id,
        bytes,
        mids,
        keywords: build_keywords(row),
        received_at: row.received_at.clone(),
        hint,
    })
}

fn run_import(net: &Net, cache: &BlobCache, job: &ImportJob) -> Result<SingleImport, JmapError> {
    let blob = upload_cached(net, cache, job.blob_local_id, &job.bytes)?;
    let item = import_item(
        blob.0.clone(),
        job.mids.clone(),
        job.keywords.clone(),
        &job.received_at,
    );
    match send_single_import(net, &job.cid, item)? {
        SingleImport::NotCreated { ref error_type, .. } if error_type == "blobNotFound" => {
            invalidate(cache, job.blob_local_id, &blob);
            let blob = upload_cached(net, cache, job.blob_local_id, &job.bytes)?;
            let item = import_item(
                blob.0,
                job.mids.clone(),
                job.keywords.clone(),
                &job.received_at,
            );
            send_single_import(net, &job.cid, item)
        }
        other => Ok(other),
    }
}

fn upload_cached(
    net: &Net,
    cache: &BlobCache,
    local_id: i64,
    bytes: &[u8],
) -> Result<JmapId, JmapError> {
    if let Some(id) = cache.lock().unwrap().get(&local_id) {
        return Ok(id.clone());
    }
    let id = blobxfer::upload_bytes(
        &net.client,
        &net.session,
        &net.account,
        "message/rfc822",
        bytes,
    )?;
    cache.lock().unwrap().insert(local_id, id.clone());
    Ok(id)
}

fn invalidate(cache: &BlobCache, local_id: i64, stale: &JmapId) {
    let mut c = cache.lock().unwrap();
    if c.get(&local_id) == Some(stale) {
        c.remove(&local_id);
    }
}

fn account(res: ImportResult, counts: &mut TypeCounts, logger: &Logger) {
    crate::progress::advance(1);
    match res.outcome {
        Ok(SingleImport::Created) => counts.created += 1,
        Ok(SingleImport::Skipped) => counts.skipped += 1,
        Ok(SingleImport::NotCreated { detail, .. }) => {
            logger.warn(&format!(
                "Email/import {} ({}) failed: {detail}",
                res.cid, res.hint
            ));
            counts.failed += 1;
        }
        Err(e) => {
            logger.warn(&format!(
                "Email/import {} ({}) send failed: {e}{}",
                res.cid,
                res.hint,
                size_note(&e)
            ));
            counts.failed += 1;
        }
    }
}

fn build_mailbox_ids(row: &EmailRow, maps: &Maps) -> Option<Map<String, Value>> {
    let mut mids = Map::new();
    for ml in &row.mailbox_locals {
        let t = maps.target(ObjectType::Mailbox, *ml)?;
        mids.insert(t.0, Value::Bool(true));
    }
    Some(mids)
}

fn build_keywords(row: &EmailRow) -> Map<String, Value> {
    let mut kw = Map::new();
    for k in &row.keywords {
        kw.insert(k.clone(), Value::Bool(true));
    }
    kw
}

fn blob_hint(row: &EmailRow, len: Option<u64>) -> String {
    let idx = index_from_json(&row.message_match);
    let mut s = match idx.mids.first() {
        Some(mid) => format!("message-id <{mid}>"),
        None => "no message-id".to_owned(),
    };
    if let Some(len) = len {
        use std::fmt::Write;
        let _ = write!(s, ", {}", crate::inspect::format_bytes(len));
    }
    s
}

fn size_note(e: &JmapError) -> &'static str {
    if matches!(
        e,
        JmapError::RequestTooLarge | JmapError::SingleObjectTooLarge(_)
    ) {
        "; exceeds the target server size limit, so this message is skipped and re-running will not migrate it"
    } else {
        ""
    }
}

fn import_item(
    blob: String,
    mids: Map<String, Value>,
    kw: Map<String, Value>,
    received_at: &str,
) -> Value {
    json!({
        "blobId": blob,
        "mailboxIds": Value::Object(mids),
        "keywords": Value::Object(kw),
        "receivedAt": received_at,
    })
}

enum SingleImport {
    Created,
    Skipped,
    NotCreated { error_type: String, detail: String },
}

fn send_single_import(net: &Net, cid: &str, item: Value) -> Result<SingleImport, JmapError> {
    let mut emails = Map::new();
    emails.insert(cid.to_owned(), item);
    let mut req = Request::new();
    req.call(
        "Email/import",
        json!({ "accountId": net.account, "emails": Value::Object(emails) }),
        "i",
    );
    req.fits(&net.limits)?;
    let resp = req.send(&net.client, &net.api)?;
    let mr = resp.first()?;
    check_method_error(mr)?;
    if let Some(err) = mr
        .args
        .get("notCreated")
        .and_then(Value::as_object)
        .and_then(|nc| nc.get(cid))
    {
        let error_type = err
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if error_type == "alreadyExists" {
            return Ok(SingleImport::Skipped);
        }
        return Ok(SingleImport::NotCreated {
            error_type,
            detail: err.to_string(),
        });
    }
    if mr
        .args
        .get("created")
        .and_then(Value::as_object)
        .is_some_and(|c| !c.is_empty())
    {
        return Ok(SingleImport::Created);
    }
    Ok(SingleImport::NotCreated {
        error_type: String::new(),
        detail: format!("Email/import returned neither created nor notCreated for {cid}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_workers_capped_by_both_request_and_upload_limits() {
        let limits = Limits {
            max_objects_in_get: 500,
            max_objects_in_set: 500,
            max_calls_in_request: 16,
            max_concurrent_requests: 8,
            max_concurrent_upload: 2,
            max_size_request: 10_000_000,
            max_size_upload: 50_000_000,
        };
        assert_eq!(import_workers(16, &limits), 2);
        assert_eq!(import_workers(1, &limits), 1);
        assert_eq!(import_workers(0, &limits), 1);
    }
}
