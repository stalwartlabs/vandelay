/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use ureq::Agent;
use ureq::config::{Config, RedirectAuthHeaders};
use ureq::tls::{RootCerts, TlsConfig};
use ureq::{ResponseExt, http::Uri};

use crate::exchange_graph::error::GraphError;
use crate::exchange_graph::retry::{HttpClass, classify_http_status, is_throttled};
use crate::jmap::http::{RetryPolicy, cross_host, retry_after_header};
use crate::jmap::retry::{self, RateLimitState};
use crate::logging::{HttpCall, LEVEL_BODIES, LEVEL_DEFAULT, LEVEL_PROGRESS, Logger};

const MAX_BODY: u64 = 256 * 1024 * 1024;
const LONG_RETRY_THRESHOLD: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy)]
pub enum Accept {
    Json,
    Text,
    Binary,
}

impl Accept {
    fn header_value(self) -> &'static str {
        match self {
            Accept::Json => "application/json",
            Accept::Text => "text/plain",
            Accept::Binary => "application/octet-stream",
        }
    }
}

#[derive(Debug, Clone)]
pub struct GraphResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub content_type: Option<String>,
}

impl GraphResponse {
    pub fn as_str(&self) -> Result<&str, GraphError> {
        std::str::from_utf8(&self.body)
            .map_err(|e| GraphError::Malformed(format!("response is not utf-8: {e}")))
    }

    pub fn json(&self) -> Result<Value, GraphError> {
        serde_json::from_slice(&self.body)
            .map_err(|e| GraphError::Malformed(format!("response is not valid json: {e}")))
    }
}

struct Inner {
    agent: Agent,
    bearer: Mutex<String>,
    retry: RetryPolicy,
    rate_limit: RateLimitState,
    log_level: AtomicU8,
    retries_total: AtomicU64,
    retry_after_sleeps: AtomicU64,
    requests_total: AtomicU64,
    user_agent: String,
}

#[derive(Clone)]
pub struct GraphClient {
    inner: Arc<Inner>,
}

enum Attempt {
    Ok {
        status: u16,
        body: Vec<u8>,
        retry_after: Option<Duration>,
        content_type: Option<String>,
    },
    Transport(GraphError),
}

impl GraphClient {
    pub fn new(bearer: String, retry: RetryPolicy, allow_invalid_certs: bool) -> GraphClient {
        let config: Config = Config::builder()
            .http_status_as_error(false)
            .redirect_auth_headers(RedirectAuthHeaders::SameHost)
            .tls_config(
                TlsConfig::builder()
                    .unversioned_rustls_crypto_provider(std::sync::Arc::new(
                        rustls::crypto::aws_lc_rs::default_provider(),
                    ))
                    .root_certs(RootCerts::PlatformVerifier)
                    .disable_verification(allow_invalid_certs)
                    .build(),
            )
            .build();
        GraphClient {
            inner: Arc::new(Inner {
                agent: config.new_agent(),
                bearer: Mutex::new(bearer),
                retry,
                rate_limit: RateLimitState::new(),
                log_level: AtomicU8::new(LEVEL_DEFAULT),
                retries_total: AtomicU64::new(0),
                retry_after_sleeps: AtomicU64::new(0),
                requests_total: AtomicU64::new(0),
                user_agent: format!("vandelay/{}", env!("CARGO_PKG_VERSION")),
            }),
        }
    }

    pub fn set_bearer(&self, bearer: String) {
        let mut g = match self.inner.bearer.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        *g = bearer;
    }

    pub fn set_logger(&self, logger: Logger) {
        self.inner
            .log_level
            .store(logger.level(), Ordering::Relaxed);
    }

    fn logger(&self) -> Logger {
        Logger::new(self.inner.log_level.load(Ordering::Relaxed))
    }

    fn bearer(&self) -> String {
        match self.inner.bearer.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.bearer())
    }

    pub fn retries_observed(&self) -> u64 {
        self.inner.retries_total.load(Ordering::Relaxed)
    }

    pub fn retry_after_sleeps(&self) -> u64 {
        self.inner.retry_after_sleeps.load(Ordering::Relaxed)
    }

    pub fn requests_observed(&self) -> u64 {
        self.inner.requests_total.load(Ordering::Relaxed)
    }

    pub fn rate_limit(&self) -> &RateLimitState {
        &self.inner.rate_limit
    }

    pub fn get(&self, url: &str, accept: Accept) -> Result<GraphResponse, GraphError> {
        self.execute("GET", url, accept, &[])
    }

    pub fn get_with_prefer(
        &self,
        url: &str,
        accept: Accept,
        prefer: &[&str],
    ) -> Result<GraphResponse, GraphError> {
        self.execute("GET", url, accept, prefer)
    }

    pub fn get_json(&self, url: &str) -> Result<Value, GraphError> {
        self.get(url, Accept::Json).and_then(|r| r.json())
    }

    pub fn get_json_with_prefer(&self, url: &str, prefer: &[&str]) -> Result<Value, GraphError> {
        self.get_with_prefer(url, Accept::Json, prefer)
            .and_then(|r| r.json())
    }

    fn execute(
        &self,
        method: &str,
        url: &str,
        accept: Accept,
        extra_prefer: &[&str],
    ) -> Result<GraphResponse, GraphError> {
        let policy = self.inner.retry;
        let logger = self.logger();
        let mut attempt: u32 = 0;
        loop {
            self.inner.rate_limit.cooldown().wait();
            self.inner.requests_total.fetch_add(1, Ordering::Relaxed);
            let outcome = self.one_attempt(method, url, accept, extra_prefer);
            match outcome {
                Attempt::Ok {
                    status,
                    body,
                    retry_after,
                    content_type,
                } => {
                    let class = classify_http_status(status);
                    match class {
                        HttpClass::Success => {
                            self.inner.rate_limit.on_success();
                            return Ok(GraphResponse {
                                status,
                                body,
                                content_type,
                            });
                        }
                        HttpClass::Vanished => {
                            return Err(GraphError::Vanished);
                        }
                        HttpClass::Auth => {
                            return Err(GraphError::Auth(format!(
                                "server returned {status}: {}",
                                truncate(&body)
                            )));
                        }
                        HttpClass::Fatal => {
                            return Err(GraphError::HttpStatus {
                                status,
                                body: truncate(&body),
                            });
                        }
                        HttpClass::Retryable => {
                            attempt += 1;
                            self.inner.retries_total.fetch_add(1, Ordering::Relaxed);
                            if attempt > policy.max_retries {
                                return Err(GraphError::RetriesExhausted(format!(
                                    "{method} {url} kept returning http {status}"
                                )));
                            }
                            if retry_after.is_some() {
                                self.inner
                                    .retry_after_sleeps
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                            let delay = if is_throttled(status) {
                                self.inner.rate_limit.on_throttle(&policy, retry_after)
                            } else {
                                match retry_after {
                                    Some(d) => d,
                                    None => retry::backoff_delay(&policy, attempt),
                                }
                            };
                            if delay >= LONG_RETRY_THRESHOLD {
                                logger.warn(&format!(
                                    "graph rate-limited (http {status}); waiting {} before retry {}/{} (shared level {})",
                                    format_retry_wait(delay),
                                    attempt,
                                    policy.max_retries,
                                    self.inner.rate_limit.level(),
                                ));
                            }
                            self.log_retry(
                                &logger,
                                RetryLog {
                                    method,
                                    url,
                                    attempt,
                                    delay,
                                    status,
                                    body: &body,
                                },
                            );
                            std::thread::sleep(delay);
                        }
                    }
                }
                Attempt::Transport(err) => {
                    if matches!(err, GraphError::Connect(_)) {
                        return Err(err);
                    }
                    attempt += 1;
                    self.inner.retries_total.fetch_add(1, Ordering::Relaxed);
                    if attempt > policy.max_retries {
                        return Err(GraphError::RetriesExhausted(format!(
                            "{method} {url}: {err}"
                        )));
                    }
                    let delay = retry::backoff_delay(&policy, attempt);
                    if delay >= LONG_RETRY_THRESHOLD {
                        logger.warn(&format!(
                            "transient transport failure ({err}); waiting {} before retry {}/{}",
                            format_retry_wait(delay),
                            attempt,
                            policy.max_retries
                        ));
                    }
                    std::thread::sleep(delay);
                }
            }
        }
    }

    fn one_attempt(
        &self,
        method: &str,
        url: &str,
        accept: Accept,
        extra_prefer: &[&str],
    ) -> Attempt {
        let mut req = match method {
            "GET" => self.inner.agent.get(url),
            other => {
                return Attempt::Transport(GraphError::Connect(format!(
                    "unsupported method {other} (graph importer is read-only)"
                )));
            }
        };
        req = req
            .header("Authorization", self.auth_header())
            .header("Accept", accept.header_value())
            .header("User-Agent", self.inner.user_agent.as_str());
        req = req.header("Prefer", "IdType=\"ImmutableId\"");
        for value in extra_prefer {
            req = req.header("Prefer", *value);
        }
        let logger = self.logger();
        let started = Instant::now();
        match req.call() {
            Ok(mut resp) => {
                let status = resp.status().as_u16();
                let final_uri = resp.get_uri().clone();
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(retry_after_header);
                let content_type = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                match resp.body_mut().with_config().limit(MAX_BODY).read_to_vec() {
                    Ok(bytes) => {
                        warn_on_redirect(&logger, method, url, &final_uri);
                        logger.trace_http(&HttpCall {
                            proto: "Graph",
                            method,
                            url,
                            status,
                            elapsed: started.elapsed(),
                            note: None,
                            request: None,
                            request_type: None,
                            response: &bytes,
                            response_type: content_type.as_deref(),
                        });
                        Attempt::Ok {
                            status,
                            body: bytes,
                            retry_after,
                            content_type,
                        }
                    }
                    Err(e) => Attempt::Transport(GraphError::Transport(format!(
                        "reading response body: {e}"
                    ))),
                }
            }
            Err(e) => {
                let err = map_ureq_error(e);
                logger.trace_http_error("Graph", method, url, &err.to_string(), started.elapsed());
                Attempt::Transport(err)
            }
        }
    }

    fn log_retry(&self, logger: &Logger, info: RetryLog<'_>) {
        let RetryLog {
            method,
            url,
            attempt,
            delay,
            status,
            body,
        } = info;
        if logger.enabled(LEVEL_BODIES) {
            eprintln!(
                "retry {}/{} {} {} after {:?} (http {status}) body={}",
                attempt,
                self.inner.retry.max_retries,
                method,
                url,
                delay,
                truncate(body)
            );
        } else if logger.enabled(LEVEL_PROGRESS) {
            eprintln!(
                "retry {}/{} (http {status})",
                attempt, self.inner.retry.max_retries
            );
        }
    }
}

struct RetryLog<'a> {
    method: &'a str,
    url: &'a str,
    attempt: u32,
    delay: Duration,
    status: u16,
    body: &'a [u8],
}

fn warn_on_redirect(logger: &Logger, method: &str, requested: &str, final_uri: &Uri) {
    if requested.ends_with("/content") {
        return;
    }
    let landed = final_uri.to_string();
    if cross_host(requested, &landed) {
        let host = url::Url::parse(&landed)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_else(|| "another host".to_owned());
        logger.warn(&format!(
            "{method} {requested} was redirected across hosts to {host}; verify the Graph endpoint host is reachable directly without redirection"
        ));
    }
}

fn map_ureq_error(err: ureq::Error) -> GraphError {
    match err {
        ureq::Error::Io(e) => GraphError::Transport(format!("io: {e}")),
        ureq::Error::Timeout(t) => GraphError::Transport(format!("timeout: {t}")),
        ureq::Error::HostNotFound => GraphError::Transport("host not found".to_owned()),
        ureq::Error::ConnectionFailed => GraphError::Transport("connection failed".to_owned()),
        ureq::Error::BodyStalled => GraphError::Transport("body stalled".to_owned()),
        ureq::Error::Tls(m) => GraphError::Transport(format!("tls: {m}")),
        ureq::Error::TooManyRedirects => GraphError::Connect("too many redirects".to_owned()),
        ureq::Error::RedirectFailed => GraphError::Connect("redirect failed".to_owned()),
        ureq::Error::TlsRequired => {
            GraphError::Connect("server requires TLS but transport is unsecured".to_owned())
        }
        other => GraphError::Connect(other.to_string()),
    }
}

fn truncate(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    if text.len() <= 512 {
        return text.into_owned();
    }
    let end = text.floor_char_boundary(512);
    format!("{}...", &text[..end])
}

fn format_retry_wait(d: Duration) -> String {
    let total = d.as_secs();
    if total < 60 {
        format!("{total}s")
    } else if total < 3600 {
        format!("{}m{:02}s", total / 60, total % 60)
    } else {
        format!(
            "{}h{:02}m{:02}s",
            total / 3600,
            (total % 3600) / 60,
            total % 60
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_construct() {
        let c = GraphClient::new("token".to_owned(), RetryPolicy::new(3), false);
        assert_eq!(c.retries_observed(), 0);
        assert_eq!(c.retry_after_sleeps(), 0);
        assert_eq!(c.requests_observed(), 0);
        assert_eq!(c.auth_header(), "Bearer token");
    }

    #[test]
    fn bearer_can_be_swapped_at_runtime() {
        let c = GraphClient::new("old".to_owned(), RetryPolicy::new(0), false);
        c.set_bearer("new".to_owned());
        assert_eq!(c.auth_header(), "Bearer new");
    }

    #[test]
    fn accept_header_values_are_what_the_spec_requires() {
        assert_eq!(Accept::Json.header_value(), "application/json");
        assert_eq!(Accept::Text.header_value(), "text/plain");
    }
}
