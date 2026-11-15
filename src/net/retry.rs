/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use time::PrimitiveDateTime;
use time::format_description::BorrowedFormatItem;
use time::macros::format_description;
use ureq::config::ConfigBuilder;
use ureq::typestate::AgentScope;

const IO_TIMEOUT: Duration = Duration::from_secs(300);
const DEFAULT_BASE: Duration = Duration::from_millis(500);
const DEFAULT_CAP: Duration = Duration::from_secs(30);
pub const THROTTLE_BASE: Duration = Duration::from_secs(1);
const THROTTLE_LEVEL_CAP: u32 = 16;
const BACKOFF_SHIFT_CAP: u32 = 32;
const THROTTLE_SHIFT_CAP: u32 = 20;
const SEED_FALLBACK: u64 = 0x9E37_79B9_7F4A_7C15;

pub const IMF_FIXDATE: &[BorrowedFormatItem<'static>] = format_description!(
    "[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT"
);

const TOO_MANY_REQUESTS: u16 = 429;
const BAD_GATEWAY: u16 = 502;
const SERVICE_UNAVAILABLE: u16 = 503;
const GATEWAY_TIMEOUT: u16 = 504;

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub base: Duration,
    pub cap: Duration,
    pub io_timeout: Duration,
}

impl RetryPolicy {
    pub fn new(max_retries: u32) -> Self {
        RetryPolicy {
            max_retries,
            base: DEFAULT_BASE,
            cap: DEFAULT_CAP,
            io_timeout: IO_TIMEOUT,
        }
    }

    pub fn apply_timeouts(&self, builder: ConfigBuilder<AgentScope>) -> ConfigBuilder<AgentScope> {
        let timeout = Some(self.io_timeout);
        builder
            .timeout_connect(timeout)
            .timeout_per_read(timeout)
            .timeout_per_write(timeout)
    }

    pub fn backoff(&self, attempt: u32, jitter: &Jitter) -> Duration {
        let shift = attempt.saturating_sub(1).min(BACKOFF_SHIFT_CAP);
        let scaled = self
            .base
            .saturating_mul(1u32.checked_shl(shift).unwrap_or(u32::MAX));
        jitter.uniform(scaled.min(self.cap))
    }

    pub fn throttle_backoff(&self, level: u32, jitter: &Jitter) -> Duration {
        let shift = level.saturating_sub(1).min(THROTTLE_SHIFT_CAP);
        let scaled = THROTTLE_BASE
            .saturating_mul(1u32.checked_shl(shift).unwrap_or(u32::MAX))
            .min(self.cap);
        let half = scaled / 2;
        half + jitter.uniform(scaled - half)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Retry,
    Fatal,
}

impl Disposition {
    pub fn of_status(status: u16) -> Disposition {
        match status {
            TOO_MANY_REQUESTS | BAD_GATEWAY | SERVICE_UNAVAILABLE | GATEWAY_TIMEOUT => {
                Disposition::Retry
            }
            _ => Disposition::Fatal,
        }
    }
}

#[derive(Debug)]
pub struct Jitter(AtomicU64);

impl Jitter {
    pub fn new() -> Jitter {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(SEED_FALLBACK);
        Jitter::with_seed(nanos ^ u64::from(std::process::id()).rotate_left(32))
    }

    pub fn with_seed(seed: u64) -> Jitter {
        Jitter(AtomicU64::new(seed.max(1)))
    }

    pub fn next_u64(&self) -> u64 {
        let mut current = self.0.load(Ordering::Relaxed);
        loop {
            let next = Jitter::step(current);
            match self
                .0
                .compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return next,
                Err(seen) => current = seen,
            }
        }
    }

    fn step(mut x: u64) -> u64 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    }

    pub fn uniform(&self, span: Duration) -> Duration {
        let nanos = span.as_nanos();
        if nanos == 0 {
            return Duration::ZERO;
        }
        let pick = u128::from(self.next_u64()) % (nanos + 1);
        Duration::from_nanos(u64::try_from(pick).unwrap_or(u64::MAX))
    }
}

impl Default for Jitter {
    fn default() -> Jitter {
        Jitter::new()
    }
}

#[derive(Debug, Default)]
pub struct Cooldown {
    until: Mutex<Option<Instant>>,
}

impl Cooldown {
    pub fn new() -> Cooldown {
        Cooldown::default()
    }

    pub fn arm(&self, delay: Duration) {
        let target = Instant::now() + delay;
        if let Ok(mut guard) = self.until.lock()
            && guard.is_none_or(|current| target > current)
        {
            *guard = Some(target);
        }
    }

    pub fn remaining(&self) -> Option<Duration> {
        let until = (*self.until.lock().ok()?)?;
        until
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
    }

    pub fn wait(&self) {
        while let Some(remaining) = self.remaining() {
            thread::sleep(remaining);
        }
    }
}

#[derive(Debug, Default)]
pub struct RateLimit {
    cooldown: Cooldown,
    level: AtomicU32,
}

impl RateLimit {
    pub fn new() -> RateLimit {
        RateLimit::default()
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

    pub fn on_throttle(
        &self,
        policy: &RetryPolicy,
        jitter: &Jitter,
        retry_after: Option<Duration>,
    ) -> Duration {
        let level = self
            .level
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1)
            .min(THROTTLE_LEVEL_CAP);
        let delay = retry_after.unwrap_or_else(|| policy.throttle_backoff(level, jitter));
        self.cooldown.arm(delay);
        delay
    }
}

pub struct RetryAfter;

impl RetryAfter {
    pub fn parse(value: &str, now: SystemTime) -> Option<Duration> {
        let trimmed = value.trim();
        if let Ok(secs) = trimmed.parse::<u64>() {
            return Some(Duration::from_secs(secs));
        }
        let parsed = PrimitiveDateTime::parse(trimmed, IMF_FIXDATE).ok()?;
        SystemTime::from(parsed.assume_utc())
            .duration_since(now)
            .ok()
    }

    pub fn parse_now(value: &str) -> Option<Duration> {
        RetryAfter::parse(value, SystemTime::now())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryWait(pub Duration);

impl fmt::Display for RetryWait {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let total = self.0.as_secs();
        match total {
            0 => write!(f, "{}ms", self.0.as_millis()),
            1..60 => write!(f, "{total}s"),
            60..3600 => write!(f, "{}m{:02}s", total / 60, total % 60),
            _ => write!(
                f,
                "{}h{:02}m{:02}s",
                total / 3600,
                (total % 3600) / 60,
                total % 60
            ),
        }
    }
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
        let jitter = Jitter::with_seed(7);
        for attempt in 1..=40 {
            let d = p.backoff(attempt, &jitter);
            assert!(d <= p.cap, "attempt {attempt} delay {d:?} exceeds cap");
        }
    }

    #[test]
    fn first_backoff_stays_within_base() {
        let p = policy();
        let jitter = Jitter::with_seed(99);
        for _ in 0..100 {
            assert!(p.backoff(1, &jitter) <= p.base);
        }
    }

    #[test]
    fn jitter_with_zero_seed_still_advances() {
        let jitter = Jitter::with_seed(0);
        let a = jitter.next_u64();
        let b = jitter.next_u64();
        assert_ne!(a, 0);
        assert_ne!(a, b);
    }

    #[test]
    fn jitter_uniform_of_zero_span_is_zero() {
        assert_eq!(Jitter::with_seed(3).uniform(Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn jitter_is_shared_across_threads() {
        let jitter = Jitter::with_seed(11);
        thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for _ in 0..1000 {
                        assert!(
                            jitter.uniform(Duration::from_millis(5)) <= Duration::from_millis(5)
                        );
                    }
                });
            }
        });
    }

    #[test]
    fn retry_after_seconds() {
        assert_eq!(
            RetryAfter::parse(" 120 ", SystemTime::now()),
            Some(Duration::from_secs(120))
        );
    }

    #[test]
    fn retry_after_http_date_in_future() {
        let now = UNIX_EPOCH + Duration::from_secs(784_111_777);
        let d = RetryAfter::parse("Sun, 06 Nov 1994 08:49:47 GMT", now);
        assert_eq!(d, Some(Duration::from_secs(10)));
    }

    #[test]
    fn retry_after_http_date_in_past_is_none() {
        let now = UNIX_EPOCH + Duration::from_secs(784_111_800);
        assert_eq!(
            RetryAfter::parse("Sun, 06 Nov 1994 08:49:47 GMT", now),
            None
        );
    }

    #[test]
    fn retry_after_garbage_is_none() {
        assert_eq!(RetryAfter::parse("soon", SystemTime::now()), None);
    }

    #[test]
    fn status_classification() {
        for status in [429, 502, 503, 504] {
            assert_eq!(Disposition::of_status(status), Disposition::Retry);
        }
        for status in [200, 400, 401, 404, 500] {
            assert_eq!(Disposition::of_status(status), Disposition::Fatal);
        }
    }

    #[test]
    fn cooldown_arms_and_clears() {
        let c = Cooldown::new();
        assert!(c.remaining().is_none());
        c.arm(Duration::from_millis(40));
        assert!(c.remaining().is_some());
        thread::sleep(Duration::from_millis(60));
        assert!(c.remaining().is_none());
    }

    #[test]
    fn cooldown_never_shortens() {
        let c = Cooldown::new();
        c.arm(Duration::from_secs(60));
        c.arm(Duration::from_millis(1));
        assert!(
            c.remaining()
                .is_some_and(|left| left > Duration::from_secs(30))
        );
    }

    #[test]
    fn throttle_backoff_doubles_then_clamps_to_cap() {
        let p = RetryPolicy::new(8);
        let jitter = Jitter::with_seed(5);
        let mut prev = p.throttle_backoff(1, &jitter);
        for level in 2..=4 {
            let d = p.throttle_backoff(level, &jitter);
            assert!(
                d >= prev / 2,
                "level {level} ({d:?}) below half of {prev:?}"
            );
            prev = d;
        }
        assert!(p.throttle_backoff(20, &jitter) <= p.cap);
    }

    #[test]
    fn rate_limit_escalates_and_resets() {
        let p = RetryPolicy::new(8);
        let jitter = Jitter::with_seed(17);
        let s = RateLimit::new();
        let first = s.on_throttle(&p, &jitter, None);
        let second = s.on_throttle(&p, &jitter, None);
        let third = s.on_throttle(&p, &jitter, None);
        assert!(second >= first);
        assert!(third >= first);
        assert_eq!(s.level(), 3);
        s.on_success();
        assert_eq!(s.level(), 0);
        assert!(s.on_throttle(&p, &jitter, None) <= THROTTLE_BASE);
    }

    #[test]
    fn rate_limit_honours_retry_after_over_schedule() {
        let p = RetryPolicy::new(8);
        let s = RateLimit::new();
        let d = s.on_throttle(&p, &Jitter::with_seed(1), Some(Duration::from_millis(750)));
        assert_eq!(d, Duration::from_millis(750));
        assert_eq!(s.level(), 1);
        assert!(s.cooldown().remaining().is_some());
    }

    #[test]
    fn retry_wait_formats_like_the_jmap_client() {
        let cases = [
            (Duration::from_millis(250), "250ms"),
            (Duration::from_secs(45), "45s"),
            (Duration::from_secs(125), "2m05s"),
            (Duration::from_secs(3725), "1h02m05s"),
        ];
        for (d, want) in cases {
            assert_eq!(RetryWait(d).to_string(), want);
        }
    }
}
