/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;

use super::Exporter;
use crate::archive::BlobId;
use crate::jmap::error::JmapError;
use crate::jmap::wire::JmapId;

pub(super) struct Uploader<'a> {
    exporter: Exporter<'a>,
    cache: HashMap<BlobId, JmapId>,
    touched: Vec<BlobId>,
}

impl<'a> Uploader<'a> {
    pub(super) fn new(exporter: &Exporter<'a>) -> Uploader<'a> {
        Uploader {
            exporter: *exporter,
            cache: HashMap::new(),
            touched: Vec::new(),
        }
    }

    pub(super) fn upload(&mut self, blob: BlobId, content_type: &str) -> Result<JmapId, JmapError> {
        self.touched.push(blob);
        if let Some(id) = self.cache.get(&blob) {
            return Ok(id.clone());
        }
        let id = if self.exporter.is_dry_run() {
            self.exporter
                .store
                .blobs()
                .len(blob)?
                .ok_or_else(|| Uploader::missing(blob))?;
            JmapId(format!("dryrun-blob-{blob}"))
        } else {
            self.exporter
                .account
                .upload(&self.bytes(blob)?, content_type)?
        };
        self.cache.insert(blob, id.clone());
        Ok(id)
    }

    pub(super) fn bytes(&self, blob: BlobId) -> Result<Vec<u8>, JmapError> {
        self.exporter
            .store
            .blobs()
            .bytes(blob)?
            .ok_or_else(|| Uploader::missing(blob))
    }

    pub(super) fn len(&self, blob: BlobId) -> Option<u64> {
        self.exporter.store.blobs().len(blob).ok().flatten()
    }

    pub(super) fn cached(&self, blob: BlobId) -> Option<&JmapId> {
        self.cache.get(&blob)
    }

    pub(super) fn remember(&mut self, blob: BlobId, id: JmapId) {
        self.cache.insert(blob, id);
    }

    pub(super) fn invalidate(&mut self, blob: BlobId) {
        self.cache.remove(&blob);
    }

    pub(super) fn clear_touched(&mut self) {
        self.touched.clear();
    }

    pub(super) fn invalidate_touched(&mut self) {
        for blob in self.touched.drain(..) {
            self.cache.remove(&blob);
        }
    }

    #[cold]
    fn missing(blob: BlobId) -> JmapError {
        JmapError::malformed(format!("blob local id {blob} missing"))
    }
}
