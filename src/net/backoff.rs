/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::Duration;

use crate::net::http::RetryStats;
use crate::net::retry::Jitter;

const SHARED_BASE: Duration = Duration::from_millis(1_000);
const TRANSPORT_BASE: Duration = Duration::from_millis(500);
const DELAY_CAP: Duration = Duration::from_millis(30_000);
const LEVEL_CAP: u32 = 16;
const SHIFT_CAP: u32 = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    TransportDrop,
    Transient,
    Fatal,
}

pub trait Retryable {
    fn fault(&self) -> Fault;
}

#[derive(Debug)]
pub enum RetryError<E> {
    Exhausted { error: E, fault: Fault },
    Fatal(E),
    Reconnect(E),
}

impl<E> RetryError<E> {
    pub fn into_inner(self) -> E {
        match self {
            RetryError::Exhausted { error, .. }
            | RetryError::Fatal(error)
            | RetryError::Reconnect(error) => error,
        }
    }
}

#[derive(Default, Clone)]
pub struct BackoffState {
    inner: Arc<BackoffInner>,
}

#[derive(Default)]
struct BackoffInner {
    level: AtomicU32,
    transient_retries: AtomicU32,
    transport_retries: AtomicU32,
    jitter: Jitter,
}

impl BackoffState {
    pub fn new() -> BackoffState {
        BackoffState::default()
    }

    pub fn reset(&self) {
        self.inner.level.store(0, Ordering::Relaxed);
    }

    pub fn current_level(&self) -> u32 {
        self.inner.level.load(Ordering::Relaxed)
    }

    pub fn transient_retries(&self) -> u32 {
        self.inner.transient_retries.load(Ordering::Relaxed)
    }

    pub fn transport_retries(&self) -> u32 {
        self.inner.transport_retries.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> RetryStats {
        let transient = u64::from(self.transient_retries());
        RetryStats {
            retries_observed: transient + u64::from(self.transport_retries()),
            retry_after_sleeps: transient,
        }
    }

    pub fn next_shared_delay(&self) -> Duration {
        let prev = self.inner.level.fetch_add(1, Ordering::Relaxed);
        self.inner.transient_retries.fetch_add(1, Ordering::Relaxed);
        let scaled = BackoffState::scaled(SHARED_BASE, prev.saturating_add(1));
        let half = scaled / 2;
        half + self.inner.jitter.uniform(scaled - half)
    }

    pub fn transport_delay(&self, attempt: u32) -> Duration {
        self.inner.transport_retries.fetch_add(1, Ordering::Relaxed);
        self.inner
            .jitter
            .uniform(BackoffState::scaled(TRANSPORT_BASE, attempt))
    }

    fn scaled(base: Duration, level: u32) -> Duration {
        let shift = level.min(LEVEL_CAP).saturating_sub(1).min(SHIFT_CAP);
        base.saturating_mul(1 << shift).min(DELAY_CAP)
    }

    pub fn run<C: ?Sized, T, E: Retryable>(
        &self,
        max_retries: u32,
        target: &mut C,
        mut op: impl FnMut(&mut C) -> Result<T, E>,
        mut reconnect: impl FnMut(&mut C, &E) -> Result<(), E>,
        mut on_transient: impl FnMut(&E),
    ) -> Result<T, RetryError<E>> {
        let mut transient_attempts: u32 = 0;
        let mut transport_attempts: u32 = 0;
        loop {
            let error = match op(target) {
                Ok(value) => {
                    self.reset();
                    return Ok(value);
                }
                Err(error) => error,
            };
            match error.fault() {
                Fault::TransportDrop => {
                    if transport_attempts >= max_retries {
                        return Err(RetryError::Exhausted {
                            error,
                            fault: Fault::TransportDrop,
                        });
                    }
                    transport_attempts += 1;
                    thread::sleep(self.transport_delay(transport_attempts));
                    reconnect(target, &error).map_err(RetryError::Reconnect)?;
                }
                Fault::Transient => {
                    if transient_attempts >= max_retries {
                        return Err(RetryError::Exhausted {
                            error,
                            fault: Fault::Transient,
                        });
                    }
                    transient_attempts += 1;
                    on_transient(&error);
                    thread::sleep(self.next_shared_delay());
                }
                Fault::Fatal => return Err(RetryError::Fatal(error)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct Fail(Fault);

    impl Retryable for Fail {
        fn fault(&self) -> Fault {
            self.0
        }
    }

    #[test]
    fn shared_delay_grows_and_caps() {
        let s = BackoffState::new();
        let d1 = s.next_shared_delay();
        assert!(d1 >= SHARED_BASE / 2 && d1 <= SHARED_BASE);
        s.next_shared_delay();
        let d3 = s.next_shared_delay();
        assert!(d3 >= d1, "{d1:?} -> {d3:?}");
        assert_eq!(s.transient_retries(), 3);
        for _ in 0..40 {
            assert!(s.next_shared_delay() <= DELAY_CAP);
        }
        let d = s.next_shared_delay();
        assert!(d >= DELAY_CAP / 2 && d <= DELAY_CAP);
    }

    #[test]
    fn reset_clears_level() {
        let s = BackoffState::new();
        s.next_shared_delay();
        s.next_shared_delay();
        assert_eq!(s.current_level(), 2);
        s.reset();
        assert_eq!(s.current_level(), 0);
    }

    #[test]
    fn transport_delay_is_local_and_bounded() {
        let s = BackoffState::new();
        assert!(s.transport_delay(1) <= TRANSPORT_BASE);
        assert!(s.transport_delay(6) <= DELAY_CAP);
        assert_eq!(s.current_level(), 0);
        assert_eq!(s.transport_retries(), 2);
    }

    #[test]
    fn counters_and_stats_are_shared_across_clones() {
        let a = BackoffState::new();
        let b = a.clone();
        a.next_shared_delay();
        a.next_shared_delay();
        b.transport_delay(1);
        assert_eq!(a.current_level(), b.current_level());
        assert_eq!(
            b.stats(),
            RetryStats {
                retries_observed: 3,
                retry_after_sleeps: 2
            }
        );
    }

    #[test]
    fn run_reconnects_on_transport_drop_then_succeeds() {
        let s = BackoffState::new();
        let mut state = (0u32, 0u32);
        let out = s.run(
            3,
            &mut state,
            |st| {
                st.0 += 1;
                if st.0 < 2 {
                    Err(Fail(Fault::TransportDrop))
                } else {
                    Ok(st.0)
                }
            },
            |st, _| {
                st.1 += 1;
                Ok(())
            },
            |_| {},
        );
        assert_eq!(out.unwrap(), 2);
        assert_eq!(state.1, 1);
    }

    #[test]
    fn run_stops_on_fatal_and_on_exhaustion() {
        let s = BackoffState::new();
        let fatal = s.run(
            3,
            &mut (),
            |_| Err::<(), _>(Fail(Fault::Fatal)),
            |_, _| Ok(()),
            |_| {},
        );
        assert!(matches!(fatal, Err(RetryError::Fatal(_))));
        let mut transients = 0;
        let exhausted = s.run(
            0,
            &mut (),
            |_| Err::<(), _>(Fail(Fault::Transient)),
            |_, _| Ok(()),
            |_| transients += 1,
        );
        assert!(matches!(
            exhausted,
            Err(RetryError::Exhausted {
                fault: Fault::Transient,
                ..
            })
        ));
        assert_eq!(transients, 0);
        let reconnect = s.run(
            1,
            &mut (),
            |_| Err::<(), _>(Fail(Fault::TransportDrop)),
            |_, _| Err(Fail(Fault::Fatal)),
            |_| {},
        );
        assert!(matches!(
            reconnect,
            Err(RetryError::Reconnect(Fail(Fault::Fatal)))
        ));
    }
}
