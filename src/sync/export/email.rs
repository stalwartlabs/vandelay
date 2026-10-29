/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

use super::blobpool::UploadPool;
use super::common::{jid, target_query_get};
use super::{Maps, Net, Plan, Uploader};
use crate::error::Error;
use crate::jmap::error::JmapError;
use crate::jmap::request::{Request, check_method_error, get_objects, retry_method_call};
use crate::jmap::retry::MethodCallKind;
use crate::jmap::session::Limits;
use crate::jmap::wire::JmapId;
use crate::logging::Logger;
use crate::sync::import_jmap::mapping::{EMAIL_SELECT, EmailRow, TargetResolver, row_to_email};
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

    let mut uploader = Uploader::new(net, &ctx.conn);
    let pool = (!net.dry_run).then(|| UploadPool::new(net, net.threads, EMAIL_MIME));
    let unmatched: Vec<&(i64, EmailRow)> = local_keys
        .iter()
        .zip(local.iter())
        .filter_map(|(key, entry)| {
            if target_keys.contains(key) {
                counts.skipped += 1;
                None
            } else {
                Some(entry)
            }
        })
        .collect();

    let mut exporter = Exporter {
        net,
        uploader: &mut uploader,
        pool: pool.as_ref(),
        maps,
        counts,
        logger,
        batch: import_batch_size(&net.limits),
    };
    let mut rest = unmatched.as_slice();
    while !rest.is_empty() {
        let (chunk, tail) = rest.split_at(exporter.batch.min(rest.len()));
        exporter.export_chunk(chunk)?;
        rest = tail;
    }

    Ok(Plan::default())
}

const EMAIL_MIME: &str = "message/rfc822";

pub fn import_batch_size(limits: &Limits) -> usize {
    usize::try_from(limits.max_objects_in_set.max(1)).unwrap_or(usize::MAX)
}

pub fn reupload_batch_size(previous: usize) -> usize {
    (previous / 2).max(1)
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

fn blob_hint(uploader: &Uploader, row: &EmailRow) -> String {
    let idx = index_from_json(&row.message_match);
    let mut s = match idx.mids.first() {
        Some(mid) => format!("message-id <{mid}>"),
        None => "no message-id".to_owned(),
    };
    if let Some(len) = uploader.blob_len(row.blob_local_id) {
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

struct Pending<'r> {
    cid: String,
    row: &'r EmailRow,
    mids: Map<String, Value>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Attempt {
    First,
    Reupload,
    LastReupload,
}

impl Attempt {
    fn suffix(self) -> &'static str {
        match self {
            Attempt::First => "",
            Attempt::Reupload | Attempt::LastReupload => " after blob re-upload",
        }
    }

    fn upload_verb(self) -> &'static str {
        match self {
            Attempt::First => "blob upload",
            Attempt::Reupload | Attempt::LastReupload => "blob re-upload",
        }
    }
}

struct Exporter<'a, 'u> {
    net: &'a Net,
    uploader: &'a mut Uploader<'u>,
    pool: Option<&'a UploadPool>,
    maps: &'a Maps,
    counts: &'a mut TypeCounts,
    logger: &'a Logger,
    batch: usize,
}

impl<'r> Exporter<'_, '_> {
    fn export_chunk(&mut self, chunk: &[&'r (i64, EmailRow)]) -> Result<(), Error> {
        let mut pending: Vec<Pending<'r>> = Vec::with_capacity(chunk.len());
        for (local_id, row) in chunk.iter().copied() {
            let cid = format!("e{local_id}");
            match build_mailbox_ids(row, self.maps) {
                Some(mids) => pending.push(Pending { cid, row, mids }),
                None => {
                    self.logger.warn(&format!(
                        "Email/import {cid} ({}) skipped: mailbox not on target",
                        blob_hint(self.uploader, row)
                    ));
                    self.counts.failed += 1;
                }
            }
        }
        let mut size = pending.len();
        let mut retry = self.import(pending, Attempt::First)?;
        if !retry.is_empty() {
            self.batch = reupload_batch_size(self.batch);
        }
        while !retry.is_empty() {
            size = reupload_batch_size(size);
            let attempt = if size == 1 {
                Attempt::LastReupload
            } else {
                Attempt::Reupload
            };
            for p in &retry {
                self.uploader.invalidate(p.row.blob_local_id);
            }
            let mut again = Vec::new();
            while !retry.is_empty() {
                let tail = retry.split_off(size.min(retry.len()));
                let head = std::mem::replace(&mut retry, tail);
                again.extend(self.import(head, attempt)?);
            }
            retry = again;
        }
        Ok(())
    }

    fn import(
        &mut self,
        pending: Vec<Pending<'r>>,
        attempt: Attempt,
    ) -> Result<Vec<Pending<'r>>, Error> {
        let mut failed = self.upload(&pending);
        let mut ready: Vec<Pending<'r>> = Vec::with_capacity(pending.len());
        let mut items: Vec<(String, Value)> = Vec::with_capacity(pending.len());
        for p in pending {
            let blob = match failed.remove(&p.row.blob_local_id) {
                Some(e) => Err(e),
                None => self
                    .uploader
                    .cached(p.row.blob_local_id)
                    .map(|b| b.0.clone())
                    .ok_or_else(|| JmapError::malformed("blob upload did not complete")),
            };
            let blob = match blob {
                Ok(b) => b,
                Err(e) => {
                    self.logger.warn(&format!(
                        "Email/import {} ({}) {} failed: {e}{}",
                        p.cid,
                        blob_hint(self.uploader, p.row),
                        attempt.upload_verb(),
                        size_note(&e)
                    ));
                    self.counts.failed += 1;
                    continue;
                }
            };
            if self.net.dry_run {
                self.counts.created += 1;
                continue;
            }
            let item = import_item(
                blob,
                p.mids.clone(),
                build_keywords(p.row),
                &p.row.received_at,
            );
            items.push((p.cid.clone(), item));
            ready.push(p);
        }
        if items.is_empty() {
            return Ok(Vec::new());
        }

        let mut results = Vec::with_capacity(items.len());
        if let Err(e) = send_import(self.net, &items, self.logger, &mut results) {
            let mapped = Error::from(e);
            if mapped.aborts_run() {
                return Err(mapped);
            }
            for p in ready.iter().skip(results.len()) {
                self.logger.warn(&format!(
                    "Email/import {} ({}) send failed{}: {mapped}",
                    p.cid,
                    blob_hint(self.uploader, p.row),
                    attempt.suffix()
                ));
                self.counts.failed += 1;
            }
            ready.truncate(results.len());
        }

        let mut retry = Vec::new();
        for (p, res) in ready.into_iter().zip(results) {
            match res {
                Ok(SingleImport::Created) => self.counts.created += 1,
                Ok(SingleImport::Skipped) => self.counts.skipped += 1,
                Ok(SingleImport::NotCreated { error_type, .. })
                    if attempt != Attempt::LastReupload && error_type == "blobNotFound" =>
                {
                    retry.push(p);
                }
                Ok(SingleImport::NotCreated { detail, .. }) => {
                    self.logger.warn(&format!(
                        "Email/import {} ({}) failed{}: {detail}",
                        p.cid,
                        blob_hint(self.uploader, p.row),
                        attempt.suffix()
                    ));
                    self.counts.failed += 1;
                }
                Err(e) => {
                    self.logger.warn(&format!(
                        "Email/import {} ({}) send failed{}: {e}{}",
                        p.cid,
                        blob_hint(self.uploader, p.row),
                        attempt.suffix(),
                        size_note(&e)
                    ));
                    self.counts.failed += 1;
                }
            }
        }
        Ok(retry)
    }

    fn upload(&mut self, pending: &[Pending<'_>]) -> HashMap<i64, JmapError> {
        let Some(pool) = self.pool else {
            let mut failed = HashMap::new();
            for p in pending {
                if let Err(e) = self.uploader.upload_with(p.row.blob_local_id, EMAIL_MIME) {
                    failed.insert(p.row.blob_local_id, e);
                }
            }
            return failed;
        };
        let mut seen: HashSet<i64> = HashSet::with_capacity(pending.len());
        let mut queue: Vec<i64> = Vec::with_capacity(pending.len());
        for p in pending {
            let local_id = p.row.blob_local_id;
            self.uploader.touch(local_id);
            if self.uploader.cached(local_id).is_none() && seen.insert(local_id) {
                queue.push(local_id);
            }
        }
        pool.upload(self.uploader, &queue)
    }
}

enum SingleImport {
    Created,
    Skipped,
    NotCreated { error_type: String, detail: String },
}

fn import_request(net: &Net, items: &[(String, Value)]) -> Request {
    let emails: Map<String, Value> = items.iter().cloned().collect();
    let mut req = Request::new();
    req.call(
        "Email/import",
        json!({ "accountId": net.account, "emails": Value::Object(emails) }),
        "i",
    );
    req
}

fn send_import(
    net: &Net,
    items: &[(String, Value)],
    logger: &Logger,
    out: &mut Vec<Result<SingleImport, JmapError>>,
) -> Result<(), JmapError> {
    let req = import_request(net, items);
    let sent = match req.fits(&net.limits) {
        Ok(()) => retry_method_call(
            &net.client,
            MethodCallKind::SingleObjectWrite,
            logger,
            || {
                let resp = req.send(&net.client, &net.api)?;
                let mr = resp.first()?;
                check_method_error(mr)?;
                Ok(items
                    .iter()
                    .map(|(cid, _)| item_outcome(&mr.args, cid))
                    .collect::<Vec<_>>())
            },
        ),
        Err(e) => Err(e),
    };
    match sent {
        Ok(outcomes) => {
            out.extend(outcomes.into_iter().map(Ok));
            Ok(())
        }
        Err(JmapError::RequestTooLarge) if items.len() > 1 => {
            let (left, right) = items.split_at(items.len() / 2);
            send_import(net, left, logger, out)?;
            send_import(net, right, logger, out)
        }
        Err(e @ JmapError::RequestTooLarge) => {
            out.push(Err(e));
            Ok(())
        }
        Err(e) => Err(e),
    }
}

fn item_outcome(args: &Value, cid: &str) -> SingleImport {
    if let Some(err) = args
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
            return SingleImport::Skipped;
        }
        return SingleImport::NotCreated {
            error_type,
            detail: err.to_string(),
        };
    }
    if args
        .get("created")
        .and_then(Value::as_object)
        .is_some_and(|c| c.contains_key(cid))
    {
        return SingleImport::Created;
    }
    SingleImport::NotCreated {
        error_type: String::new(),
        detail: format!("Email/import returned neither created nor notCreated for {cid}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(set: u64) -> Limits {
        Limits {
            max_objects_in_get: 500,
            max_objects_in_set: set,
            max_calls_in_request: 16,
            max_concurrent_requests: 4,
            max_concurrent_upload: 4,
            max_size_request: 10_000_000,
            max_size_upload: 50_000_000,
        }
    }

    #[test]
    fn batch_size_follows_max_objects_in_set() {
        assert_eq!(import_batch_size(&limits(500)), 500);
        assert_eq!(import_batch_size(&limits(7)), 7);
        assert_eq!(import_batch_size(&limits(0)), 1);
    }

    #[test]
    fn reupload_batches_halve_down_to_one() {
        let mut size = 500;
        let mut steps = Vec::new();
        while size > 1 {
            size = reupload_batch_size(size);
            steps.push(size);
        }
        assert_eq!(steps, vec![250, 125, 62, 31, 15, 7, 3, 1]);
        assert_eq!(reupload_batch_size(1), 1);
        assert_eq!(reupload_batch_size(0), 1);
    }

    #[test]
    fn chunking_yields_ceil_n_over_limit_batches() {
        let ids: Vec<u32> = (0..1001).collect();
        let sizes: Vec<usize> = ids
            .chunks(import_batch_size(&limits(500)))
            .map(<[u32]>::len)
            .collect();
        assert_eq!(sizes, vec![500, 500, 1]);
    }

    #[test]
    fn item_outcome_reads_each_creation_id() {
        let args = json!({
            "created": { "e1": { "id": "M1" } },
            "notCreated": {
                "e2": { "type": "alreadyExists", "existingId": "M9" },
                "e3": { "type": "blobNotFound" },
                "e4": { "type": "overQuota" }
            }
        });
        assert!(matches!(item_outcome(&args, "e1"), SingleImport::Created));
        assert!(matches!(item_outcome(&args, "e2"), SingleImport::Skipped));
        assert!(matches!(
            item_outcome(&args, "e3"),
            SingleImport::NotCreated { ref error_type, .. } if error_type == "blobNotFound"
        ));
        assert!(matches!(
            item_outcome(&args, "e4"),
            SingleImport::NotCreated { ref error_type, .. } if error_type == "overQuota"
        ));
        assert!(matches!(
            item_outcome(&args, "e5"),
            SingleImport::NotCreated { ref error_type, .. } if error_type.is_empty()
        ));
    }
}
