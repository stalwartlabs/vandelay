/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use ureq::ResponseExt;
use ureq::http::{HeaderMap, Uri};

use crate::jmap::error::JmapError;
use crate::jmap::inflight::{Permit, Semaphore};
use crate::jmap::session::Limits;
use crate::logging::{HttpCall, Level, Logger};
use crate::net::auth::Auth;
use crate::net::http::{
    AgentProfile, BodyExcerpt, ClientOptions, HttpCore, Methods, Redirects, RetryStats,
    TransportError,
};
use crate::net::retry::{Disposition, RateLimit, RetryAfter, RetryPolicy, RetryWait};

const PROFILE: AgentProfile = AgentProfile {
    redirects: Redirects::Follow,
    methods: Methods::Standard,
    max_body: 512 * 1024 * 1024,
};

const LONG_RETRY_THRESHOLD: Duration = Duration::from_secs(10);

const STATUS_UNAUTHORIZED: u16 = 401;
const STATUS_FORBIDDEN: u16 = 403;
const STATUS_PAYLOAD_TOO_LARGE: u16 = 413;
const PROTO: &str = "JMAP";

struct Gates {
    requests: Semaphore,
    uploads: Semaphore,
}

struct Inner {
    core: HttpCore,
    auth: Auth,
    gates: OnceLock<Gates>,
    max_upload_bytes: AtomicU64,
}

#[derive(Debug, Clone, Copy)]
enum Lane {
    Api,
    Upload,
}

#[derive(Clone)]
pub struct JmapClient {
    inner: Arc<Inner>,
}

enum Attempt {
    Response {
        status: u16,
        body: Vec<u8>,
        retry_after: Option<Duration>,
        rate_headers: RateHeaders,
    },
    Transport(JmapError),
}

struct Exchange<'a> {
    method: &'a str,
    url: &'a str,
    body: Option<&'a [u8]>,
    content_type: Option<&'a str>,
}

impl JmapClient {
    pub fn new(auth: Auth, options: ClientOptions) -> JmapClient {
        JmapClient {
            inner: Arc::new(Inner {
                core: HttpCore::new(options, PROFILE),
                auth,
                gates: OnceLock::new(),
                max_upload_bytes: AtomicU64::new(0),
            }),
        }
    }

    pub fn set_limits(&self, limits: &Limits) {
        let _ = self.inner.gates.set(Gates {
            requests: Semaphore::new(limits.max_concurrent_requests),
            uploads: Semaphore::new(limits.max_concurrent_upload),
        });
        self.inner
            .max_upload_bytes
            .store(limits.max_size_upload, Ordering::Relaxed);
    }

    pub fn stats(&self) -> RetryStats {
        self.inner.core.stats().snapshot()
    }

    pub fn auth(&self) -> &Auth {
        &self.inner.auth
    }

    pub fn retry(&self) -> &RetryPolicy {
        self.inner.core.retry()
    }

    pub fn rate_limit(&self) -> &RateLimit {
        self.inner.core.rate_limit()
    }

    pub fn logger(&self) -> Logger {
        self.inner.core.logger()
    }

    pub fn backoff(&self, attempt: u32) -> Duration {
        self.inner.core.backoff(attempt)
    }

    pub fn get(&self, url: &str) -> Result<String, JmapError> {
        let body = self.execute(
            Lane::Api,
            &Exchange {
                method: "GET",
                url,
                body: None,
                content_type: None,
            },
        )?;
        String::from_utf8(body).map_err(|e| JmapError::Malformed(format!("non-utf8 body: {e}")))
    }

    pub fn post_json(&self, url: &str, body: &Value) -> Result<Value, JmapError> {
        self.post_payload(url, &serde_json::to_vec(body)?)
    }

    pub fn post_payload(&self, url: &str, payload: &[u8]) -> Result<Value, JmapError> {
        let raw = self.execute(
            Lane::Api,
            &Exchange {
                method: "POST",
                url,
                body: Some(payload),
                content_type: Some("application/json"),
            },
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
        if cap > 0 && u64::try_from(bytes.len()).unwrap_or(u64::MAX) > cap {
            return Err(JmapError::SingleObjectTooLarge(format!(
                "blob upload of {} bytes exceeds maxSizeUpload ({cap})",
                bytes.len()
            )));
        }
        let raw = self.execute(
            Lane::Upload,
            &Exchange {
                method: "POST",
                url: upload_url,
                body: Some(bytes),
                content_type: Some(content_type),
            },
        )?;
        serde_json::from_slice(&raw)
            .map_err(|e| JmapError::Malformed(format!("upload response is not valid json: {e}")))
    }

    pub fn download(&self, url: &str) -> Result<Vec<u8>, JmapError> {
        self.execute(
            Lane::Api,
            &Exchange {
                method: "GET",
                url,
                body: None,
                content_type: None,
            },
        )
    }

    fn permits(&self, lane: Lane) -> (Option<Permit<'_>>, Option<Permit<'_>>) {
        let Some(gates) = self.inner.gates.get() else {
            return (None, None);
        };
        let uploads = match lane {
            Lane::Api => None,
            Lane::Upload => Some(gates.uploads.acquire()),
        };
        (Some(gates.requests.acquire()), uploads)
    }

    fn execute(&self, lane: Lane, exchange: &Exchange<'_>) -> Result<Vec<u8>, JmapError> {
        let core = &self.inner.core;
        let max_retries = core.retry().max_retries;
        let logger = core.logger();
        let mut attempt: u32 = 0;
        loop {
            core.rate_limit().cooldown().wait();
            let outcome = {
                let _permits = self.permits(lane);
                self.one_attempt(exchange)
            };
            match outcome {
                Attempt::Response { status, body, .. } if (200..300).contains(&status) => {
                    core.rate_limit().on_success();
                    return Ok(body);
                }
                Attempt::Response {
                    status,
                    body,
                    retry_after,
                    rate_headers,
                } => {
                    let problem = serde_json::from_slice::<Value>(&body).ok();
                    match StatusOutcome::classify(status, problem.as_ref()) {
                        StatusOutcome::Auth => {
                            return Err(JmapError::Auth(format!(
                                "server returned {status}: {}",
                                BodyExcerpt(&body)
                            )));
                        }
                        StatusOutcome::RequestTooLarge => return Err(JmapError::RequestTooLarge),
                        StatusOutcome::Fatal => {
                            return Err(JmapError::HttpStatus {
                                status,
                                body: BodyExcerpt(&body).to_string(),
                            });
                        }
                        StatusOutcome::Retryable => {}
                    }
                    attempt += 1;
                    core.stats().note_retry();
                    let reason = RetryReason {
                        status,
                        problem: problem.as_ref(),
                        rate_headers: &rate_headers,
                    }
                    .to_string();
                    if attempt > max_retries {
                        return Err(JmapError::RetriesExhausted(format!(
                            "{} {} kept returning {reason}",
                            exchange.method, exchange.url
                        )));
                    }
                    if retry_after.is_some() {
                        core.stats().note_retry_after();
                    }
                    let delay = core.throttle(retry_after);
                    if delay >= LONG_RETRY_THRESHOLD {
                        logger.warn(format_args!(
                            "server rate-limited ({reason}); waiting {} before retry {attempt}/{max_retries} (shared level {})",
                            RetryWait(delay),
                            core.rate_limit().level(),
                        ));
                    }
                    self.log_retry(exchange, attempt, delay, &reason, &body);
                    thread::sleep(delay);
                }
                Attempt::Transport(err) => {
                    if let JmapError::Connect(_) = err {
                        return Err(err);
                    }
                    attempt += 1;
                    core.stats().note_retry();
                    if attempt > max_retries {
                        return Err(JmapError::RetriesExhausted(format!(
                            "{} {}: {err}",
                            exchange.method, exchange.url
                        )));
                    }
                    let delay = core.backoff(attempt);
                    if delay >= LONG_RETRY_THRESHOLD {
                        logger.warn(format_args!(
                            "transient transport failure ({err}); waiting {} before retry {attempt}/{max_retries}",
                            RetryWait(delay),
                        ));
                    }
                    self.log_retry(exchange, attempt, delay, &err.to_string(), &[]);
                    thread::sleep(delay);
                }
            }
        }
    }

    fn one_attempt(&self, exchange: &Exchange<'_>) -> Attempt {
        let core = &self.inner.core;
        let auth = self.inner.auth.header_value();
        let logger = core.logger();
        let started = Instant::now();
        let result = match exchange.body {
            Some(payload) => {
                let mut req = core
                    .agent()
                    .post(exchange.url)
                    .header("Authorization", auth)
                    .header("Accept", "application/json");
                if let Some(ct) = exchange.content_type {
                    req = req.header("Content-Type", ct);
                }
                req.send(payload)
            }
            None => core
                .agent()
                .get(exchange.url)
                .header("Authorization", auth)
                .header("Accept", "application/json")
                .call(),
        };
        let mut resp = match result {
            Ok(resp) => resp,
            Err(e) => {
                let err = JmapError::from(TransportError::from(e));
                logger.trace_http_error(
                    PROTO,
                    exchange.method,
                    exchange.url,
                    &err.to_string(),
                    started.elapsed(),
                );
                return Attempt::Transport(err);
            }
        };
        let status = resp.status().as_u16();
        let final_uri = resp.get_uri().clone();
        let headers = resp.headers();
        let response_type = logger
            .enabled(Level::Bodies)
            .then(|| {
                headers
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned)
            })
            .flatten();
        let retry_after = headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(RetryAfter::parse_now);
        let rate_headers = if (200..300).contains(&status) {
            RateHeaders::default()
        } else {
            RateHeaders::collect(headers)
        };
        match resp
            .body_mut()
            .with_config()
            .limit(PROFILE.max_body)
            .read_to_vec()
        {
            Ok(bytes) => {
                self.warn_on_redirect(exchange, &final_uri);
                logger.trace_http(&HttpCall {
                    proto: PROTO,
                    method: exchange.method,
                    url: exchange.url,
                    status,
                    elapsed: started.elapsed(),
                    note: None,
                    request: exchange.body,
                    request_type: exchange.content_type,
                    response: &bytes,
                    response_type: response_type.as_deref(),
                });
                Attempt::Response {
                    status,
                    body: bytes,
                    retry_after,
                    rate_headers,
                }
            }
            Err(e) => {
                Attempt::Transport(JmapError::Transport(format!("reading response body: {e}")))
            }
        }
    }

    fn warn_on_redirect(&self, exchange: &Exchange<'_>, final_uri: &Uri) {
        if final_uri == exchange.url {
            return;
        }
        let landed = final_uri.to_string();
        if HttpCore::is_cross_host(exchange.url, &landed) {
            self.logger().warn(format_args!(
                "{} {} was redirected across hosts to {landed}; a \
                JMAP endpoint must not redirect: verify the server's advertised base \
                URL and that the request host matches its configured hostname",
                exchange.method, exchange.url
            ));
        }
    }

    fn log_retry(
        &self,
        exchange: &Exchange<'_>,
        attempt: u32,
        delay: Duration,
        reason: &str,
        body: &[u8],
    ) {
        let logger = self.logger();
        let max_retries = self.retry().max_retries;
        if logger.enabled(Level::Bodies) {
            let (sep, excerpt) = if body.is_empty() {
                ("", BodyExcerpt(&[]))
            } else {
                (" body=", BodyExcerpt(body))
            };
            logger.info(format_args!(
                "retry {attempt}/{max_retries} {} {} after {delay:?} ({reason}){sep}{excerpt}",
                exchange.method, exchange.url,
            ));
        } else {
            logger.progress(format_args!("retry {attempt}/{max_retries} ({reason})"));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusOutcome {
    Retryable,
    Fatal,
    Auth,
    RequestTooLarge,
}

impl StatusOutcome {
    fn classify(status: u16, problem: Option<&Value>) -> StatusOutcome {
        if status == STATUS_UNAUTHORIZED || status == STATUS_FORBIDDEN {
            return StatusOutcome::Auth;
        }
        if status == STATUS_PAYLOAD_TOO_LARGE {
            return StatusOutcome::RequestTooLarge;
        }
        if let Some(problem) = problem
            && let Some(kind) = problem.get("type").and_then(Value::as_str)
        {
            if kind.ends_with(":requestTooLarge") {
                return StatusOutcome::RequestTooLarge;
            }
            if kind.ends_with(":limit") {
                let which = problem
                    .get("limit")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some(outcome) = hashify::map!(
                    which.as_bytes(),
                    StatusOutcome,
                    "maxSizeRequest" => StatusOutcome::RequestTooLarge,
                    "maxSizeUpload" => StatusOutcome::RequestTooLarge,
                    "maxConcurrentRequests" => StatusOutcome::Retryable,
                    "maxConcurrentUpload" => StatusOutcome::Retryable,
                ) {
                    return *outcome;
                }
                if problem.get("subType").and_then(Value::as_str) == Some("rateLimit") {
                    return StatusOutcome::Retryable;
                }
            }
        }
        match Disposition::of_status(status) {
            Disposition::Retry => StatusOutcome::Retryable,
            Disposition::Fatal => StatusOutcome::Fatal,
        }
    }
}

#[derive(Default)]
struct RateHeaders(Vec<String>);

impl RateHeaders {
    fn collect(headers: &HeaderMap) -> RateHeaders {
        let mut pairs: Vec<String> = headers
            .iter()
            .filter(|(name, _)| {
                hashify::set!(
                    name.as_str().as_bytes(),
                    "retry-after",
                    "ratelimit",
                    "ratelimit-policy",
                )
            })
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| format!("{}={value}", name.as_str()))
            })
            .collect();
        pairs.sort_unstable();
        RateHeaders(pairs)
    }
}

impl fmt::Display for RateHeaders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = self.0.iter();
        if let Some(first) = parts.next() {
            f.write_str(first)?;
        }
        for part in parts {
            f.write_str("; ")?;
            f.write_str(part)?;
        }
        Ok(())
    }
}

struct ProblemDetail<'a>(&'a Value);

impl ProblemDetail<'_> {
    fn of(problem: Option<&Value>) -> Option<ProblemDetail<'_>> {
        let problem = problem?;
        (problem.get("title").and_then(Value::as_str).is_some()
            || problem.get("detail").and_then(Value::as_str).is_some())
        .then_some(ProblemDetail(problem))
    }
}

impl fmt::Display for ProblemDetail<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let title = self.0.get("title").and_then(Value::as_str);
        let detail = self.0.get("detail").and_then(Value::as_str);
        match (title, detail) {
            (Some(t), Some(d)) => write!(f, "{t}: {d}"),
            (Some(t), None) => f.write_str(t),
            (None, Some(d)) => f.write_str(d),
            (None, None) => Ok(()),
        }
    }
}

struct RetryReason<'a> {
    status: u16,
    problem: Option<&'a Value>,
    rate_headers: &'a RateHeaders,
}

impl fmt::Display for RetryReason<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "http {}", self.status)?;
        if let Some(detail) = ProblemDetail::of(self.problem) {
            write!(f, ": {detail}")?;
        }
        if !self.rate_headers.0.is_empty() {
            write!(f, " [{}]", self.rate_headers)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::Level;
    use crate::net::http::TlsPolicy;

    fn client(retry: RetryPolicy) -> JmapClient {
        JmapClient::new(
            Auth::Bearer {
                token: "t".to_owned(),
            },
            ClientOptions {
                retry,
                tls: TlsPolicy::Verify,
                logger: Logger::new(Level::Default),
            },
        )
    }

    #[test]
    fn basic_header_matches_rfc7617_example() {
        let auth = Auth::Basic {
            user: "Aladdin".to_owned(),
            password: "open sesame".to_owned(),
        };
        assert_eq!(auth.header_value(), "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
    }

    #[test]
    fn bearer_header() {
        let auth = Auth::Bearer {
            token: "abc.def".to_owned(),
        };
        assert_eq!(auth.header_value(), "Bearer abc.def");
    }

    // A server that accepts the connection but never answers fails the request instead of hanging.
    #[test]
    fn silent_server_times_out() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/jmap/session", listener.local_addr().unwrap());
        let holder = std::thread::spawn(move || listener.accept().map(|(stream, _)| stream));
        let mut retry = RetryPolicy::new(0);
        retry.io_timeout = Duration::from_millis(200);
        let client = client(retry);
        let started = Instant::now();
        let err = client.get(&url).unwrap_err();
        assert!(
            matches!(&err, JmapError::RetriesExhausted(m) if m.contains("timeout")),
            "got {err:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        drop(holder.join().unwrap());
    }

    #[test]
    fn cross_host_detects_hostname_change() {
        assert!(HttpCore::is_cross_host(
            "https://mail.example.com/jmap",
            "https://login.example.net/sso"
        ));
    }

    #[test]
    fn cross_host_ignores_same_host_path_change() {
        assert!(!HttpCore::is_cross_host(
            "https://mail.example.com/jmap",
            "https://mail.example.com/jmap/api"
        ));
    }

    #[test]
    fn cross_host_flags_port_change() {
        assert!(HttpCore::is_cross_host(
            "https://mail.example.com/jmap",
            "https://mail.example.com:8443/jmap"
        ));
    }

    #[test]
    fn limit_problem_unknown_kind_on_429_is_retried() {
        let body = br#"{"type":"urn:ietf:params:jmap:error:limit","limit":"someServerLimit"}"#;
        assert!(matches!(
            StatusOutcome::classify(429, serde_json::from_slice(body).ok().as_ref()),
            StatusOutcome::Retryable
        ));
    }

    #[test]
    fn limit_problem_max_size_request_is_resplit() {
        let body = br#"{"type":"urn:ietf:params:jmap:error:limit","limit":"maxSizeRequest"}"#;
        assert!(matches!(
            StatusOutcome::classify(400, serde_json::from_slice(body).ok().as_ref()),
            StatusOutcome::RequestTooLarge
        ));
    }

    #[test]
    fn limit_problem_max_concurrent_is_retried() {
        let body =
            br#"{"type":"urn:ietf:params:jmap:error:limit","limit":"maxConcurrentRequests"}"#;
        assert!(matches!(
            StatusOutcome::classify(429, serde_json::from_slice(body).ok().as_ref()),
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
        let client = client(RetryPolicy::new(0));
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
        assert_eq!(client.stats().retries_observed, 0);
    }

    #[test]
    fn upload_within_max_size_is_not_short_circuited() {
        let client = client(RetryPolicy::new(0));
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
        let got = ProblemDetail::of(serde_json::from_slice::<Value>(body).ok().as_ref())
            .map(|d| d.to_string())
            .expect("title+detail");
        assert!(got.contains("Quota exceeded"));
        assert!(got.contains("50000000 bytes"));
    }

    #[test]
    fn problem_detail_title_only() {
        let body = br#"{"title":"Too Many Requests"}"#;
        assert_eq!(
            ProblemDetail::of(serde_json::from_slice::<Value>(body).ok().as_ref())
                .map(|d| d.to_string())
                .as_deref(),
            Some("Too Many Requests")
        );
    }

    #[test]
    fn problem_detail_returns_none_for_empty_body() {
        assert_eq!(
            ProblemDetail::of(serde_json::from_slice::<Value>(b"{}").ok().as_ref())
                .map(|d| d.to_string()),
            None
        );
        assert_eq!(
            ProblemDetail::of(serde_json::from_slice::<Value>(b"not json").ok().as_ref())
                .map(|d| d.to_string()),
            None
        );
    }

    #[test]
    fn counters_start_at_zero() {
        let client = client(RetryPolicy::new(0));
        assert_eq!(client.stats(), RetryStats::default());
    }
}
