/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;

use super::Exporter;
use super::uploader::Uploader;
use crate::archive::BlobId;
use crate::jmap::error::JmapError;
use crate::jmap::session::Lane;
use crate::jmap::wire::JmapId;
use crate::sync::pool::Pool;

const MAX_INFLIGHT_BLOBS: usize = 64;
const MAX_INFLIGHT_BYTES: u64 = 256 * 1024 * 1024;

type UploadJob = (BlobId, Vec<u8>);
type UploadDone = (BlobId, u64, Result<JmapId, JmapError>);

#[derive(Debug)]
struct Budget {
    count: usize,
    bytes: u64,
    max_count: usize,
    max_bytes: u64,
}

impl Budget {
    fn new(max_count: usize, max_bytes: u64) -> Budget {
        Budget {
            count: 0,
            bytes: 0,
            max_count: max_count.max(1),
            max_bytes,
        }
    }

    fn admits(&self, len: u64) -> bool {
        self.count == 0
            || (self.count < self.max_count && self.bytes.saturating_add(len) <= self.max_bytes)
    }

    fn take(&mut self, len: u64) {
        self.count += 1;
        self.bytes = self.bytes.saturating_add(len);
    }

    fn release(&mut self, len: u64) {
        self.count = self.count.saturating_sub(1);
        self.bytes = self.bytes.saturating_sub(len);
    }

    fn is_idle(&self) -> bool {
        self.count == 0
    }
}

pub(super) struct UploadPool {
    pool: Pool<UploadJob, UploadDone>,
}

impl UploadPool {
    pub(super) fn new(exporter: &Exporter<'_>, content_type: &'static str) -> UploadPool {
        let workers = exporter
            .account
            .limits()
            .workers(exporter.threads, Lane::Upload);
        let account = exporter.account.clone();
        let pool = Pool::new(workers, move |(blob, bytes): UploadJob| {
            let len = bytes.len() as u64;
            let res = account.upload(&bytes, content_type);
            (blob, len, res)
        });
        UploadPool { pool }
    }

    pub(super) fn upload(
        &self,
        uploader: &mut Uploader<'_>,
        blobs: &[BlobId],
    ) -> HashMap<BlobId, JmapError> {
        let mut failed = HashMap::new();
        let mut budget = Budget::new(MAX_INFLIGHT_BLOBS, MAX_INFLIGHT_BYTES);
        for &blob in blobs {
            if uploader.cached(blob).is_some() {
                continue;
            }
            let bytes = match uploader.bytes(blob) {
                Ok(b) => b,
                Err(e) => {
                    failed.insert(blob, e);
                    continue;
                }
            };
            let len = bytes.len() as u64;
            while !budget.admits(len) {
                if !self.collect_one(uploader, &mut budget, &mut failed) {
                    break;
                }
            }
            budget.take(len);
            self.pool.submit((blob, bytes));
        }
        while !budget.is_idle() {
            if !self.collect_one(uploader, &mut budget, &mut failed) {
                break;
            }
        }
        failed
    }

    fn collect_one(
        &self,
        uploader: &mut Uploader<'_>,
        budget: &mut Budget,
        failed: &mut HashMap<BlobId, JmapError>,
    ) -> bool {
        match self.pool.results().recv() {
            Ok((blob, len, res)) => {
                budget.release(len);
                match res {
                    Ok(id) => uploader.remember(blob, id),
                    Err(e) => {
                        failed.insert(blob, e);
                    }
                }
                true
            }
            Err(_) => {
                *budget = Budget::new(MAX_INFLIGHT_BLOBS, MAX_INFLIGHT_BYTES);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_bounds_count_and_bytes() {
        let mut b = Budget::new(2, 100);
        assert!(b.admits(500));
        b.take(60);
        assert!(!b.admits(41));
        assert!(b.admits(40));
        b.take(40);
        assert!(!b.admits(0));
        b.release(60);
        assert!(b.admits(60));
        assert!(!b.admits(61));
        b.release(40);
        assert!(b.is_idle());
    }

    #[test]
    fn budget_admits_oversize_blob_only_when_idle() {
        let mut b = Budget::new(8, 10);
        assert!(b.admits(1_000));
        b.take(1_000);
        assert!(!b.admits(1));
        b.release(1_000);
        assert!(b.admits(1));
    }
}
