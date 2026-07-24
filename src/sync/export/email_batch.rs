/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! Batched email upload via `Blob/upload` (RFC 9404).
//!
//! The per-message path costs two HTTP requests per email: one blob upload plus
//! one `Email/import`. Servers cap in-flight requests per user
//! (`maxConcurrentRequests`, commonly 4), so request count — not bandwidth —
//! sets the ceiling. When the target advertises `urn:ietf:params:jmap:blob`,
//! one request creates a whole batch of blobs and a second imports them all.
//!
//! Only used when the capability is present; otherwise the caller keeps the
//! per-message path.

use std::collections::HashMap;

use base64::Engine;
use serde_json::{Map, Value, json};

use crate::jmap::error::JmapError;
use crate::jmap::request::{Request, URN_BLOB, check_method_error};
use crate::jmap::session::{Limits, Session};
use crate::jmap::wire::JmapId;

/// Upper bound on messages per batch. Large batches make each request slow and
/// make a single failure expensive to retry, and measured throughput plateaus
/// well before this, so the cap stays modest.
const MAX_BATCH: usize = 100;

/// Fraction of `maxSizeRequest` a batch may occupy. Base64 inflates payloads by
/// 4/3 and the envelope adds overhead, so half leaves ample headroom.
const REQUEST_BUDGET_DIVISOR: u64 = 2;

pub fn supports_blob_upload(session: &Session) -> bool {
    session.capabilities.contains_key(URN_BLOB)
}

/// Messages per batch, and the encoded-byte budget for one batch.
pub fn batch_limits(limits: &Limits) -> (usize, usize) {
    let count = MAX_BATCH.min(limits.max_objects_in_set.max(1) as usize);
    let bytes = (limits.max_size_request / REQUEST_BUDGET_DIVISOR) as usize;
    (count.max(1), bytes.max(1))
}

/// Encoded size a message contributes to a batch: base64 is 4 bytes per 3.
pub fn encoded_len(raw: usize) -> usize {
    raw.div_ceil(3) * 4
}

/// Outcome of creating one blob within a batch.
pub enum BlobOutcome {
    Created(JmapId),
    Failed { error_type: String, detail: String },
}

/// Creates every blob of a batch in a single `Blob/upload` call.
///
/// `items` maps a caller-chosen creation id to the raw message bytes. The
/// returned map has one entry per input id. An `Err` means the whole request
/// failed and the caller should fall back to the per-message path.
pub fn upload_batch(
    client: &crate::jmap::http::HttpClient,
    api_url: &str,
    account: &str,
    limits: &Limits,
    items: &[(String, &[u8])],
) -> Result<HashMap<String, BlobOutcome>, JmapError> {
    let mut create = Map::new();
    for (cid, bytes) in items {
        create.insert(
            cid.clone(),
            json!({
                "data": [ { "data:asBase64":
                    base64::engine::general_purpose::STANDARD.encode(bytes) } ],
                "type": "message/rfc822",
            }),
        );
    }
    let mut req = Request::new();
    req.call(
        "Blob/upload",
        json!({ "accountId": account, "create": Value::Object(create) }),
        "u",
    );
    req.fits(limits)?;
    let resp = req.send(client, api_url)?;
    let mr = resp.first()?;
    check_method_error(mr)?;

    let mut out = HashMap::with_capacity(items.len());
    let created = mr.args.get("created").and_then(Value::as_object);
    let not_created = mr.args.get("notCreated").and_then(Value::as_object);
    for (cid, _) in items {
        if let Some(id) = created
            .and_then(|c| c.get(cid))
            .and_then(|v| v.get("id"))
            .and_then(Value::as_str)
        {
            out.insert(cid.clone(), BlobOutcome::Created(JmapId(id.to_owned())));
            continue;
        }
        let err = not_created.and_then(|nc| nc.get(cid));
        out.insert(
            cid.clone(),
            BlobOutcome::Failed {
                error_type: err
                    .and_then(|e| e.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                detail: match err {
                    Some(e) => e.to_string(),
                    None => "Blob/upload returned neither created nor notCreated".to_owned(),
                },
            },
        );
    }
    Ok(out)
}

/// True when the server refused this blob because the account's upload quota
/// for the current window is exhausted.
pub fn is_over_quota(outcome: Option<&BlobOutcome>) -> bool {
    matches!(
        outcome,
        Some(BlobOutcome::Failed { error_type, .. }) if error_type == "overQuota"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(max_set: u64, max_req: u64) -> Limits {
        Limits {
            max_objects_in_get: 500,
            max_objects_in_set: max_set,
            max_calls_in_request: 16,
            max_concurrent_requests: 4,
            max_concurrent_upload: 4,
            max_size_request: max_req,
            max_size_upload: 50_000_000,
        }
    }

    #[test]
    fn batch_count_is_capped_by_max_objects_in_set() {
        assert_eq!(batch_limits(&limits(20, 10_000_000)).0, 20);
        assert_eq!(batch_limits(&limits(500, 10_000_000)).0, MAX_BATCH);
    }

    #[test]
    fn batch_count_and_bytes_never_collapse_to_zero() {
        let (count, bytes) = batch_limits(&limits(0, 1));
        assert_eq!(count, 1);
        assert_eq!(bytes, 1);
    }

    #[test]
    fn byte_budget_leaves_headroom_under_max_size_request() {
        let (_, bytes) = batch_limits(&limits(500, 10_000_000));
        assert_eq!(bytes, 5_000_000);
    }

    #[test]
    fn over_quota_is_recognised_only_for_that_error_type() {
        assert!(is_over_quota(Some(&BlobOutcome::Failed {
            error_type: "overQuota".to_owned(),
            detail: String::new(),
        })));
        assert!(!is_over_quota(Some(&BlobOutcome::Failed {
            error_type: "tooLarge".to_owned(),
            detail: String::new(),
        })));
        assert!(!is_over_quota(Some(&BlobOutcome::Created(JmapId(
            "b".to_owned()
        )))));
        assert!(!is_over_quota(None));
    }

    #[test]
    fn encoded_len_matches_base64_growth() {
        assert_eq!(encoded_len(3), 4);
        assert_eq!(encoded_len(4), 8);
        assert_eq!(encoded_len(0), 0);
        assert!(encoded_len(50_000) >= 50_000 * 4 / 3);
    }
}
