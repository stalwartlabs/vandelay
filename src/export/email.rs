/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod import;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::mem;

use serde_json::{Map, Value, json};

use self::import::SingleImport;
use super::blobpool::UploadPool;
use super::target::{ClientId, Maps, TargetJson};
use super::uploader::Uploader;
use super::{Exporter, Plan};
use crate::archive::{BlobId, LocalId};
use crate::console::ByteSize;
use crate::error::Error;
use crate::jmap::error::JmapError;
use crate::jmap::request::Request;
use crate::jmap::session::Limits;
use crate::jmap::wire::JmapId;
use crate::sync::TypeCounts;
use crate::sync::jmap_map::EmailRow;
use crate::sync::keys::{EmailIndex, EmailKey};
use crate::types::ObjectType;

const EMAIL_MIME: &str = "message/rfc822";
const EMAIL_PARSE_MAX_ITEMS: usize = 10;
const EMAIL_KEY_PROPERTIES: [&str; 5] = ["messageId", "from", "subject", "sentAt", "to"];

impl EmailIndex {
    fn from_target(v: &Value) -> EmailIndex {
        let addresses = |k: &str| -> Vec<String> {
            v.get(k)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.get("email").and_then(Value::as_str).map(str::to_owned))
                        .collect()
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
        EmailIndex::new(
            &mids,
            &addresses("from"),
            v.get("subject").and_then(Value::as_str).unwrap_or(""),
            v.get("sentAt").and_then(Value::as_str).unwrap_or(""),
            &addresses("to"),
        )
    }
}

impl Exporter<'_> {
    pub(super) fn reconcile_emails(
        &self,
        maps: &Maps,
        counts: &mut TypeCounts,
    ) -> Result<Plan, Error> {
        let target_keys = self.email_target_keys()?;
        let locals = self.email_locals()?;
        let unmatched: Vec<&(LocalId, EmailRow)> = locals
            .iter()
            .filter(|(_, row)| {
                let matched =
                    target_keys.contains(&EmailIndex::from_json(&row.message_match).key());
                if matched {
                    counts.skipped += 1;
                }
                !matched
            })
            .collect();

        let mut batch = EmailBatch {
            exporter: self,
            uploader: Uploader::new(self),
            pool: (!self.is_dry_run()).then(|| UploadPool::new(self, EMAIL_MIME)),
            maps,
            counts,
            target_keys: &target_keys,
            size: EmailBatch::initial_size(self.account.limits()),
        };
        let mut rest = unmatched.as_slice();
        while !rest.is_empty() {
            let (chunk, tail) = rest.split_at(batch.size.min(rest.len()));
            batch.export_chunk(chunk)?;
            rest = tail;
        }
        Ok(Plan::default())
    }

    fn email_target_keys(&self) -> Result<HashSet<EmailKey>, Error> {
        let ty = ObjectType::Email;
        let targets = self.query_get(ty, Some(&["messageId"]))?;
        let mut indices: Vec<EmailIndex> = targets.iter().map(EmailIndex::from_target).collect();
        let fallback_ids: Vec<JmapId> = targets
            .iter()
            .zip(&indices)
            .filter(|(_, i)| i.mids.is_empty())
            .filter_map(|(v, _)| TargetJson(v).id().map(|id| JmapId(id.to_owned())))
            .collect();
        if !fallback_ids.is_empty() {
            let got = self.account.get::<Value>(
                ty.jmap_name(),
                &fallback_ids,
                Some(&EMAIL_KEY_PROPERTIES),
            )?;
            let by_id: HashMap<&str, &Value> = got
                .list
                .iter()
                .filter_map(|v| TargetJson(v).id().map(|i| (i, v)))
                .collect();
            for (v, slot) in targets.iter().zip(indices.iter_mut()) {
                if let Some(full) = TargetJson(v).id().and_then(|i| by_id.get(i)) {
                    *slot = EmailIndex::from_target(full);
                }
            }
        }
        Ok(indices.iter().map(EmailIndex::key).collect())
    }

    fn email_locals(&self) -> Result<Vec<(LocalId, EmailRow)>, Error> {
        let mut stmt = self
            .store
            .connection()
            .prepare(EmailRow::SELECT)
            .map_err(Error::partial)?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, EmailRow::from_row(row))))
            .map_err(Error::partial)?;
        let rows: Vec<_> = rows.collect::<Result<_, _>>().map_err(Error::partial)?;
        let mut out = Vec::with_capacity(rows.len());
        for (id, email) in rows {
            out.push((id, email?));
        }
        Ok(out)
    }
}

struct Pending<'r> {
    cid: ClientId,
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

struct BlobHint {
    mid: Option<String>,
    len: Option<u64>,
}

impl fmt::Display for BlobHint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.mid {
            Some(mid) => write!(f, "message-id <{mid}>")?,
            None => f.write_str("no message-id")?,
        }
        if let Some(len) = self.len {
            write!(f, ", {}", ByteSize(len))?;
        }
        Ok(())
    }
}

struct SizeNote<'e>(&'e JmapError);

impl fmt::Display for SizeNote<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if matches!(
            self.0,
            JmapError::RequestTooLarge | JmapError::SingleObjectTooLarge(_)
        ) {
            f.write_str("; exceeds the target server size limit, so this message is skipped and re-running will not migrate it")?;
        }
        Ok(())
    }
}

struct EmailBatch<'e, 'a> {
    exporter: &'e Exporter<'a>,
    uploader: Uploader<'a>,
    pool: Option<UploadPool>,
    maps: &'e Maps,
    counts: &'e mut TypeCounts,
    target_keys: &'e HashSet<EmailKey>,
    size: usize,
}

impl<'r> EmailBatch<'_, '_> {
    fn initial_size(limits: &Limits) -> usize {
        usize::try_from(limits.max_objects_in_set.max(1)).unwrap_or(usize::MAX)
    }

    fn halve(previous: usize) -> usize {
        (previous / 2).max(1)
    }

    fn hint(&self, row: &EmailRow) -> BlobHint {
        BlobHint {
            mid: EmailIndex::from_json(&row.message_match)
                .mids
                .into_iter()
                .next(),
            len: self.uploader.len(row.blob),
        }
    }

    fn mailbox_ids(&self, row: &EmailRow) -> Option<Map<String, Value>> {
        let mut mids = Map::new();
        for mailbox in row.mailboxes.as_slice() {
            let target = self.maps.target(ObjectType::Mailbox, *mailbox)?;
            mids.insert(target.0, Value::Bool(true));
        }
        Some(mids)
    }

    fn export_chunk(&mut self, chunk: &[&'r (LocalId, EmailRow)]) -> Result<(), Error> {
        let mut pending: Vec<Pending<'r>> = Vec::with_capacity(chunk.len());
        for (local, row) in chunk.iter().copied() {
            let cid = ClientId::email(*local);
            match self.mailbox_ids(row) {
                Some(mids) => pending.push(Pending { cid, row, mids }),
                None => {
                    self.exporter.logger.warn(format_args!(
                        "Email/import {cid} ({}) skipped: mailbox not on target",
                        self.hint(row)
                    ));
                    self.counts.failed += 1;
                }
            }
        }
        let mut size = pending.len();
        let mut retry = self.import(pending, Attempt::First)?;
        if !retry.is_empty() {
            self.size = EmailBatch::halve(self.size);
        }
        while !retry.is_empty() {
            size = EmailBatch::halve(size);
            let attempt = if size == 1 {
                Attempt::LastReupload
            } else {
                Attempt::Reupload
            };
            for p in &retry {
                self.uploader.invalidate(p.row.blob);
            }
            let mut again = Vec::new();
            while !retry.is_empty() {
                let tail = retry.split_off(size.min(retry.len()));
                let head = mem::replace(&mut retry, tail);
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
        let logger = self.exporter.logger;
        let mut failed = self.upload(&pending);
        let on_target = match attempt {
            Attempt::First => self.parsed_on_target(&pending, &failed)?,
            Attempt::Reupload | Attempt::LastReupload => HashSet::new(),
        };
        let mut ready: Vec<Pending<'r>> = Vec::with_capacity(pending.len());
        let mut items: Vec<(ClientId, Value)> = Vec::with_capacity(pending.len());
        for p in pending {
            let blob = match failed.remove(&p.row.blob) {
                Some(e) => Err(e),
                None => self
                    .uploader
                    .cached(p.row.blob)
                    .map(|b| b.0.clone())
                    .ok_or_else(|| JmapError::malformed("blob upload did not complete")),
            };
            let blob = match blob {
                Ok(b) => b,
                Err(e) => {
                    logger.warn(format_args!(
                        "Email/import {} ({}) {} failed: {e}{}",
                        p.cid,
                        self.hint(p.row),
                        attempt.upload_verb(),
                        SizeNote(&e)
                    ));
                    self.counts.failed += 1;
                    continue;
                }
            };
            if on_target.contains(&blob) {
                self.counts.skipped += 1;
                continue;
            }
            if self.exporter.is_dry_run() {
                self.counts.created += 1;
                continue;
            }
            let keywords: Map<String, Value> = p
                .row
                .keywords
                .iter()
                .map(|k| (k.clone(), Value::Bool(true)))
                .collect();
            let item = json!({
                "blobId": blob,
                "mailboxIds": Value::Object(p.mids.clone()),
                "keywords": Value::Object(keywords),
                "receivedAt": p.row.received_at,
            });
            items.push((p.cid.clone(), item));
            ready.push(p);
        }
        if items.is_empty() {
            return Ok(Vec::new());
        }

        let mut results = Vec::with_capacity(items.len());
        if let Err(e) = self.exporter.send_import(&items, &mut results) {
            let mapped = Error::from(e);
            if mapped.aborts_run() {
                return Err(mapped);
            }
            for p in ready.iter().skip(results.len()) {
                logger.warn(format_args!(
                    "Email/import {} ({}) send failed{}: {mapped}",
                    p.cid,
                    self.hint(p.row),
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
                    logger.warn(format_args!(
                        "Email/import {} ({}) failed{}: {detail}",
                        p.cid,
                        self.hint(p.row),
                        attempt.suffix()
                    ));
                    self.counts.failed += 1;
                }
                Err(e) => {
                    logger.warn(format_args!(
                        "Email/import {} ({}) send failed{}: {e}{}",
                        p.cid,
                        self.hint(p.row),
                        attempt.suffix(),
                        SizeNote(&e)
                    ));
                    self.counts.failed += 1;
                }
            }
        }
        Ok(retry)
    }

    fn parsed_on_target(
        &self,
        pending: &[Pending<'_>],
        failed: &HashMap<BlobId, JmapError>,
    ) -> Result<HashSet<String>, Error> {
        let mut found = HashSet::new();
        if self.exporter.is_dry_run() || self.target_keys.is_empty() {
            return Ok(found);
        }
        let account = self.exporter.account;
        let blobs: Vec<&str> = pending
            .iter()
            .filter(|p| !failed.contains_key(&p.row.blob))
            .filter_map(|p| self.uploader.cached(p.row.blob))
            .map(|b| b.0.as_str())
            .collect();
        let calls = usize::try_from(account.limits().max_calls_in_request.max(1)).unwrap_or(1);
        for group in blobs.chunks(EMAIL_PARSE_MAX_ITEMS.saturating_mul(calls)) {
            let mut req = Request::new();
            for (n, chunk) in group.chunks(EMAIL_PARSE_MAX_ITEMS).enumerate() {
                req.call(
                    "Email/parse",
                    json!({
                        "accountId": account.account_id(),
                        "blobIds": chunk,
                        "properties": EMAIL_KEY_PROPERTIES,
                    }),
                    format!("p{n}"),
                );
            }
            let resp = match account.send(&req) {
                Ok(resp) => resp,
                Err(e) => {
                    let mapped = Error::from(e);
                    if mapped.aborts_run() {
                        return Err(mapped);
                    }
                    self.exporter.logger.warn(format_args!(
                        "Email/parse failed, importing without a target duplicate check: {mapped}"
                    ));
                    return Ok(found);
                }
            };
            for mr in &resp.method_responses {
                if let Err(e) = mr.check_error() {
                    self.exporter.logger.warn(format_args!(
                        "Email/parse failed, importing without a target duplicate check: {e}"
                    ));
                    continue;
                }
                let Some(parsed) = mr.args.get("parsed").and_then(Value::as_object) else {
                    continue;
                };
                found.extend(
                    parsed
                        .iter()
                        .filter(|(_, email)| {
                            self.target_keys
                                .contains(&EmailIndex::from_target(email).key())
                        })
                        .map(|(blob, _)| blob.clone()),
                );
            }
        }
        Ok(found)
    }

    fn upload(&mut self, pending: &[Pending<'_>]) -> HashMap<BlobId, JmapError> {
        let Some(pool) = &self.pool else {
            let mut failed = HashMap::new();
            for p in pending {
                if let Err(e) = self.uploader.upload(p.row.blob, EMAIL_MIME) {
                    failed.insert(p.row.blob, e);
                }
            }
            return failed;
        };
        let mut seen: HashSet<BlobId> = HashSet::with_capacity(pending.len());
        let mut queue: Vec<BlobId> = Vec::with_capacity(pending.len());
        for p in pending {
            let blob = p.row.blob;
            if self.uploader.cached(blob).is_none() && seen.insert(blob) {
                queue.push(blob);
            }
        }
        pool.upload(&mut self.uploader, &queue)
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
        assert_eq!(EmailBatch::initial_size(&limits(500)), 500);
        assert_eq!(EmailBatch::initial_size(&limits(7)), 7);
        assert_eq!(EmailBatch::initial_size(&limits(0)), 1);
    }

    #[test]
    fn reupload_batches_halve_down_to_one() {
        let mut size = 500;
        let mut steps = Vec::new();
        while size > 1 {
            size = EmailBatch::halve(size);
            steps.push(size);
        }
        assert_eq!(steps, vec![250, 125, 62, 31, 15, 7, 3, 1]);
        assert_eq!(EmailBatch::halve(1), 1);
        assert_eq!(EmailBatch::halve(0), 1);
    }

    #[test]
    fn chunking_yields_ceil_n_over_limit_batches() {
        let ids: Vec<u32> = (0..1001).collect();
        let sizes: Vec<usize> = ids
            .chunks(EmailBatch::initial_size(&limits(500)))
            .map(<[u32]>::len)
            .collect();
        assert_eq!(sizes, vec![500, 500, 1]);
    }

    #[test]
    fn blob_hint_and_size_note_texts() {
        let hint = BlobHint {
            mid: Some("a@b".to_owned()),
            len: Some(2048),
        };
        assert_eq!(hint.to_string(), "message-id <a@b>, 2.0 KB");
        let bare = BlobHint {
            mid: None,
            len: None,
        };
        assert_eq!(bare.to_string(), "no message-id");
        assert!(
            SizeNote(&JmapError::RequestTooLarge)
                .to_string()
                .starts_with("; exceeds")
        );
        assert_eq!(SizeNote(&JmapError::malformed("x")).to_string(), "");
    }
}
