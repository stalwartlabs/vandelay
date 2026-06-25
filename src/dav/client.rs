/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::io::{BufReader, Read};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use digest_auth::{AuthContext, HttpMethod, WwwAuthenticateHeader};
use ureq::Agent;
use ureq::Body;
use ureq::config::{Config, RedirectAuthHeaders};
use ureq::http::{Method, Request, Response};
use ureq::tls::{RootCerts, TlsConfig};

use crate::dav::parse::{ControlStrippingReader, DavResponse, parse_multistatus};
use crate::dav::retry::{DavOutcome, classify};
use crate::jmap::error::JmapError;
use crate::jmap::http::{Auth, RetryPolicy, retry_after_header};
use crate::jmap::retry::{self, RateLimitState};
use crate::logging::{LEVEL_BODIES, LEVEL_DEFAULT, LEVEL_PROGRESS, Logger};

const MAX_BODY: u64 = 512 * 1024 * 1024;
const LONG_RETRY_THRESHOLD: Duration = Duration::from_secs(10);
const MAX_DAV_REDIRECTS: u32 = 5;

const ACCEPT_DAV: &str = "application/xml, text/xml;q=0.9, */*;q=0.5";
const ACCEPT_BINARY: &str = "*/*";

#[derive(Debug)]
pub struct DavHttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub etag: Option<String>,
    pub content_type: Option<String>,
    pub last_modified: Option<String>,
}

#[derive(Debug)]
pub struct MultiStatus {
    pub status: u16,
    pub responses: Vec<DavResponse>,
    pub final_url: String,
}

struct Inner {
    agent: Agent,
    auth: Auth,
    nonce_cache: Mutex<Option<WwwAuthenticateHeader>>,
    retry: RetryPolicy,
    rate_limit: RateLimitState,
    log_level: AtomicU8,
    retries_total: AtomicU64,
    retry_after_sleeps: AtomicU64,
    user_agent: String,
}

#[derive(Clone)]
pub struct DavClient {
    inner: Arc<Inner>,
}

impl DavClient {
    pub fn new(auth: Auth, retry: RetryPolicy, allow_invalid_certs: bool) -> Self {
        let config: Config = Config::builder()
            .http_status_as_error(false)
            .allow_non_standard_methods(true)
            .max_redirects(0)
            .redirect_auth_headers(RedirectAuthHeaders::SameHost)
            .tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::PlatformVerifier)
                    .disable_verification(allow_invalid_certs)
                    .build(),
            )
            .build();
        DavClient {
            inner: Arc::new(Inner {
                agent: config.new_agent(),
                auth,
                nonce_cache: Mutex::new(None),
                retry,
                rate_limit: RateLimitState::new(),
                log_level: AtomicU8::new(LEVEL_DEFAULT),
                retries_total: AtomicU64::new(0),
                retry_after_sleeps: AtomicU64::new(0),
                user_agent: format!("vandelay/{}", env!("CARGO_PKG_VERSION")),
            }),
        }
    }

    pub fn set_logger(&self, logger: Logger) {
        self.inner
            .log_level
            .store(logger.level(), Ordering::Relaxed);
    }

    pub fn logger(&self) -> Logger {
        Logger::new(self.inner.log_level.load(Ordering::Relaxed))
    }

    pub fn rate_limit(&self) -> &RateLimitState {
        &self.inner.rate_limit
    }

    pub fn retries_observed(&self) -> u64 {
        self.inner.retries_total.load(Ordering::Relaxed)
    }

    pub fn retry_after_sleeps(&self) -> u64 {
        self.inner.retry_after_sleeps.load(Ordering::Relaxed)
    }

    fn authorization_header(&self, req: &WireRequest<'_>) -> Result<Option<String>, JmapError> {
        match &self.inner.auth {
            Auth::Basic { .. } | Auth::Bearer { .. } => Ok(self.inner.auth.header_value()),
            Auth::Digest { .. } => {
                let mut cache = self
                    .inner
                    .nonce_cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let Some(challenge) = cache.as_mut() else {
                    return Ok(None);
                };
                compute_digest_header(&self.inner.auth, challenge, req.method, req.url, req.body)
                    .map(Some)
            }
        }
    }

    fn handle_digest_challenge(
        &self,
        include_auth: bool,
        www_authenticate: Option<&str>,
        digest_retried: &mut bool,
    ) -> Result<bool, JmapError> {
        if !include_auth || *digest_retried || !matches!(self.inner.auth, Auth::Digest { .. }) {
            return Ok(false);
        }
        let Some(header) = www_authenticate else {
            return Ok(false);
        };
        let Some(challenge) = extract_digest_challenge(header) else {
            return Ok(false);
        };
        let parsed = WwwAuthenticateHeader::parse(challenge)
            .map_err(|e| JmapError::Auth(format!("invalid digest challenge: {e}")))?;
        *self
            .inner
            .nonce_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(parsed);
        *digest_retried = true;
        Ok(true)
    }

    pub fn propfind(&self, url: &str, depth: u8, body: &str) -> Result<DavHttpResponse, JmapError> {
        self.execute(
            "PROPFIND",
            url,
            Some(body.as_bytes()),
            Some("application/xml; charset=utf-8"),
            Some(depth),
            ACCEPT_DAV,
        )
    }

    pub fn report(&self, url: &str, depth: u8, body: &str) -> Result<DavHttpResponse, JmapError> {
        self.execute(
            "REPORT",
            url,
            Some(body.as_bytes()),
            Some("application/xml; charset=utf-8"),
            Some(depth),
            ACCEPT_DAV,
        )
    }

    pub fn get(&self, url: &str) -> Result<DavHttpResponse, JmapError> {
        self.execute("GET", url, None, None, None, ACCEPT_BINARY)
    }

    pub fn propfind_responses(
        &self,
        url: &str,
        depth: u8,
        body: &str,
        base_url: &str,
    ) -> Result<MultiStatus, JmapError> {
        self.stream_multistatus("PROPFIND", url, Some(body.as_bytes()), depth, base_url)
    }

    pub fn report_responses(
        &self,
        url: &str,
        depth: u8,
        body: &str,
        base_url: &str,
    ) -> Result<MultiStatus, JmapError> {
        self.stream_multistatus("REPORT", url, Some(body.as_bytes()), depth, base_url)
    }

    pub fn get_stream(&self, url: &str) -> Result<DavStream, JmapError> {
        let logger = self.logger();
        let policy = self.inner.retry;
        let mut attempt: u32 = 0;
        let original_url = url.to_owned();
        let mut current_url = url.to_owned();
        let mut redirects: u32 = 0;
        let mut include_auth = true;
        let mut digest_retried = false;
        loop {
            self.inner.rate_limit.cooldown().wait();
            let outcome = self.one_attempt_stream(&current_url, include_auth);
            match outcome {
                AttemptStream::Ok {
                    status,
                    body_reader,
                    retry_after,
                    etag,
                    content_type,
                    last_modified,
                    location,
                    www_authenticate,
                } => {
                    if (200..300).contains(&status) {
                        self.inner.rate_limit.on_success();
                        return Ok(DavStream {
                            status,
                            body_reader,
                            etag,
                            content_type,
                            last_modified,
                        });
                    }
                    if status == 404 || status == 410 {
                        self.inner.rate_limit.on_success();
                        return Ok(DavStream {
                            status,
                            body_reader: Box::new(std::io::empty()),
                            etag: None,
                            content_type: None,
                            last_modified: None,
                        });
                    }
                    if is_redirect_status(status) {
                        let Some(loc) = location else {
                            return Err(JmapError::Connect(format!(
                                "GET {current_url} returned {status} without Location"
                            )));
                        };
                        if redirects >= MAX_DAV_REDIRECTS {
                            return Err(JmapError::Connect(format!(
                                "too many redirects ({redirects}) starting from {url}"
                            )));
                        }
                        redirects += 1;
                        let next = resolve_redirect(&current_url, &loc)?;
                        if include_auth && !same_host(&original_url, &next) {
                            include_auth = false;
                            if logger.enabled(LEVEL_PROGRESS) {
                                eprintln!(
                                    "DAV redirect leaves origin host {original_url}; dropping Authorization for {next}"
                                );
                            }
                        }
                        if logger.enabled(LEVEL_PROGRESS) {
                            eprintln!("DAV {status}: {current_url} -> {next}");
                        }
                        current_url = next;
                        continue;
                    }
                    if status == 401
                        && self.handle_digest_challenge(
                            include_auth,
                            www_authenticate.as_deref(),
                            &mut digest_retried,
                        )?
                    {
                        continue;
                    }
                    let mut body_bytes = Vec::new();
                    let mut limited = body_reader.take(64 * 1024);
                    let _ = limited.read_to_end(&mut body_bytes);
                    self.handle_status(
                        &logger,
                        status,
                        &body_bytes,
                        retry_after,
                        &mut attempt,
                        &policy,
                    )?;
                }
                AttemptStream::Transport(err) => {
                    self.handle_transport(&logger, err, &mut attempt, &policy)?;
                }
            }
        }
    }

    fn execute(
        &self,
        method: &str,
        url: &str,
        body: Option<&[u8]>,
        content_type: Option<&str>,
        depth: Option<u8>,
        accept: &str,
    ) -> Result<DavHttpResponse, JmapError> {
        let logger = self.logger();
        let policy = self.inner.retry;
        let mut attempt: u32 = 0;
        let original_url = url.to_owned();
        let mut current_url = url.to_owned();
        let mut redirects: u32 = 0;
        let mut include_auth = true;
        let mut digest_retried = false;
        loop {
            self.inner.rate_limit.cooldown().wait();
            let outcome = self.one_attempt(WireRequest {
                method,
                url: &current_url,
                body,
                content_type,
                depth,
                accept,
                include_auth,
            });
            match outcome {
                Attempt::Ok {
                    status,
                    body,
                    retry_after,
                    etag,
                    content_type,
                    last_modified,
                    location,
                    www_authenticate,
                } => {
                    if (200..300).contains(&status) {
                        self.inner.rate_limit.on_success();
                        return Ok(DavHttpResponse {
                            status,
                            body,
                            etag,
                            content_type,
                            last_modified,
                        });
                    }
                    if is_redirect_status(status) {
                        let Some(loc) = location else {
                            return Err(JmapError::Connect(format!(
                                "{method} {current_url} returned {status} without Location: {}",
                                truncate(&body)
                            )));
                        };
                        if redirects >= MAX_DAV_REDIRECTS {
                            return Err(JmapError::Connect(format!(
                                "too many redirects ({redirects}) starting from {url}"
                            )));
                        }
                        redirects += 1;
                        let next = resolve_redirect(&current_url, &loc)?;
                        if include_auth && !same_host(&original_url, &next) {
                            include_auth = false;
                            if logger.enabled(LEVEL_PROGRESS) {
                                eprintln!(
                                    "DAV redirect leaves origin host {original_url}; dropping Authorization for {next}"
                                );
                            }
                        }
                        if logger.enabled(LEVEL_PROGRESS) {
                            eprintln!("DAV {status}: {current_url} -> {next}");
                        }
                        current_url = next;
                        continue;
                    }
                    if status == 401
                        && self.handle_digest_challenge(
                            include_auth,
                            www_authenticate.as_deref(),
                            &mut digest_retried,
                        )?
                    {
                        continue;
                    }
                    let disposition = classify(status, &body);
                    match disposition {
                        DavOutcome::Vanished => {
                            return Ok(DavHttpResponse {
                                status,
                                body,
                                etag: None,
                                content_type: None,
                                last_modified: None,
                            });
                        }
                        DavOutcome::Auth => {
                            return Err(JmapError::Auth(format!(
                                "server returned {status}: {}",
                                truncate(&body)
                            )));
                        }
                        DavOutcome::Fatal => {
                            return Err(JmapError::HttpStatus {
                                status,
                                body: truncate(&body),
                            });
                        }
                        DavOutcome::Retryable => {
                            attempt += 1;
                            self.inner.retries_total.fetch_add(1, Ordering::Relaxed);
                            if attempt > policy.max_retries {
                                return Err(JmapError::RetriesExhausted(format!(
                                    "{method} {current_url} kept returning {status}"
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
                                    "DAV {method} rate-limited (http {status}); waiting {}s before retry {}/{}",
                                    delay.as_secs(),
                                    attempt,
                                    policy.max_retries,
                                ));
                            }
                            if logger.enabled(LEVEL_BODIES) {
                                eprintln!(
                                    "retry {}/{} {method} {current_url} after {:?} (http {status}) body={}",
                                    attempt,
                                    policy.max_retries,
                                    delay,
                                    truncate(&body),
                                );
                            } else if logger.enabled(LEVEL_PROGRESS) {
                                eprintln!(
                                    "retry {}/{} (http {status})",
                                    attempt, policy.max_retries
                                );
                            }
                            std::thread::sleep(delay);
                        }
                        DavOutcome::Success => unreachable!(),
                    }
                }
                Attempt::Transport(err) => {
                    self.handle_transport(&logger, err, &mut attempt, &policy)?;
                }
            }
        }
    }

    fn stream_multistatus(
        &self,
        method: &str,
        url: &str,
        body: Option<&[u8]>,
        depth: u8,
        base_url: &str,
    ) -> Result<MultiStatus, JmapError> {
        let logger = self.logger();
        let policy = self.inner.retry;
        let mut attempt: u32 = 0;
        let original_url = url.to_owned();
        let mut current_url = url.to_owned();
        let mut redirects: u32 = 0;
        let mut include_auth = true;
        let mut digest_retried = false;
        loop {
            self.inner.rate_limit.cooldown().wait();
            let send = self.build_and_send(WireRequest {
                method,
                url: &current_url,
                body,
                content_type: Some("application/xml; charset=utf-8"),
                depth: Some(depth),
                accept: ACCEPT_DAV,
                include_auth,
            });
            match send {
                Ok(mut resp) => {
                    let status = resp.status().as_u16();
                    let www_authenticate = header_str(&resp, "www-authenticate");
                    let retry_after = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(retry_after_header);
                    let location = resp
                        .headers()
                        .get("location")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    if is_redirect_status(status) {
                        let Some(loc) = location else {
                            return Err(JmapError::Connect(format!(
                                "{method} {current_url} returned {status} without Location"
                            )));
                        };
                        if redirects >= MAX_DAV_REDIRECTS {
                            return Err(JmapError::Connect(format!(
                                "too many redirects ({redirects}) starting from {url}"
                            )));
                        }
                        redirects += 1;
                        let next = resolve_redirect(&current_url, &loc)?;
                        if include_auth && !same_host(&original_url, &next) {
                            include_auth = false;
                            if logger.enabled(LEVEL_PROGRESS) {
                                eprintln!(
                                    "DAV redirect leaves origin host {original_url}; dropping Authorization for {next}"
                                );
                            }
                        }
                        if logger.enabled(LEVEL_PROGRESS) {
                            eprintln!("DAV {status}: {current_url} -> {next}");
                        }
                        current_url = next;
                        continue;
                    }
                    if status == 401
                        && self.handle_digest_challenge(
                            include_auth,
                            www_authenticate.as_deref(),
                            &mut digest_retried,
                        )?
                    {
                        continue;
                    }
                    if (200..300).contains(&status) {
                        self.inner.rate_limit.on_success();
                        let parse_base = if current_url == url {
                            base_url
                        } else {
                            &current_url
                        };
                        let raw = resp.into_body().into_reader();
                        let stripped = ControlStrippingReader::new(raw);
                        let reader = BufReader::new(stripped);
                        let responses = parse_multistatus(reader, parse_base)
                            .map_err(|e| JmapError::Malformed(format!("multistatus parse: {e}")))?;
                        return Ok(MultiStatus {
                            status,
                            responses,
                            final_url: current_url.clone(),
                        });
                    }
                    let body_bytes =
                        match resp.body_mut().with_config().limit(64 * 1024).read_to_vec() {
                            Ok(b) => b,
                            Err(e) => {
                                return Err(JmapError::Transport(format!(
                                    "reading error body: {e}"
                                )));
                            }
                        };
                    let disposition = classify(status, &body_bytes);
                    match disposition {
                        DavOutcome::Vanished => {
                            return Ok(MultiStatus {
                                status,
                                responses: Vec::new(),
                                final_url: current_url.clone(),
                            });
                        }
                        DavOutcome::Auth => {
                            return Err(JmapError::Auth(format!(
                                "server returned {status}: {}",
                                truncate(&body_bytes)
                            )));
                        }
                        DavOutcome::Fatal => {
                            return Err(JmapError::HttpStatus {
                                status,
                                body: truncate(&body_bytes),
                            });
                        }
                        DavOutcome::Retryable => {
                            attempt += 1;
                            self.inner.retries_total.fetch_add(1, Ordering::Relaxed);
                            if attempt > policy.max_retries {
                                return Err(JmapError::RetriesExhausted(format!(
                                    "{method} {url} kept returning {status}"
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
                                    "DAV {method} rate-limited (http {status}); waiting {}s before retry {}/{}",
                                    delay.as_secs(),
                                    attempt,
                                    policy.max_retries,
                                ));
                            }
                            std::thread::sleep(delay);
                        }
                        DavOutcome::Success => unreachable!(),
                    }
                }
                Err(e) => {
                    self.handle_transport(&logger, e, &mut attempt, &policy)?;
                }
            }
        }
    }

    fn handle_status(
        &self,
        logger: &Logger,
        status: u16,
        body: &[u8],
        retry_after: Option<Duration>,
        attempt: &mut u32,
        policy: &RetryPolicy,
    ) -> Result<(), JmapError> {
        let disposition = classify(status, body);
        match disposition {
            DavOutcome::Vanished => Err(JmapError::HttpStatus {
                status,
                body: truncate(body),
            }),
            DavOutcome::Auth => Err(JmapError::Auth(format!("server returned {status}"))),
            DavOutcome::Fatal => Err(JmapError::HttpStatus {
                status,
                body: truncate(body),
            }),
            DavOutcome::Retryable => {
                *attempt += 1;
                self.inner.retries_total.fetch_add(1, Ordering::Relaxed);
                if *attempt > policy.max_retries {
                    return Err(JmapError::RetriesExhausted(format!(
                        "GET stream kept returning {status}"
                    )));
                }
                if retry_after.is_some() {
                    self.inner
                        .retry_after_sleeps
                        .fetch_add(1, Ordering::Relaxed);
                }
                let delay = self.inner.rate_limit.on_throttle(policy, retry_after);
                if delay >= LONG_RETRY_THRESHOLD {
                    logger.warn(&format!(
                        "DAV stream rate-limited (http {status}); waiting {}s",
                        delay.as_secs()
                    ));
                }
                std::thread::sleep(delay);
                Ok(())
            }
            DavOutcome::Success => Ok(()),
        }
    }

    fn handle_transport(
        &self,
        logger: &Logger,
        err: JmapError,
        attempt: &mut u32,
        policy: &RetryPolicy,
    ) -> Result<(), JmapError> {
        match transport_disposition(&err) {
            retry::Disposition::Fatal => Err(err),
            retry::Disposition::Retryable => {
                *attempt += 1;
                self.inner.retries_total.fetch_add(1, Ordering::Relaxed);
                if *attempt > policy.max_retries {
                    return Err(JmapError::RetriesExhausted(format!("{err}")));
                }
                let delay = retry::backoff_delay(policy, *attempt);
                if delay >= LONG_RETRY_THRESHOLD {
                    logger.warn(&format!(
                        "transient transport failure ({err}); waiting {}s before retry {}/{}",
                        delay.as_secs(),
                        attempt,
                        policy.max_retries,
                    ));
                }
                std::thread::sleep(delay);
                Ok(())
            }
        }
    }

    fn one_attempt(&self, req: WireRequest<'_>) -> Attempt {
        match self.build_and_send(req) {
            Ok(mut resp) => {
                let status = resp.status().as_u16();
                let etag = header_str(&resp, "etag");
                let content_type_header = header_str(&resp, "content-type");
                let last_modified = header_str(&resp, "last-modified");
                let location = header_str(&resp, "location");
                let www_authenticate = header_str(&resp, "www-authenticate");
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(retry_after_header);
                if is_redirect_status(status) && location.is_some() {
                    return Attempt::Ok {
                        status,
                        body: Vec::new(),
                        retry_after,
                        etag,
                        content_type: content_type_header,
                        last_modified,
                        location,
                        www_authenticate,
                    };
                }
                let body_limit = if (200..300).contains(&status) {
                    MAX_BODY
                } else {
                    64 * 1024
                };
                match resp
                    .body_mut()
                    .with_config()
                    .limit(body_limit)
                    .read_to_vec()
                {
                    Ok(bytes) => Attempt::Ok {
                        status,
                        body: bytes,
                        retry_after,
                        etag,
                        content_type: content_type_header,
                        last_modified,
                        location,
                        www_authenticate,
                    },
                    Err(e) => Attempt::Transport(JmapError::Transport(format!(
                        "reading response body: {e}"
                    ))),
                }
            }
            Err(e) => Attempt::Transport(e),
        }
    }

    fn one_attempt_stream(&self, url: &str, include_auth: bool) -> AttemptStream {
        let req = WireRequest {
            method: "GET",
            url,
            body: None,
            content_type: None,
            depth: None,
            accept: ACCEPT_BINARY,
            include_auth,
        };
        match self.build_and_send(req) {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let etag = header_str(&resp, "etag");
                let content_type = header_str(&resp, "content-type");
                let last_modified = header_str(&resp, "last-modified");
                let location = header_str(&resp, "location");
                let www_authenticate = header_str(&resp, "www-authenticate");
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(retry_after_header);
                let body_reader = resp.into_body().into_reader();
                AttemptStream::Ok {
                    status,
                    body_reader: Box::new(body_reader),
                    etag,
                    content_type,
                    last_modified,
                    location,
                    www_authenticate,
                    retry_after,
                }
            }
            Err(e) => AttemptStream::Transport(e),
        }
    }

    fn build_and_send(&self, req: WireRequest<'_>) -> Result<Response<Body>, JmapError> {
        let method_parsed = Method::from_bytes(req.method.as_bytes())
            .map_err(|e| JmapError::Connect(format!("invalid HTTP method {}: {e}", req.method)))?;
        let mut builder = Request::builder()
            .method(method_parsed)
            .uri(req.url)
            .header("Accept", req.accept)
            .header("User-Agent", self.inner.user_agent.as_str());
        if req.include_auth
            && let Some(auth) = self.authorization_header(&req)?
        {
            builder = builder.header("Authorization", auth);
        }
        if let Some(d) = req.depth {
            builder = builder.header("Depth", d.to_string());
        }
        if let Some(ct) = req.content_type {
            builder = builder.header("Content-Type", ct);
        }
        let payload: &[u8] = req.body.unwrap_or(&[]);
        let request = builder
            .body(payload)
            .map_err(|e| JmapError::Connect(format!("building request: {e}")))?;
        self.inner.agent.run(request).map_err(map_ureq_error)
    }
}

struct WireRequest<'a> {
    method: &'a str,
    url: &'a str,
    body: Option<&'a [u8]>,
    content_type: Option<&'a str>,
    depth: Option<u8>,
    accept: &'a str,
    include_auth: bool,
}

pub struct DavStream {
    pub status: u16,
    pub body_reader: Box<dyn Read + Send>,
    pub etag: Option<String>,
    pub content_type: Option<String>,
    pub last_modified: Option<String>,
}

impl Read for DavStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.body_reader.read(buf)
    }
}

enum Attempt {
    Ok {
        status: u16,
        body: Vec<u8>,
        retry_after: Option<Duration>,
        etag: Option<String>,
        content_type: Option<String>,
        last_modified: Option<String>,
        location: Option<String>,
        www_authenticate: Option<String>,
    },
    Transport(JmapError),
}

enum AttemptStream {
    Ok {
        status: u16,
        body_reader: Box<dyn Read + Send>,
        etag: Option<String>,
        content_type: Option<String>,
        last_modified: Option<String>,
        location: Option<String>,
        www_authenticate: Option<String>,
        retry_after: Option<Duration>,
    },
    Transport(JmapError),
}

fn header_str(resp: &Response<Body>, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

fn is_redirect_status(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn resolve_redirect(current: &str, location: &str) -> Result<String, JmapError> {
    crate::dav::href::join_absolute(current, location)
        .map_err(|e| JmapError::Connect(format!("redirect: {e}")))
}

fn url_host_and_port(url: &str) -> Option<(String, Option<u16>)> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_owned();
    Some((host, parsed.port_or_known_default()))
}

fn same_host(a: &str, b: &str) -> bool {
    match (url_host_and_port(a), url_host_and_port(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

fn extract_digest_challenge(header: &str) -> Option<&str> {
    let trimmed = header.trim();
    if trimmed.starts_with("Digest") {
        return Some(trimmed);
    }
    let lower = trimmed.to_ascii_lowercase();
    lower.find("digest ").map(|idx| &trimmed[idx..])
}

fn digest_request_uri(url: &str) -> Result<String, JmapError> {
    let parsed =
        url::Url::parse(url).map_err(|e| JmapError::Connect(format!("digest uri: {e}")))?;
    let mut uri = parsed.path().to_owned();
    if uri.is_empty() {
        uri.push('/');
    }
    if let Some(query) = parsed.query() {
        uri.push('?');
        uri.push_str(query);
    }
    Ok(uri)
}

fn compute_digest_header(
    auth: &Auth,
    challenge: &mut WwwAuthenticateHeader,
    method: &str,
    url: &str,
    body: Option<&[u8]>,
) -> Result<String, JmapError> {
    compute_digest_header_with_cnonce(auth, challenge, method, url, body, None)
}

fn compute_digest_header_with_cnonce(
    auth: &Auth,
    challenge: &mut WwwAuthenticateHeader,
    method: &str,
    url: &str,
    body: Option<&[u8]>,
    cnonce: Option<&str>,
) -> Result<String, JmapError> {
    let Auth::Digest { user, password } = auth else {
        return Err(JmapError::Auth(
            "digest auth requested with non-digest credentials".to_owned(),
        ));
    };
    let uri = digest_request_uri(url)?;
    let mut context = AuthContext::new_with_method(
        user.as_str(),
        password.as_str(),
        uri,
        body,
        HttpMethod::from(method),
    );
    if let Some(cnonce) = cnonce {
        context.set_custom_cnonce(cnonce);
    }
    challenge
        .respond(&context)
        .map(|header| header.to_string())
        .map_err(|e| JmapError::Auth(format!("digest auth failed: {e}")))
}

fn transport_disposition(err: &JmapError) -> retry::Disposition {
    match err {
        JmapError::Connect(_) => retry::Disposition::Fatal,
        _ => retry::Disposition::Retryable,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_constructs_cleanly() {
        let c = DavClient::new(
            Auth::Basic {
                user: "u".into(),
                password: "p".into(),
            },
            RetryPolicy::new(3),
            false,
        );
        assert_eq!(c.retries_observed(), 0);
        assert_eq!(c.retry_after_sleeps(), 0);
    }

    #[test]
    fn auth_header_appears_via_builder() {
        let c = DavClient::new(
            Auth::Bearer {
                token: "abc".into(),
            },
            RetryPolicy::new(0),
            false,
        );
        let logger = c.logger();
        assert_eq!(logger.level(), LEVEL_DEFAULT);
    }

    #[test]
    fn compute_digest_header_matches_rfc7616_example() {
        let mut challenge = WwwAuthenticateHeader::parse(
            r#"Digest realm="http-auth@example.org", qop="auth, auth-int", algorithm=MD5, nonce="7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v", opaque="FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS""#,
        )
        .expect("parse challenge");
        let auth = Auth::Digest {
            user: "Mufasa".into(),
            password: "Circle of Life".into(),
        };
        let header = compute_digest_header_with_cnonce(
            &auth,
            &mut challenge,
            "GET",
            "https://example.org/dir/index.html",
            None,
            Some("f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ"),
        )
        .expect("digest header");
        assert_eq!(
            header,
            r#"Digest username="Mufasa", realm="http-auth@example.org", nonce="7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v", uri="/dir/index.html", qop=auth, nc=00000001, cnonce="f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ", response="8ca523f5e9506fed4657c9700eebdbec", opaque="FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS", algorithm=MD5"#
        );
    }

    #[test]
    fn digest_authorization_header_uses_cached_nonce() {
        let client = DavClient::new(
            Auth::Digest {
                user: "Mufasa".into(),
                password: "Circle of Life".into(),
            },
            RetryPolicy::new(0),
            false,
        );
        *client
            .inner
            .nonce_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(
            WwwAuthenticateHeader::parse(
                r#"Digest realm="testrealm@host.com", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093""#,
            )
            .expect("parse challenge"),
        );
        let header = client
            .authorization_header(&WireRequest {
                method: "GET",
                url: "http://example.com/dir/index.html",
                body: None,
                content_type: None,
                depth: None,
                accept: ACCEPT_BINARY,
                include_auth: true,
            })
            .expect("auth header");
        assert_eq!(
            header.as_deref(),
            Some(
                r#"Digest username="Mufasa", realm="testrealm@host.com", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", uri="/dir/index.html", response="2951cdbad33b2271fcb6b8e7b8feac23""#
            )
        );
    }

    #[test]
    fn digest_authorization_header_is_omitted_without_nonce() {
        let client = DavClient::new(
            Auth::Digest {
                user: "Mufasa".into(),
                password: "Circle of Life".into(),
            },
            RetryPolicy::new(0),
            false,
        );
        let header = client
            .authorization_header(&WireRequest {
                method: "GET",
                url: "http://example.com/dir/index.html",
                body: None,
                content_type: None,
                depth: None,
                accept: ACCEPT_BINARY,
                include_auth: true,
            })
            .expect("auth header");
        assert_eq!(header, None);
    }

    #[test]
    fn truncate_preserves_char_boundaries() {
        let mut body = vec![b'a'; 510];
        body.extend_from_slice("\u{1F4A9}".as_bytes());
        body.extend_from_slice(&[b'b'; 600]);
        let s = truncate(&body);
        assert!(s.ends_with("..."));
        assert!(s.is_char_boundary(s.len()));
    }
}
