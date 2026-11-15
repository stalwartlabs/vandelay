/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ureq::Agent;
use ureq::config::{Config, RedirectAuthHeaders};
use ureq::tls::{RootCerts, TlsConfig};
use url::Url;

use crate::logging::Logger;
use crate::net::retry::{Jitter, RateLimit, RetryPolicy};

const USER_AGENT: &str = concat!("vandelay/", env!("CARGO_PKG_VERSION"));
const EXCERPT_LIMIT: usize = 512;
const ELLIPSIS: &str = "...";
const REPLACEMENT: &str = "\u{FFFD}";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsPolicy {
    Verify,
    AcceptInvalid,
}

#[derive(Debug, Clone, Copy)]
pub struct ClientOptions {
    pub retry: RetryPolicy,
    pub tls: TlsPolicy,
    pub logger: Logger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redirects {
    Follow,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Methods {
    Standard,
    Extended,
}

#[derive(Debug, Clone, Copy)]
pub struct AgentProfile {
    pub redirects: Redirects,
    pub methods: Methods,
    pub max_body: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetryStats {
    pub retries_observed: u64,
    pub retry_after_sleeps: u64,
}

#[derive(Debug, Default)]
pub struct TransferStats {
    retries: AtomicU64,
    retry_after_sleeps: AtomicU64,
    requests: AtomicU64,
}

impl TransferStats {
    pub fn snapshot(&self) -> RetryStats {
        RetryStats {
            retries_observed: self.retries.load(Ordering::Relaxed),
            retry_after_sleeps: self.retry_after_sleeps.load(Ordering::Relaxed),
        }
    }

    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    pub fn note_request(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn note_retry(&self) {
        self.retries.fetch_add(1, Ordering::Relaxed);
    }

    pub fn note_retry_after(&self) {
        self.retry_after_sleeps.fetch_add(1, Ordering::Relaxed);
    }
}

pub struct HttpCore {
    agent: Agent,
    retry: RetryPolicy,
    tls: TlsPolicy,
    max_body: u64,
    rate_limit: RateLimit,
    jitter: Jitter,
    stats: TransferStats,
    logger: Logger,
}

impl HttpCore {
    pub fn new(options: ClientOptions, profile: AgentProfile) -> HttpCore {
        let mut builder = options
            .retry
            .apply_timeouts(Config::builder())
            .http_status_as_error(false)
            .redirect_auth_headers(RedirectAuthHeaders::SameHost)
            .tls_config(
                TlsConfig::builder()
                    .unversioned_rustls_crypto_provider(Arc::new(
                        rustls::crypto::aws_lc_rs::default_provider(),
                    ))
                    .root_certs(RootCerts::PlatformVerifier)
                    .disable_verification(options.tls == TlsPolicy::AcceptInvalid)
                    .build(),
            );
        if profile.methods == Methods::Extended {
            builder = builder.allow_non_standard_methods(true);
        }
        if profile.redirects == Redirects::Manual {
            builder = builder.max_redirects(0);
        }
        HttpCore {
            agent: builder.build().new_agent(),
            retry: options.retry,
            tls: options.tls,
            max_body: profile.max_body,
            rate_limit: RateLimit::new(),
            jitter: Jitter::new(),
            stats: TransferStats::default(),
            logger: options.logger,
        }
    }

    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    pub fn retry(&self) -> &RetryPolicy {
        &self.retry
    }

    pub fn tls(&self) -> TlsPolicy {
        self.tls
    }

    pub fn max_body(&self) -> u64 {
        self.max_body
    }

    pub fn rate_limit(&self) -> &RateLimit {
        &self.rate_limit
    }

    pub fn logger(&self) -> Logger {
        self.logger
    }

    pub fn stats(&self) -> &TransferStats {
        &self.stats
    }

    pub fn user_agent(&self) -> &'static str {
        USER_AGENT
    }

    pub fn backoff(&self, attempt: u32) -> Duration {
        self.retry.backoff(attempt, &self.jitter)
    }

    pub fn throttle(&self, retry_after: Option<Duration>) -> Duration {
        self.rate_limit
            .on_throttle(&self.retry, &self.jitter, retry_after)
    }

    pub fn is_cross_host(requested: &str, landed: &str) -> bool {
        match (Url::parse(requested), Url::parse(landed)) {
            (Ok(a), Ok(b)) => {
                a.host_str() != b.host_str()
                    || a.port_or_known_default() != b.port_or_known_default()
            }
            _ => false,
        }
    }
}

#[derive(Debug)]
pub enum TransportError {
    Io(io::Error),
    Timeout(ureq::Timeout),
    HostNotFound,
    ConnectionFailed,
    BodyStalled,
    Tls(&'static str),
    TooManyRedirects,
    RedirectFailed,
    TlsRequired,
    Other(ureq::Error),
}

impl TransportError {
    pub fn is_connect(&self) -> bool {
        matches!(
            self,
            TransportError::TooManyRedirects
                | TransportError::RedirectFailed
                | TransportError::TlsRequired
                | TransportError::Other(_)
        )
    }
}

impl From<ureq::Error> for TransportError {
    fn from(err: ureq::Error) -> TransportError {
        match err {
            ureq::Error::Io(e) => TransportError::Io(e),
            ureq::Error::Timeout(t) => TransportError::Timeout(t),
            ureq::Error::HostNotFound => TransportError::HostNotFound,
            ureq::Error::ConnectionFailed => TransportError::ConnectionFailed,
            ureq::Error::BodyStalled => TransportError::BodyStalled,
            ureq::Error::Tls(m) => TransportError::Tls(m),
            ureq::Error::TooManyRedirects => TransportError::TooManyRedirects,
            ureq::Error::RedirectFailed => TransportError::RedirectFailed,
            ureq::Error::TlsRequired => TransportError::TlsRequired,
            other => TransportError::Other(other),
        }
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Io(e) => write!(f, "io: {e}"),
            TransportError::Timeout(t) => write!(f, "timeout: {t}"),
            TransportError::HostNotFound => f.write_str("host not found"),
            TransportError::ConnectionFailed => f.write_str("connection failed"),
            TransportError::BodyStalled => f.write_str("body stalled"),
            TransportError::Tls(m) => write!(f, "tls: {m}"),
            TransportError::TooManyRedirects => f.write_str("too many redirects"),
            TransportError::RedirectFailed => f.write_str("redirect failed"),
            TransportError::TlsRequired => {
                f.write_str("server requires TLS but transport is unsecured")
            }
            TransportError::Other(e) => e.fmt(f),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BodyExcerpt<'a>(pub &'a [u8]);

impl fmt::Display for BodyExcerpt<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut budget = EXCERPT_LIMIT;
        for chunk in self.0.utf8_chunks() {
            let valid = chunk.valid();
            if valid.len() > budget {
                let cut = valid.floor_char_boundary(budget);
                f.write_str(valid.get(..cut).unwrap_or_default())?;
                return f.write_str(ELLIPSIS);
            }
            f.write_str(valid)?;
            budget -= valid.len();
            if !chunk.invalid().is_empty() {
                if REPLACEMENT.len() > budget {
                    return f.write_str(ELLIPSIS);
                }
                f.write_str(REPLACEMENT)?;
                budget -= REPLACEMENT.len();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::Level;

    fn reference_truncate(body: &[u8]) -> String {
        let text = String::from_utf8_lossy(body);
        if text.len() <= 512 {
            return text.into_owned();
        }
        let end = text.floor_char_boundary(512);
        format!("{}...", &text[..end])
    }

    fn options(tls: TlsPolicy) -> ClientOptions {
        ClientOptions {
            retry: RetryPolicy::new(3),
            tls,
            logger: Logger::new(Level::Quiet),
        }
    }

    #[test]
    fn body_excerpt_matches_reference_truncate() {
        let mut cases: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"short".to_vec(),
            vec![b'a'; 512],
            vec![b'a'; 513],
            "é".repeat(300).into_bytes(),
            "€".repeat(200).into_bytes(),
        ];
        for prefix in 505..=515 {
            let mut body = vec![b'x'; prefix];
            body.extend_from_slice(&[0xFF, 0xFE, b'z']);
            body.extend_from_slice("ü".as_bytes());
            cases.push(body);
            let mut body = vec![b'x'; prefix];
            body.extend_from_slice("€".as_bytes());
            body.extend_from_slice(&[b'y'; 10]);
            cases.push(body);
            let mut body = vec![b'x'; prefix];
            body.push(0x80);
            cases.push(body);
        }
        for body in cases {
            assert_eq!(
                BodyExcerpt(&body).to_string(),
                reference_truncate(&body),
                "len {}",
                body.len()
            );
        }
    }

    #[test]
    fn transport_error_texts_are_unchanged() {
        let cases = [
            (ureq::Error::HostNotFound, "host not found", false),
            (ureq::Error::ConnectionFailed, "connection failed", false),
            (ureq::Error::BodyStalled, "body stalled", false),
            (ureq::Error::TooManyRedirects, "too many redirects", true),
            (ureq::Error::RedirectFailed, "redirect failed", true),
            (
                ureq::Error::TlsRequired,
                "server requires TLS but transport is unsecured",
                true,
            ),
        ];
        for (err, text, connect) in cases {
            let mapped = TransportError::from(err);
            assert_eq!(mapped.to_string(), text);
            assert_eq!(mapped.is_connect(), connect, "{text}");
        }
        let io = TransportError::from(ureq::Error::Io(io::Error::other("broken pipe")));
        assert_eq!(io.to_string(), "io: broken pipe");
        assert!(!io.is_connect());
    }

    #[test]
    fn cross_host_detects_host_and_port_changes() {
        assert!(HttpCore::is_cross_host(
            "https://a.example.com/x",
            "https://b.example.com/x"
        ));
        assert!(HttpCore::is_cross_host(
            "https://a.example.com/x",
            "https://a.example.com:8443/x"
        ));
        assert!(!HttpCore::is_cross_host(
            "https://a.example.com/x",
            "https://a.example.com:443/y"
        ));
        assert!(!HttpCore::is_cross_host(
            "not a url",
            "https://a.example.com/"
        ));
    }

    #[test]
    fn core_keeps_options_and_profile() {
        let core = HttpCore::new(
            options(TlsPolicy::AcceptInvalid),
            AgentProfile {
                redirects: Redirects::Manual,
                methods: Methods::Extended,
                max_body: 1024,
            },
        );
        assert_eq!(core.max_body(), 1024);
        assert_eq!(core.tls(), TlsPolicy::AcceptInvalid);
        assert_eq!(core.retry().max_retries, 3);
        assert_eq!(core.logger().level(), Level::Quiet);
        assert!(core.user_agent().starts_with("vandelay/"));
        assert_eq!(core.stats().snapshot(), RetryStats::default());
        assert!(core.backoff(1) <= core.retry().base);
    }

    #[test]
    fn throttle_arms_the_shared_cooldown() {
        let core = HttpCore::new(
            options(TlsPolicy::Verify),
            AgentProfile {
                redirects: Redirects::Follow,
                methods: Methods::Standard,
                max_body: 1,
            },
        );
        let delay = core.throttle(Some(Duration::from_secs(5)));
        assert_eq!(delay, Duration::from_secs(5));
        assert_eq!(core.rate_limit().level(), 1);
        assert!(core.rate_limit().cooldown().remaining().is_some());
    }

    #[test]
    fn transfer_stats_count() {
        let stats = TransferStats::default();
        stats.note_request();
        stats.note_retry();
        stats.note_retry();
        stats.note_retry_after();
        assert_eq!(stats.requests(), 1);
        assert_eq!(
            stats.snapshot(),
            RetryStats {
                retries_observed: 2,
                retry_after_sleeps: 1
            }
        );
    }

    #[test]
    fn redirect_limit_is_manual_only_when_asked() {
        let follow = HttpCore::new(
            options(TlsPolicy::Verify),
            AgentProfile {
                redirects: Redirects::Follow,
                methods: Methods::Standard,
                max_body: 1,
            },
        );
        let manual = HttpCore::new(
            options(TlsPolicy::Verify),
            AgentProfile {
                redirects: Redirects::Manual,
                methods: Methods::Standard,
                max_body: 1,
            },
        );
        assert_ne!(follow.agent().config().max_redirects(), 0);
        assert_eq!(manual.agent().config().max_redirects(), 0);
    }
}
