/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::jmap::session::Limits;

pub fn effective_workers(requested: usize, limits: &Limits, upload: bool) -> usize {
    let server_cap = if upload {
        limits.max_concurrent_upload
    } else {
        limits.max_concurrent_requests
    };
    let cap = server_cap.max(1).min(usize::MAX as u64) as usize;
    requested.clamp(1, cap)
}

pub struct Pool<J, R>
where
    J: Send + 'static,
    R: Send + 'static,
{
    job_tx: Option<Sender<J>>,
    result_rx: Receiver<R>,
    workers: Vec<JoinHandle<()>>,
}

impl<J, R> Pool<J, R>
where
    J: Send + 'static,
    R: Send + 'static,
{
    pub fn new<F>(threads: usize, worker: F) -> Pool<J, R>
    where
        F: Fn(J) -> R + Send + Sync + 'static,
    {
        let threads = threads.max(1);
        let (job_tx, job_rx) = unbounded::<J>();
        let (result_tx, result_rx) = unbounded::<R>();
        let worker = std::sync::Arc::new(worker);
        let mut workers = Vec::with_capacity(threads);
        for _ in 0..threads {
            let job_rx = job_rx.clone();
            let result_tx = result_tx.clone();
            let worker = worker.clone();
            workers.push(std::thread::spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    if result_tx.send(worker(job)).is_err() {
                        break;
                    }
                }
            }));
        }
        Pool {
            job_tx: Some(job_tx),
            result_rx,
            workers,
        }
    }

    pub fn submit(&self, job: J) {
        if let Some(tx) = &self.job_tx {
            let _ = tx.send(job);
        }
    }

    pub fn results(&self) -> &Receiver<R> {
        &self.result_rx
    }

    pub fn finish(mut self) -> Vec<R> {
        drop(self.job_tx.take());
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
        self.result_rx.try_iter().collect()
    }
}

impl<J, R> Drop for Pool<J, R>
where
    J: Send + 'static,
    R: Send + 'static,
{
    fn drop(&mut self) {
        drop(self.job_tx.take());
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(req: u64, up: u64) -> Limits {
        Limits {
            max_objects_in_get: 500,
            max_objects_in_set: 500,
            max_calls_in_request: 16,
            max_concurrent_requests: req,
            max_concurrent_upload: up,
            max_size_request: 10_000_000,
            max_size_upload: 50_000_000,
        }
    }

    #[test]
    fn worker_count_is_clamped_to_server_limits() {
        let l = limits(4, 2);
        assert_eq!(effective_workers(16, &l, false), 4);
        assert_eq!(effective_workers(16, &l, true), 2);
        assert_eq!(effective_workers(1, &l, false), 1);
        assert_eq!(effective_workers(0, &l, false), 1);
    }

    #[test]
    fn pool_processes_all_jobs_across_workers() {
        let pool: Pool<u64, u64> = Pool::new(4, |n| n * n);
        for n in 0..100 {
            pool.submit(n);
        }
        let mut got = pool.finish();
        got.sort_unstable();
        let expected: Vec<u64> = (0..100).map(|n| n * n).collect();
        assert_eq!(got, expected);
    }
}
