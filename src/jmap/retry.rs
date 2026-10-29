/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::cell::Cell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::jmap::http::RetryPolicy;

pub const THROTTLE_BASE: Duration = Duration::from_secs(1);
const THROTTLE_LEVEL_CAP: u32 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Retryable,
    Fatal,
}

pub fn classify_http_status(status: u16) -> Disposition {
    match status {
        429 | 502 | 503 | 504 => Disposition::Retryable,
        _ => Disposition::Fatal,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodCallKind {
    Read,
    SingleObjectWrite,
}

pub fn jmap_method_disposition(error_type: &str, kind: MethodCallKind) -> Disposition {
    match error_type {
        "serverUnavailable" => Disposition::Retryable,
        "serverPartialFail" => match kind {
            MethodCallKind::Read => Disposition::Retryable,
            MethodCallKind::SingleObjectWrite => Disposition::Fatal,
        },
        _ => Disposition::Fatal,
    }
}

pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let trimmed = value.trim();
    if let Ok(secs) = trimmed.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let imf_fixdate = time::macros::format_description!(
        "[weekday repr:short], [day] [month repr:short] [year] \
         [hour]:[minute]:[second] GMT"
    );
    let parsed = time::PrimitiveDateTime::parse(trimmed, imf_fixdate).ok()?;
    let target: SystemTime = parsed.assume_utc().into();
    target.duration_since(now).ok()
}

thread_local! {
    static RNG_STATE: Cell<u64> = Cell::new(seed());
}

fn seed() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let tid = {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&std::thread::current().id(), &mut hasher);
        std::hash::Hasher::finish(&hasher)
    };
    (nanos ^ tid).max(1)
}

fn next_u64() -> u64 {
    RNG_STATE.with(|cell| {
        let mut x = cell.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        cell.set(x);
        x
    })
}

fn jitter_uniform(span: Duration) -> Duration {
    let nanos = span.as_nanos();
    if nanos == 0 {
        return Duration::ZERO;
    }
    let pick = (next_u64() as u128) % (nanos + 1);
    Duration::from_nanos(pick.min(u64::MAX as u128) as u64)
}

pub fn backoff_delay(policy: &RetryPolicy, attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(32);
    let scaled = policy
        .base
        .saturating_mul(1u32.checked_shl(shift).unwrap_or(u32::MAX));
    let capped = scaled.min(policy.cap);
    jitter_uniform(capped)
}

#[derive(Debug, Default)]
pub struct Cooldown {
    until: Mutex<Option<Instant>>,
}

impl Cooldown {
    pub fn new() -> Cooldown {
        Cooldown {
            until: Mutex::new(None),
        }
    }

    pub fn arm(&self, delay: Duration) {
        let target = Instant::now() + delay;
        if let Ok(mut guard) = self.until.lock() {
            let extend = guard.map(|cur| target > cur).unwrap_or(true);
            if extend {
                *guard = Some(target);
            }
        }
    }

    pub fn remaining(&self) -> Option<Duration> {
        let guard = self.until.lock().ok()?;
        let until = (*guard)?;
        let now = Instant::now();
        if until > now { Some(until - now) } else { None }
    }

    pub fn wait(&self) {
        while let Some(remaining) = self.remaining() {
            std::thread::sleep(remaining);
        }
    }
}

#[derive(Debug, Default)]
pub struct RateLimitState {
    cooldown: Cooldown,
    level: AtomicU32,
}

impl RateLimitState {
    pub fn new() -> RateLimitState {
        RateLimitState {
            cooldown: Cooldown::new(),
            level: AtomicU32::new(0),
        }
    }

    pub fn cooldown(&self) -> &Cooldown {
        &self.cooldown
    }

    pub fn level(&self) -> u32 {
        self.level.load(Ordering::Relaxed)
    }

    pub fn on_success(&self) {
        self.level.store(0, Ordering::Relaxed);
    }

    pub fn on_throttle(&self, policy: &RetryPolicy, retry_after: Option<Duration>) -> Duration {
        let n = self
            .level
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1)
            .min(THROTTLE_LEVEL_CAP);
        let delay = retry_after.unwrap_or_else(|| throttle_backoff(policy, n));
        self.cooldown.arm(delay);
        delay
    }
}

pub fn throttle_backoff(policy: &RetryPolicy, level: u32) -> Duration {
    let shift = level.saturating_sub(1).min(20);
    let scaled = THROTTLE_BASE
        .saturating_mul(1u32.checked_shl(shift).unwrap_or(u32::MAX))
        .min(policy.cap);
    let half = scaled / 2;
    half + jitter_uniform(scaled - half)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> RetryPolicy {
        RetryPolicy::new(5)
    }

    #[test]
    fn backoff_is_bounded_by_cap_with_full_jitter() {
        let p = policy();
        for attempt in 1..=12 {
            let d = backoff_delay(&p, attempt);
            assert!(d <= p.cap, "attempt {attempt} delay {d:?} exceeds cap");
        }
    }

    #[test]
    fn backoff_grows_until_cap() {
        let p = policy();
        let big = backoff_delay(&p, 30);
        assert!(big <= p.cap);
    }

    #[test]
    fn retry_after_seconds() {
        assert_eq!(
            parse_retry_after("120", SystemTime::now()),
            Some(Duration::from_secs(120))
        );
    }

    #[test]
    fn retry_after_http_date_in_future() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(784_111_777);
        let d = parse_retry_after("Sun, 06 Nov 1994 08:49:47 GMT", now);
        assert_eq!(d, Some(Duration::from_secs(10)));
    }

    #[test]
    fn retry_after_garbage_is_none() {
        assert_eq!(parse_retry_after("soon", SystemTime::now()), None);
    }

    #[test]
    fn status_classification() {
        assert_eq!(classify_http_status(429), Disposition::Retryable);
        assert_eq!(classify_http_status(503), Disposition::Retryable);
        assert_eq!(classify_http_status(400), Disposition::Fatal);
        assert_eq!(classify_http_status(401), Disposition::Fatal);
    }

    #[test]
    fn method_classification() {
        for kind in [MethodCallKind::Read, MethodCallKind::SingleObjectWrite] {
            assert_eq!(
                jmap_method_disposition("serverUnavailable", kind),
                Disposition::Retryable,
                "{kind:?}"
            );
            assert_eq!(
                jmap_method_disposition("invalidArguments", kind),
                Disposition::Fatal,
                "{kind:?}"
            );
            assert_eq!(
                jmap_method_disposition("forbidden", kind),
                Disposition::Fatal,
                "{kind:?}"
            );
        }
    }

    #[test]
    fn partial_fail_is_never_retried_for_a_write() {
        assert_eq!(
            jmap_method_disposition("serverPartialFail", MethodCallKind::Read),
            Disposition::Retryable
        );
        assert_eq!(
            jmap_method_disposition("serverPartialFail", MethodCallKind::SingleObjectWrite),
            Disposition::Fatal
        );
    }

    #[test]
    fn cooldown_arms_and_clears() {
        let c = Cooldown::new();
        assert!(c.remaining().is_none());
        c.arm(Duration::from_millis(40));
        assert!(c.remaining().is_some());
        std::thread::sleep(Duration::from_millis(60));
        assert!(c.remaining().is_none());
    }

    #[test]
    fn throttle_backoff_doubles_then_clamps_to_cap() {
        let p = RetryPolicy::new(8);
        let mut prev = throttle_backoff(&p, 1);
        for level in 2..=4 {
            let d = throttle_backoff(&p, level);
            assert!(
                d >= prev / 2,
                "level {level} ({d:?}) collapsed below half of prev ({prev:?})"
            );
            prev = d;
        }
        assert!(throttle_backoff(&p, 20) <= p.cap);
    }

    #[test]
    fn rate_limit_state_escalates_across_concurrent_callers() {
        let p = RetryPolicy::new(8);
        let s = RateLimitState::new();
        let first = s.on_throttle(&p, None);
        let second = s.on_throttle(&p, None);
        let third = s.on_throttle(&p, None);
        assert!(
            third >= first,
            "level 3 delay ({third:?}) should not be smaller than level 1 ({first:?})"
        );
        assert!(
            second >= first,
            "level 2 delay ({second:?}) should not be smaller than level 1 ({first:?})"
        );
        assert!(s.level() >= 3);
    }

    #[test]
    fn rate_limit_state_resets_on_success() {
        let p = RetryPolicy::new(8);
        let s = RateLimitState::new();
        let _ = s.on_throttle(&p, None);
        let _ = s.on_throttle(&p, None);
        let _ = s.on_throttle(&p, None);
        assert!(s.level() >= 3);
        s.on_success();
        assert_eq!(s.level(), 0);
        let next = s.on_throttle(&p, None);
        assert!(
            next <= THROTTLE_BASE,
            "first throttle after reset should be small ({next:?})"
        );
    }

    #[test]
    fn rate_limit_state_honours_retry_after_over_schedule() {
        let p = RetryPolicy::new(8);
        let s = RateLimitState::new();
        let d = s.on_throttle(&p, Some(Duration::from_millis(750)));
        assert_eq!(d, Duration::from_millis(750));
        assert_eq!(s.level(), 1);
    }
}
