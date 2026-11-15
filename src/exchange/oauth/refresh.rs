/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::exchange::oauth::OAuthError;
use crate::exchange::oauth::token::AcquiredToken;
use crate::logging::Logger;

const SLEEP_CHUNK: Duration = Duration::from_secs(2);
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy)]
pub struct RefreshSchedule {
    pub lead: Duration,
    pub fallback_lifetime: Duration,
}

pub struct RefreshPlan {
    pub label: &'static str,
    pub schedule: RefreshSchedule,
    pub deadline_unix: u64,
    pub refresh_token: Option<String>,
    pub logger: Logger,
}

pub struct TokenRefresher {
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

struct Sleeper(Arc<AtomicBool>);

impl Sleeper {
    fn stopped(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    fn sleep(&self, total: Duration) -> bool {
        let mut left = total;
        while !left.is_zero() && !self.stopped() {
            let step = SLEEP_CHUNK.min(left);
            thread::sleep(step);
            left = left.saturating_sub(step);
        }
        !self.stopped()
    }
}

impl TokenRefresher {
    pub fn unix_now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    pub fn spawn<R, A>(plan: RefreshPlan, mut renew: R, apply: A) -> Option<TokenRefresher>
    where
        R: FnMut(Option<&str>) -> Result<AcquiredToken, OAuthError> + Send + 'static,
        A: Fn(&AcquiredToken) + Send + 'static,
    {
        let shutdown = Arc::new(AtomicBool::new(false));
        let sleeper = Sleeper(shutdown.clone());
        let handle = thread::Builder::new()
            .name(format!(
                "vandelay-{}-token-refresh",
                plan.label.to_ascii_lowercase()
            ))
            .spawn(move || {
                let RefreshPlan {
                    label,
                    schedule,
                    mut deadline_unix,
                    mut refresh_token,
                    logger,
                } = plan;
                while !sleeper.stopped() {
                    let now = TokenRefresher::unix_now();
                    let refresh_at = deadline_unix.saturating_sub(schedule.lead.as_secs());
                    if refresh_at > now && !sleeper.sleep(Duration::from_secs(refresh_at - now)) {
                        break;
                    }
                    match renew(refresh_token.as_deref()) {
                        Ok(token) => {
                            apply(&token);
                            let now = TokenRefresher::unix_now();
                            deadline_unix = token
                                .deadline_unix(now)
                                .filter(|deadline| *deadline > now)
                                .unwrap_or_else(|| {
                                    now.saturating_add(schedule.fallback_lifetime.as_secs())
                                });
                            if token.refresh_token.is_some() {
                                refresh_token = token.refresh_token;
                            }
                        }
                        Err(e) => {
                            logger.warn(format_args!(
                                "{label} token refresh failed: {e}; sleeping 60s before retry"
                            ));
                            sleeper.sleep(RETRY_AFTER_FAILURE);
                        }
                    }
                }
            })
            .ok()?;
        Some(TokenRefresher {
            shutdown,
            handle: Some(handle),
        })
    }
}

impl Drop for TokenRefresher {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Instant;

    use crate::logging::Level;

    const SCHEDULE: RefreshSchedule = RefreshSchedule {
        lead: Duration::from_secs(60),
        fallback_lifetime: Duration::from_secs(3000),
    };

    // A due token is renewed with the stored refresh token, and drop joins promptly.
    #[test]
    fn renews_a_due_token_and_joins_on_drop() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let applied = Arc::new(Mutex::new(Vec::new()));
        let seen_in = seen.clone();
        let applied_in = applied.clone();
        let refresher = TokenRefresher::spawn(
            RefreshPlan {
                label: "test",
                schedule: SCHEDULE,
                deadline_unix: TokenRefresher::unix_now(),
                refresh_token: Some("R0".to_owned()),
                logger: Logger::new(Level::Quiet),
            },
            move |refresh| {
                seen_in.lock().unwrap().push(refresh.map(str::to_owned));
                Ok(AcquiredToken {
                    access_token: "fresh".to_owned(),
                    refresh_token: Some("R1".to_owned()),
                    expires_in: Some(3600),
                    tenant_id: None,
                    upn: None,
                    name: None,
                })
            },
            move |token| applied_in.lock().unwrap().push(token.access_token.clone()),
        )
        .unwrap();
        let started = Instant::now();
        while applied.lock().unwrap().is_empty() && started.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(5));
        }
        let dropped = Instant::now();
        drop(refresher);
        assert!(dropped.elapsed() < Duration::from_secs(5));
        assert_eq!(applied.lock().unwrap().as_slice(), ["fresh"]);
        assert_eq!(seen.lock().unwrap().as_slice(), [Some("R0".to_owned())]);
    }
}
