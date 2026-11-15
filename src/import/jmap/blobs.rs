/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::Value;

use super::JmapImporter;
use crate::archive::BlobId;
use crate::error::Error;
use crate::jmap::error::JmapError;
use crate::jmap::session::Lane;
use crate::sync::emailmeta::EmailMeta;
use crate::sync::pool::Pool;
use crate::types::ObjectType;

const EMAIL_MEDIA_TYPE: &str = "message/rfc822";
const SIEVE_MEDIA_TYPE: &str = "application/sieve";
const OCTET_STREAM: &str = "application/octet-stream";

pub(super) struct BlobRef {
    blob_id: String,
    type_hint: Cow<'static, str>,
    name: Cow<'static, str>,
}

type Downloaded = (String, Result<Vec<u8>, JmapError>);

impl BlobRef {
    pub(super) fn collect<'a>(
        ty: ObjectType,
        objects: impl Iterator<Item = &'a Value>,
    ) -> Vec<BlobRef> {
        let mut seen: HashSet<&'a str> = HashSet::new();
        let mut refs = Vec::new();
        for o in objects {
            match ty {
                ObjectType::Email => {
                    BlobRef::push_top(o, &mut seen, &mut refs, EMAIL_MEDIA_TYPE, "email")
                }
                ObjectType::SieveScript => {
                    BlobRef::push_top(o, &mut seen, &mut refs, SIEVE_MEDIA_TYPE, "script")
                }
                ObjectType::FileNode => {
                    if let Some(b) = o.get("blobId").and_then(Value::as_str)
                        && seen.insert(b)
                    {
                        let media_type = o
                            .get("mediaType")
                            .and_then(Value::as_str)
                            .unwrap_or(OCTET_STREAM);
                        let name = o.get("name").and_then(Value::as_str).unwrap_or("file");
                        refs.push(BlobRef {
                            blob_id: b.to_owned(),
                            type_hint: Cow::Owned(media_type.to_owned()),
                            name: Cow::Owned(name.to_owned()),
                        });
                    }
                }
                ObjectType::ContactCard | ObjectType::CalendarEvent => {
                    BlobRef::walk(o, &mut seen, &mut refs)
                }
                _ => return refs,
            }
        }
        refs
    }

    fn push_top<'a>(
        object: &'a Value,
        seen: &mut HashSet<&'a str>,
        refs: &mut Vec<BlobRef>,
        type_hint: &'static str,
        name: &'static str,
    ) {
        if let Some(b) = object.get("blobId").and_then(Value::as_str)
            && seen.insert(b)
        {
            refs.push(BlobRef {
                blob_id: b.to_owned(),
                type_hint: Cow::Borrowed(type_hint),
                name: Cow::Borrowed(name),
            });
        }
    }

    fn walk<'a>(value: &'a Value, seen: &mut HashSet<&'a str>, refs: &mut Vec<BlobRef>) {
        match value {
            Value::Object(map) => {
                if let Some(Value::String(b)) = map.get("blobId")
                    && seen.insert(b)
                {
                    refs.push(BlobRef {
                        blob_id: b.clone(),
                        type_hint: Cow::Borrowed(OCTET_STREAM),
                        name: Cow::Borrowed("blob"),
                    });
                }
                for v in map.values() {
                    BlobRef::walk(v, seen, refs);
                }
            }
            Value::Array(items) => {
                for v in items {
                    BlobRef::walk(v, seen, refs);
                }
            }
            _ => {}
        }
    }
}

pub(super) struct StoredEmail {
    pub(super) match_key: String,
    pub(super) date_header: Option<String>,
}

pub(super) struct StoredBlob {
    pub(super) local_id: BlobId,
    pub(super) email: Option<StoredEmail>,
}

#[derive(Default)]
pub(super) struct StoredBlobs(HashMap<String, StoredBlob>);

impl StoredBlobs {
    pub(super) fn get(&self, jmap_blob_id: &str, kind: &str) -> Result<&StoredBlob, JmapError> {
        self.0
            .get(jmap_blob_id)
            .ok_or_else(|| JmapError::malformed(format!("{kind} blob missing")))
    }
}

impl StoredBlobs {
    pub(super) fn intern(&self, jmap_blob_id: &str) -> Result<BlobId, JmapError> {
        self.0
            .get(jmap_blob_id)
            .map(|b| b.local_id)
            .ok_or_else(|| JmapError::malformed(format!("blob {jmap_blob_id} was not prefetched")))
    }
}

impl JmapImporter<'_> {
    pub(super) fn download_blobs(
        &self,
        ty: ObjectType,
        refs: Vec<BlobRef>,
    ) -> Result<StoredBlobs, Error> {
        if refs.is_empty() {
            return Ok(StoredBlobs::default());
        }
        let workers = self.account.limits().workers(self.threads, Lane::Upload);
        let account = Arc::clone(self.account);
        let pool: Pool<BlobRef, Downloaded> = Pool::new(workers, move |r: BlobRef| {
            let result = account.download(&r.blob_id, &r.type_hint, &r.name);
            (r.blob_id, result)
        });
        let total = refs.len();
        for r in refs {
            pool.submit(r);
        }
        let blobs = self.store.blobs();
        let mut out = HashMap::with_capacity(total);
        for _ in 0..total {
            let Ok((blob_id, result)) = pool.results().recv() else {
                break;
            };
            match result {
                Ok(bytes) => {
                    let local_id = blobs.intern(&bytes).map_err(Error::partial)?;
                    let email = (ty == ObjectType::Email).then(|| {
                        let meta = EmailMeta::from_blob(&bytes);
                        StoredEmail {
                            match_key: meta.index.to_json(),
                            date_header: meta.date,
                        }
                    });
                    out.insert(blob_id, StoredBlob { local_id, email });
                }
                Err(e) => self
                    .logger
                    .warn(format_args!("blob {blob_id} download failed: {e}")),
            }
        }
        self.logger.progress(format_args!(
            "  Blobs: {} / {} downloaded",
            out.len(),
            total
        ));
        Ok(StoredBlobs(out))
    }
}
