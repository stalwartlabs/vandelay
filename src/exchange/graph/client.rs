/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::str;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use ureq::ResponseExt;
use ureq::http::Uri;
use url::Url;

use crate::exchange::graph::error::GraphError;
use crate::exchange::graph::retry::HttpClass;
use crate::logging::{HttpCall, Level, Logger};
use crate::net::http::{
    AgentProfile, BodyExcerpt, ClientOptions, HttpCore, Methods, Redirects, RetryStats,
    TransportError,
};
use crate::net::retry::{RetryAfter, RetryWait};

const MAX_BODY: u64 = 256 * 1024 * 1024;
const LONG_RETRY_THRESHOLD: Duration = Duration::from_secs(10);
const PROFILE: AgentProfile = AgentProfile {
    redirects: Redirects::Follow,
    methods: Methods::Standard,
    max_body: MAX_BODY,
};
const IMMUTABLE_ID: &str = "IdType=\"ImmutableId\"";
const DRIVE_CONTENT_SUFFIX: &str = "/content";

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
        str::from_utf8(&self.body)
            .map_err(|e| GraphError::Malformed(format!("response is not utf-8: {e}")))
    }

    pub fn json(&self) -> Result<Value, GraphError> {
        serde_json::from_slice(&self.body)
            .map_err(|e| GraphError::Malformed(format!("response is not valid json: {e}")))
    }
}

struct Inner {
    core: HttpCore,
    bearer: Mutex<String>,
}

#[derive(Clone)]
pub struct GraphClient {
    inner: Arc<Inner>,
}

enum Attempt {
    Response {
        status: u16,
        body: Vec<u8>,
        retry_after: Option<Duration>,
        content_type: Option<String>,
    },
    Transport(GraphError),
}

struct Request<'a> {
    url: &'a str,
    accept: Accept,
    prefer: &'a [&'a str],
}

impl GraphClient {
    const METHOD: &str = "GET";

    pub fn new(bearer: String, options: ClientOptions) -> GraphClient {
        GraphClient {
            inner: Arc::new(Inner {
                core: HttpCore::new(options, PROFILE),
                bearer: Mutex::new(bearer),
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

    pub fn logger(&self) -> Logger {
        self.inner.core.logger()
    }

    fn auth_header(&self) -> String {
        let bearer = match self.inner.bearer.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        format!("Bearer {bearer}")
    }

    pub fn stats(&self) -> RetryStats {
        self.inner.core.stats().snapshot()
    }

    pub fn requests_observed(&self) -> u64 {
        self.inner.core.stats().requests()
    }

    pub fn get(&self, url: &str, accept: Accept) -> Result<GraphResponse, GraphError> {
        self.execute(&Request {
            url,
            accept,
            prefer: &[],
        })
    }

    pub fn get_with_prefer(
        &self,
        url: &str,
        accept: Accept,
        prefer: &[&str],
    ) -> Result<GraphResponse, GraphError> {
        self.execute(&Request {
            url,
            accept,
            prefer,
        })
    }

    pub fn get_json(&self, url: &str) -> Result<Value, GraphError> {
        self.get(url, Accept::Json).and_then(|r| r.json())
    }

    pub fn get_json_with_prefer(&self, url: &str, prefer: &[&str]) -> Result<Value, GraphError> {
        self.get_with_prefer(url, Accept::Json, prefer)
            .and_then(|r| r.json())
    }

    fn execute(&self, request: &Request<'_>) -> Result<GraphResponse, GraphError> {
        let core = &self.inner.core;
        let method = GraphClient::METHOD;
        let url = request.url;
        let max_retries = core.retry().max_retries;
        let logger = core.logger();
        let mut attempt: u32 = 0;
        loop {
            core.rate_limit().cooldown().wait();
            core.stats().note_request();
            let (status, body, retry_after) = match self.one_attempt(request) {
                Attempt::Response {
                    status,
                    body,
                    retry_after,
                    content_type,
                } => match HttpClass::of_status(status) {
                    HttpClass::Success => {
                        core.rate_limit().on_success();
                        return Ok(GraphResponse {
                            status,
                            body,
                            content_type,
                        });
                    }
                    HttpClass::Vanished => return Err(GraphError::Vanished),
                    HttpClass::Auth => {
                        return Err(GraphError::Auth(format!(
                            "server returned {status}: {}",
                            BodyExcerpt(&body)
                        )));
                    }
                    HttpClass::Fatal => {
                        return Err(GraphError::HttpStatus {
                            status,
                            body: BodyExcerpt(&body).to_string(),
                        });
                    }
                    HttpClass::Retryable => (status, body, retry_after),
                },
                Attempt::Transport(err) => {
                    if matches!(err, GraphError::Connect(_)) {
                        return Err(err);
                    }
                    attempt += 1;
                    core.stats().note_retry();
                    if attempt > max_retries {
                        return Err(GraphError::RetriesExhausted(format!(
                            "{method} {url}: {err}"
                        )));
                    }
                    let delay = core.backoff(attempt);
                    if delay >= LONG_RETRY_THRESHOLD {
                        logger.warn(format_args!(
                            "transient transport failure ({err}); waiting {} before retry {attempt}/{max_retries}",
                            RetryWait(delay),
                        ));
                    }
                    thread::sleep(delay);
                    continue;
                }
            };
            attempt += 1;
            core.stats().note_retry();
            if attempt > max_retries {
                return Err(GraphError::RetriesExhausted(format!(
                    "{method} {url} kept returning http {status}"
                )));
            }
            if retry_after.is_some() {
                core.stats().note_retry_after();
            }
            let delay = match retry_after {
                _ if HttpClass::arms_throttle(status) => core.throttle(retry_after),
                Some(d) => d,
                None => core.backoff(attempt),
            };
            if delay >= LONG_RETRY_THRESHOLD {
                logger.warn(format_args!(
                    "Graph rate-limited (http {status}); waiting {} before retry {attempt}/{max_retries} (shared level {})",
                    RetryWait(delay),
                    core.rate_limit().level(),
                ));
            }
            if logger.enabled(Level::Bodies) {
                logger.progress(format_args!(
                    "retry {attempt}/{max_retries} {method} {url} after {delay:?} (http {status}) body={}",
                    BodyExcerpt(&body)
                ));
            } else {
                logger.progress(format_args!(
                    "retry {attempt}/{max_retries} (http {status})"
                ));
            }
            thread::sleep(delay);
        }
    }

    fn one_attempt(&self, request: &Request<'_>) -> Attempt {
        let core = &self.inner.core;
        let method = GraphClient::METHOD;
        let url = request.url;
        let mut req = core
            .agent()
            .get(url)
            .header("Authorization", self.auth_header())
            .header("Accept", request.accept.header_value())
            .header("User-Agent", core.user_agent())
            .header("Prefer", IMMUTABLE_ID);
        for value in request.prefer {
            req = req.header("Prefer", *value);
        }
        let logger = core.logger();
        let started = Instant::now();
        match req.call() {
            Ok(mut resp) => {
                let status = resp.status().as_u16();
                let final_uri = resp.get_uri().clone();
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(RetryAfter::parse_now);
                let content_type = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                match resp.body_mut().with_config().limit(MAX_BODY).read_to_vec() {
                    Ok(bytes) => {
                        GraphClient::warn_on_redirect(logger, url, &final_uri);
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
                        Attempt::Response {
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
                let err = GraphError::from(TransportError::from(e));
                logger.trace_http_error("Graph", method, url, &err.to_string(), started.elapsed());
                Attempt::Transport(err)
            }
        }
    }

    fn warn_on_redirect(logger: Logger, requested: &str, final_uri: &Uri) {
        if requested.ends_with(DRIVE_CONTENT_SUFFIX) {
            return;
        }
        let landed = final_uri.to_string();
        if HttpCore::is_cross_host(requested, &landed) {
            let parsed = Url::parse(&landed).ok();
            let host = parsed
                .as_ref()
                .and_then(Url::host_str)
                .unwrap_or("another host");
            logger.warn(format_args!(
                "{} {requested} was redirected across hosts to {host}; verify the Graph endpoint host is reachable directly without redirection",
                GraphClient::METHOD
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::http::TlsPolicy;
    use crate::net::retry::RetryPolicy;

    fn options(max_retries: u32) -> ClientOptions {
        ClientOptions {
            retry: RetryPolicy::new(max_retries),
            tls: TlsPolicy::Verify,
            logger: Logger::new(Level::Quiet),
        }
    }

    #[test]
    fn defaults_construct() {
        let c = GraphClient::new("token".to_owned(), options(3));
        assert_eq!(c.stats(), RetryStats::default());
        assert_eq!(c.requests_observed(), 0);
        assert_eq!(c.auth_header(), "Bearer token");
    }

    #[test]
    fn bearer_can_be_swapped_at_runtime() {
        let c = GraphClient::new("old".to_owned(), options(0));
        c.set_bearer("new".to_owned());
        assert_eq!(c.auth_header(), "Bearer new");
    }

    #[test]
    fn accept_header_values_are_what_the_spec_requires() {
        assert_eq!(Accept::Json.header_value(), "application/json");
        assert_eq!(Accept::Text.header_value(), "text/plain");
    }
}
