/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

pub struct Semaphore {
    state: Mutex<u64>,
    cond: Condvar,
}

impl Semaphore {
    pub fn new(capacity: u64) -> Semaphore {
        Semaphore {
            state: Mutex::new(capacity.max(1)),
            cond: Condvar::new(),
        }
    }

    pub fn acquire(&self) -> Permit<'_> {
        let mut guard = self.lock();
        while *guard == 0 {
            guard = self
                .cond
                .wait(guard)
                .unwrap_or_else(PoisonError::into_inner);
        }
        *guard -= 1;
        Permit { sem: self }
    }

    fn release(&self) {
        let mut guard = self.lock();
        *guard = guard.saturating_add(1);
        self.cond.notify_one();
    }

    fn lock(&self) -> MutexGuard<'_, u64> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub struct Permit<'a> {
    sem: &'a Semaphore,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.sem.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn permit_decrements_and_drop_releases() {
        let s = Semaphore::new(1);
        {
            let _p = s.acquire();
            let s2 = &s;
            let started = Instant::now();
            thread::scope(|sc| {
                let h = sc.spawn(move || {
                    let _q = s2.acquire();
                    started.elapsed()
                });
                thread::sleep(Duration::from_millis(80));
                drop(_p);
                let waited = h.join().unwrap();
                assert!(
                    waited >= Duration::from_millis(60),
                    "second acquirer should have waited until first released, waited {waited:?}"
                );
            });
        }
    }

    #[test]
    fn capacity_bounds_concurrent_acquirers() {
        let s = Arc::new(Semaphore::new(2));
        let in_flight = Arc::new(AtomicU64::new(0));
        let max = Arc::new(AtomicU64::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let s = s.clone();
            let inf = in_flight.clone();
            let mx = max.clone();
            handles.push(thread::spawn(move || {
                let _p = s.acquire();
                let cur = inf.fetch_add(1, Ordering::SeqCst) + 1;
                mx.fetch_max(cur, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(30));
                inf.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            max.load(Ordering::SeqCst),
            2,
            "max in-flight must equal capacity"
        );
    }
}
