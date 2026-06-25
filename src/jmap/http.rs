/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;
use ureq::Agent;
use ureq::config::{Config, RedirectAuthHeaders};
use ureq::tls::{RootCerts, TlsConfig};

use crate::jmap::error::JmapError;
use crate::jmap::inflight::{Permit, Semaphore};
use crate::jmap::retry::{self, Disposition, RateLimitState};
use crate::jmap::session::Limits;
use crate::logging::{LEVEL_BODIES, LEVEL_DEFAULT, LEVEL_PROGRESS, Logger};

const MAX_BODY: u64 = 512 * 1024 * 1024;

const LONG_RETRY_THRESHOLD: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub enum Auth {
    Basic { user: String, password: String },
    Digest { user: String, password: String },
    Bearer { token: String },
}

impl Auth {
    pub fn header_value(&self) -> Option<String> {
        match self {
            Auth::Basic { user, password } => Some(format!(
                "Basic {}",
                STANDARD.encode(format!("{user}:{password}"))
            )),
            Auth::Digest { .. } => None,
            Auth::Bearer { token } => Some(format!("Bearer {token}")),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub base: Duration,
    pub cap: Duration,
}

impl RetryPolicy {
    pub fn new(max_retries: u32) -> Self {
        RetryPolicy {
            max_retries,
            base: Duration::from_millis(500),
            cap: Duration::from_secs(30),
        }
    }
}

struct Inner {
    agent: Agent,
    auth: Auth,
    retry: RetryPolicy,
    allow_invalid_certs: bool,
    rate_limit: RateLimitState,
    log_level: AtomicU8,
    requests_gate: OnceLock<Semaphore>,
    uploads_gate: OnceLock<Semaphore>,
    max_upload_bytes: AtomicU64,
    retries_total: AtomicU64,
    retry_after_sleeps: AtomicU64,
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Api,
    Upload,
}

#[derive(Clone)]
pub struct HttpClient {
    inner: Arc<Inner>,
}

enum Attempt {
    Ok {
        status: u16,
        body: Vec<u8>,
        retry_after: Option<Duration>,
        rate_limit_headers: Vec<(String, String)>,
    },
    Transport(JmapError),
}

impl HttpClient {
    pub fn new(auth: Auth, retry: RetryPolicy, allow_invalid_certs: bool) -> Self {
        assert!(
            !matches!(&auth, Auth::Digest { .. }),
            "Digest auth is only supported for DAV connections"
        );
        let config: Config = Config::builder()
            .http_status_as_error(false)
            .redirect_auth_headers(RedirectAuthHeaders::SameHost)
            .tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::PlatformVerifier)
                    .disable_verification(allow_invalid_certs)
                    .build(),
            )
            .build();
        HttpClient {
            inner: Arc::new(Inner {
                agent: config.new_agent(),
                auth,
                retry,
                allow_invalid_certs,
                rate_limit: RateLimitState::new(),
                log_level: AtomicU8::new(LEVEL_DEFAULT),
                requests_gate: OnceLock::new(),
                uploads_gate: OnceLock::new(),
                max_upload_bytes: AtomicU64::new(0),
                retries_total: AtomicU64::new(0),
                retry_after_sleeps: AtomicU64::new(0),
            }),
        }
    }

    pub fn set_limits(&self, limits: &Limits) {
        let _ = self
            .inner
            .requests_gate
            .set(Semaphore::new(limits.max_concurrent_requests));
        let _ = self
            .inner
            .uploads_gate
            .set(Semaphore::new(limits.max_concurrent_upload));
        self.inner
            .max_upload_bytes
            .store(limits.max_size_upload, Ordering::Relaxed);
    }

    pub fn retries_observed(&self) -> u64 {
        self.inner.retries_total.load(Ordering::Relaxed)
    }

    pub fn retry_after_sleeps(&self) -> u64 {
        self.inner.retry_after_sleeps.load(Ordering::Relaxed)
    }

    fn acquire_requests(&self) -> Option<Permit<'_>> {
        self.inner.requests_gate.get().map(Semaphore::acquire)
    }

    fn acquire_uploads(&self) -> Option<Permit<'_>> {
        self.inner.uploads_gate.get().map(Semaphore::acquire)
    }

    pub fn auth(&self) -> &Auth {
        &self.inner.auth
    }

    pub fn retry(&self) -> &RetryPolicy {
        &self.inner.retry
    }

    pub fn allow_invalid_certs(&self) -> bool {
        self.inner.allow_invalid_certs
    }

    pub fn rate_limit(&self) -> &RateLimitState {
        &self.inner.rate_limit
    }

    pub fn throttle_level(&self) -> u32 {
        self.inner.rate_limit.level()
    }

    pub fn set_logger(&self, logger: Logger) {
        self.inner
            .log_level
            .store(logger.level(), Ordering::Relaxed);
    }

    fn logger(&self) -> Logger {
        Logger::new(self.inner.log_level.load(Ordering::Relaxed))
    }

    pub fn get(&self, url: &str) -> Result<String, JmapError> {
        let body = self.execute(Kind::Api, "GET", url, None, None)?;
        String::from_utf8(body).map_err(|e| JmapError::Malformed(format!("non-utf8 body: {e}")))
    }

    pub fn post_json(&self, url: &str, body: &Value) -> Result<Value, JmapError> {
        let payload = serde_json::to_vec(body)?;
        let raw = self.execute(
            Kind::Api,
            "POST",
            url,
            Some(&payload),
            Some("application/json"),
        )?;
        serde_json::from_slice(&raw)
            .map_err(|e| JmapError::Malformed(format!("response is not valid json: {e}")))
    }

    pub fn upload(
        &self,
        upload_url: &str,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<Value, JmapError> {
        let cap = self.inner.max_upload_bytes.load(Ordering::Relaxed);
        if cap > 0 && (bytes.len() as u64) > cap {
            return Err(JmapError::SingleObjectTooLarge(format!(
                "blob upload of {} bytes exceeds maxSizeUpload ({cap})",
                bytes.len()
            )));
        }
        let raw = self.execute(
            Kind::Upload,
            "POST",
            upload_url,
            Some(bytes),
            Some(content_type),
        )?;
        serde_json::from_slice(&raw)
            .map_err(|e| JmapError::Malformed(format!("upload response is not valid json: {e}")))
    }

    pub fn download(&self, url: &str) -> Result<Vec<u8>, JmapError> {
        self.execute(Kind::Api, "GET", url, None, None)
    }

    fn execute(
        &self,
        kind: Kind,
        method: &str,
        url: &str,
        body: Option<&[u8]>,
        content_type: Option<&str>,
    ) -> Result<Vec<u8>, JmapError> {
        let policy = self.inner.retry;
        let logger = self.logger();
        let mut attempt: u32 = 0;
        loop {
            self.inner.rate_limit.cooldown().wait();
            let attempt_outcome = {
                let _r = self.acquire_requests();
                let _u = if matches!(kind, Kind::Upload) {
                    self.acquire_uploads()
                } else {
                    None
                };
                self.one_attempt(url, body, content_type)
            };
            match attempt_outcome {
                Attempt::Ok { status, body, .. } if (200..300).contains(&status) => {
                    self.inner.rate_limit.on_success();
                    return Ok(body);
                }
                Attempt::Ok {
                    status,
                    body,
                    retry_after,
                    rate_limit_headers,
                } => {
                    let disposition = self.status_disposition(status, &body);
                    match disposition {
                        StatusOutcome::Auth => {
                            return Err(JmapError::Auth(format!(
                                "server returned {status}: {}",
                                truncate(&body)
                            )));
                        }
                        StatusOutcome::RequestTooLarge => {
                            return Err(JmapError::RequestTooLarge);
                        }
                        StatusOutcome::Fatal => {
                            return Err(JmapError::HttpStatus {
                                status,
                                body: truncate(&body),
                            });
                        }
                        StatusOutcome::Retryable => {
                            attempt += 1;
                            self.inner.retries_total.fetch_add(1, Ordering::Relaxed);
                            let detail = problem_detail(&body);
                            let headers_blurb = format_rate_headers(&rate_limit_headers);
                            let reason = match (&detail, headers_blurb.as_deref()) {
                                (Some(d), Some(h)) => format!("http {status}: {d} [{h}]"),
                                (Some(d), None) => format!("http {status}: {d}"),
                                (None, Some(h)) => format!("http {status} [{h}]"),
                                (None, None) => format!("http {status}"),
                            };
                            if attempt > policy.max_retries {
                                return Err(JmapError::RetriesExhausted(format!(
                                    "{method} {url} kept returning {reason}"
                                )));
                            }
                            if retry_after.is_some() {
                                self.inner
                                    .retry_after_sleeps
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                            let delay = self.inner.rate_limit.on_throttle(&policy, retry_after);
                            if delay >= LONG_RETRY_THRESHOLD {
                                logger.warn(&format!(
                                    "server rate-limited ({reason}); waiting {} before retry {}/{} (shared level {})",
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
                                    reason: &reason,
                                    body: &body,
                                },
                            );
                            std::thread::sleep(delay);
                        }
                    }
                }
                Attempt::Transport(err) => match transport_disposition(&err) {
                    Disposition::Fatal => return Err(err),
                    Disposition::Retryable => {
                        attempt += 1;
                        self.inner.retries_total.fetch_add(1, Ordering::Relaxed);
                        if attempt > policy.max_retries {
                            return Err(JmapError::RetriesExhausted(format!(
                                "{method} {url}: {err}"
                            )));
                        }
                        let delay = retry::backoff_delay(&policy, attempt);
                        if delay >= LONG_RETRY_THRESHOLD {
                            logger.warn(&format!(
                                "transient transport failure ({err}); waiting {} before retry {}/{}",
                                format_retry_wait(delay),
                                attempt,
                                policy.max_retries,
                            ));
                        }
                        self.log_retry(
                            &logger,
                            RetryLog {
                                method,
                                url,
                                attempt,
                                delay,
                                reason: &err.to_string(),
                                body: &[],
                            },
                        );
                        std::thread::sleep(delay);
                    }
                },
            }
        }
    }

    fn one_attempt(&self, url: &str, body: Option<&[u8]>, content_type: Option<&str>) -> Attempt {
        let auth = self
            .inner
            .auth
            .header_value()
            .expect("Digest auth is only supported for DAV connections");
        let result = if let Some(payload) = body {
            let mut req = self
                .inner
                .agent
                .post(url)
                .header("Authorization", auth)
                .header("Accept", "application/json");
            if let Some(ct) = content_type {
                req = req.header("Content-Type", ct);
            }
            req.send(payload)
        } else {
            self.inner
                .agent
                .get(url)
                .header("Authorization", auth)
                .header("Accept", "application/json")
                .call()
        };
        match result {
            Ok(mut resp) => {
                let status = resp.status().as_u16();
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(retry_after_header);
                let rate_limit_headers: Vec<(String, String)> = resp
                    .headers()
                    .iter()
                    .filter_map(|(name, value)| {
                        let lower = name.as_str().to_ascii_lowercase();
                        if matches!(
                            lower.as_str(),
                            "retry-after" | "ratelimit" | "ratelimit-policy" | "content-type"
                        ) {
                            value.to_str().ok().map(|v| (lower, v.to_owned()))
                        } else {
                            None
                        }
                    })
                    .collect();
                match resp.body_mut().with_config().limit(MAX_BODY).read_to_vec() {
                    Ok(bytes) => Attempt::Ok {
                        status,
                        body: bytes,
                        retry_after,
                        rate_limit_headers,
                    },
                    Err(e) => Attempt::Transport(JmapError::Transport(format!(
                        "reading response body: {e}"
                    ))),
                }
            }
            Err(e) => Attempt::Transport(map_ureq_error(e)),
        }
    }

    fn status_disposition(&self, status: u16, body: &[u8]) -> StatusOutcome {
        if status == 401 || status == 403 {
            return StatusOutcome::Auth;
        }
        if status == 413 {
            return StatusOutcome::RequestTooLarge;
        }
        if let Ok(problem) = serde_json::from_slice::<Value>(body)
            && let Some(kind) = problem.get("type").and_then(Value::as_str)
        {
            if kind.ends_with(":requestTooLarge") {
                return StatusOutcome::RequestTooLarge;
            }
            if kind.ends_with(":limit") {
                let which = problem.get("limit").and_then(Value::as_str).unwrap_or("");
                let subtype = problem.get("subType").and_then(Value::as_str).unwrap_or("");
                match which {
                    "maxSizeRequest" | "maxSizeUpload" => {
                        return StatusOutcome::RequestTooLarge;
                    }
                    "maxConcurrentRequests" | "maxConcurrentUpload" => {
                        return StatusOutcome::Retryable;
                    }
                    _ if subtype == "rateLimit" => {
                        return StatusOutcome::Retryable;
                    }
                    _ => {}
                }
            }
        }
        match retry::classify_http_status(status) {
            Disposition::Retryable => StatusOutcome::Retryable,
            Disposition::Fatal => StatusOutcome::Fatal,
        }
    }

    fn log_retry(&self, logger: &Logger, r: RetryLog<'_>) {
        if logger.enabled(LEVEL_BODIES) {
            let snippet = if r.body.is_empty() {
                String::new()
            } else {
                format!(" body={}", truncate(r.body))
            };
            eprintln!(
                "retry {}/{} {} {} after {:?} ({}){snippet}",
                r.attempt, self.inner.retry.max_retries, r.method, r.url, r.delay, r.reason,
            );
        } else if logger.enabled(LEVEL_PROGRESS) {
            eprintln!(
                "retry {}/{} ({})",
                r.attempt, self.inner.retry.max_retries, r.reason
            );
        }
    }
}

enum StatusOutcome {
    Retryable,
    Fatal,
    Auth,
    RequestTooLarge,
}

struct RetryLog<'a> {
    method: &'a str,
    url: &'a str,
    attempt: u32,
    delay: Duration,
    reason: &'a str,
    body: &'a [u8],
}

fn transport_disposition(err: &JmapError) -> Disposition {
    match err {
        JmapError::Connect(_) => Disposition::Fatal,
        _ => Disposition::Retryable,
    }
}

fn map_ureq_error(err: ureq::Error) -> JmapError {
    match err {
        ureq::Error::Io(e) => JmapError::Transport(format!("io: {e}")),
        ureq::Error::Timeout(t) => JmapError::Transport(format!("timeout: {t}")),
        ureq::Error::HostNotFound => JmapError::Transport("host not found".to_owned()),
        ureq::Error::ConnectionFailed => JmapError::Transport("connection failed".to_owned()),
        ureq::Error::BodyStalled => JmapError::Transport("body stalled".to_owned()),
        ureq::Error::Tls(m) => JmapError::Transport(format!("tls: {m}")),
        ureq::Error::TooManyRedirects => JmapError::Connect("too many redirects".to_owned()),
        ureq::Error::RedirectFailed => JmapError::Connect("redirect failed".to_owned()),
        ureq::Error::TlsRequired => {
            JmapError::Connect("server requires TLS but transport is unsecured".to_owned())
        }
        other => JmapError::Connect(other.to_string()),
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

pub fn retry_after_header(value: &str) -> Option<Duration> {
    retry::parse_retry_after(value, SystemTime::now())
}

fn problem_detail(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let title = value.get("title").and_then(Value::as_str);
    let detail = value.get("detail").and_then(Value::as_str);
    match (title, detail) {
        (Some(t), Some(d)) => Some(format!("{t}: {d}")),
        (Some(t), None) => Some(t.to_owned()),
        (None, Some(d)) => Some(d.to_owned()),
        (None, None) => None,
    }
}

fn format_rate_headers(headers: &[(String, String)]) -> Option<String> {
    if headers.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = headers
        .iter()
        .filter(|(name, _)| name != "content-type")
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    if parts.is_empty() {
        return None;
    }
    parts.sort();
    Some(parts.join("; "))
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
    fn basic_header_matches_rfc7617_example() {
        let auth = Auth::Basic {
            user: "Aladdin".to_owned(),
            password: "open sesame".to_owned(),
        };
        assert_eq!(
            auth.header_value().as_deref(),
            Some("Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==")
        );
    }

    #[test]
    fn bearer_header() {
        let auth = Auth::Bearer {
            token: "abc.def".to_owned(),
        };
        assert_eq!(auth.header_value().as_deref(), Some("Bearer abc.def"));
    }

    #[test]
    fn digest_header_is_deferred() {
        let auth = Auth::Digest {
            user: "alice".to_owned(),
            password: "pw".to_owned(),
        };
        assert_eq!(auth.header_value(), None);
    }

    #[test]
    #[should_panic(expected = "Digest auth is only supported for DAV connections")]
    fn digest_auth_is_rejected_for_http_client() {
        let _ = HttpClient::new(
            Auth::Digest {
                user: "alice".to_owned(),
                password: "pw".to_owned(),
            },
            RetryPolicy::new(0),
            false,
        );
    }

    #[test]
    fn truncate_does_not_panic_on_multibyte_boundary() {
        let mut body = vec![b'a'; 510];
        body.extend_from_slice("\u{1F4A9}".as_bytes());
        body.extend_from_slice(&[b'b'; 600]);
        let s = truncate(&body);
        assert!(s.ends_with("..."));
        assert!(s.is_char_boundary(s.len()));
        let inner = s.trim_end_matches('.');
        assert!(inner.is_char_boundary(inner.len()));
    }

    #[test]
    fn truncate_short_body_returns_verbatim() {
        let s = truncate("héllo".as_bytes());
        assert_eq!(s, "héllo");
    }

    #[test]
    fn limit_problem_unknown_kind_on_429_is_retried() {
        let client = HttpClient::new(
            Auth::Bearer {
                token: "t".to_owned(),
            },
            RetryPolicy::new(0),
            false,
        );
        let body = br#"{"type":"urn:ietf:params:jmap:error:limit","limit":"someServerLimit"}"#;
        assert!(matches!(
            client.status_disposition(429, body),
            StatusOutcome::Retryable
        ));
    }

    #[test]
    fn limit_problem_max_size_request_is_resplit() {
        let client = HttpClient::new(
            Auth::Bearer {
                token: "t".to_owned(),
            },
            RetryPolicy::new(0),
            false,
        );
        let body = br#"{"type":"urn:ietf:params:jmap:error:limit","limit":"maxSizeRequest"}"#;
        assert!(matches!(
            client.status_disposition(400, body),
            StatusOutcome::RequestTooLarge
        ));
    }

    #[test]
    fn limit_problem_max_concurrent_is_retried() {
        let client = HttpClient::new(
            Auth::Bearer {
                token: "t".to_owned(),
            },
            RetryPolicy::new(0),
            false,
        );
        let body =
            br#"{"type":"urn:ietf:params:jmap:error:limit","limit":"maxConcurrentRequests"}"#;
        assert!(matches!(
            client.status_disposition(429, body),
            StatusOutcome::Retryable
        ));
    }

    fn limits_with(max_upload: u64, req: u64, up: u64) -> Limits {
        Limits {
            max_objects_in_get: 500,
            max_objects_in_set: 500,
            max_calls_in_request: 16,
            max_concurrent_requests: req,
            max_concurrent_upload: up,
            max_size_request: 10_000_000,
            max_size_upload: max_upload,
        }
    }

    #[test]
    fn upload_rejects_oversize_before_sending() {
        let client = HttpClient::new(
            Auth::Bearer {
                token: "t".to_owned(),
            },
            RetryPolicy::new(0),
            false,
        );
        client.set_limits(&limits_with(10, 4, 4));
        let err = client
            .upload(
                "http://127.0.0.1:1/upload",
                "application/octet-stream",
                &[0u8; 11],
            )
            .unwrap_err();
        assert!(
            matches!(err, JmapError::SingleObjectTooLarge(_)),
            "got {err:?}"
        );
        assert_eq!(client.retries_observed(), 0);
    }

    #[test]
    fn upload_within_max_size_is_not_short_circuited() {
        let client = HttpClient::new(
            Auth::Bearer {
                token: "t".to_owned(),
            },
            RetryPolicy::new(0),
            false,
        );
        client.set_limits(&limits_with(1024, 4, 4));
        let err = client
            .upload(
                "http://127.0.0.1:1/upload",
                "application/octet-stream",
                b"abc",
            )
            .unwrap_err();
        assert!(
            !matches!(err, JmapError::SingleObjectTooLarge(_)),
            "expected the call to proceed to transport (got SingleObjectTooLarge): {err:?}"
        );
    }

    #[test]
    fn problem_detail_extracts_title_and_detail() {
        let body = br#"{"type":"about:blank","status":429,"title":"Quota exceeded","detail":"You have exceeded the blob upload quota of 1000 files or 50000000 bytes."}"#;
        let got = problem_detail(body).expect("title+detail");
        assert!(got.contains("Quota exceeded"));
        assert!(got.contains("50000000 bytes"));
    }

    #[test]
    fn problem_detail_title_only() {
        let body = br#"{"title":"Too Many Requests"}"#;
        assert_eq!(problem_detail(body).as_deref(), Some("Too Many Requests"));
    }

    #[test]
    fn problem_detail_returns_none_for_empty_body() {
        assert_eq!(problem_detail(b"{}"), None);
        assert_eq!(problem_detail(b"not json"), None);
    }

    #[test]
    fn format_retry_wait_short_medium_long() {
        assert_eq!(format_retry_wait(Duration::from_secs(7)), "7s");
        assert_eq!(format_retry_wait(Duration::from_secs(67)), "1m07s");
        assert_eq!(format_retry_wait(Duration::from_secs(1427)), "23m47s");
        assert_eq!(format_retry_wait(Duration::from_secs(3661)), "1h01m01s");
    }

    #[test]
    fn counters_start_at_zero() {
        let client = HttpClient::new(
            Auth::Bearer {
                token: "t".to_owned(),
            },
            RetryPolicy::new(0),
            false,
        );
        assert_eq!(client.retries_observed(), 0);
        assert_eq!(client.retry_after_sleeps(), 0);
    }
}
