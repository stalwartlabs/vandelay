/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::error::Error;
use std::io::{self, Read};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ureq::Body;
use ureq::BodyReader;
use ureq::http::{Method, Request, Response};

use crate::dav::error::DavError;
use crate::dav::parse::DavResponse;
use crate::logging::Logger;
use crate::net::auth::Auth;
use crate::net::http::{
    AgentProfile, ClientOptions, HttpCore, Methods, Redirects, RetryStats, TransportError,
};

mod buffered;
mod stream;
mod wire;

use wire::{Credentials, Verb, WireRequest};

const MAX_BODY: u64 = 512 * 1024 * 1024;
const ERROR_BODY_LIMIT: u64 = 64 * 1024;
const LONG_RETRY_THRESHOLD: Duration = Duration::from_secs(10);
const MAX_DAV_REDIRECTS: u32 = 5;
const PROTO: &str = "DAV";

const ACCEPT_DAV: &str = "application/xml, text/xml;q=0.9, */*;q=0.5";
const ACCEPT_BINARY: &str = "*/*";
const XML_BODY_TYPE: &str = "application/xml; charset=utf-8";
const XML_TRACE_TYPE: &str = "application/xml";

const MOVED_PERMANENTLY: u16 = 301;
const FOUND: u16 = 302;
const SEE_OTHER: u16 = 303;
const TEMPORARY_REDIRECT: u16 = 307;
const PERMANENT_REDIRECT: u16 = 308;
const NOT_FOUND: u16 = 404;
const GONE: u16 = 410;

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

pub struct DavStream {
    pub status: u16,
    pub etag: Option<String>,
    pub content_type: Option<String>,
    pub last_modified: Option<String>,
    body: Option<BodyReader<'static>>,
}

impl Read for DavStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.body {
            Some(body) => body.read(buf),
            None => Ok(0),
        }
    }
}

struct Inner {
    core: HttpCore,
    authorization: String,
}

#[derive(Clone)]
pub struct DavClient {
    inner: Arc<Inner>,
}

impl DavClient {
    pub fn new(auth: Auth, options: ClientOptions) -> DavClient {
        let profile = AgentProfile {
            redirects: Redirects::Manual,
            methods: Methods::Extended,
            max_body: MAX_BODY,
        };
        DavClient {
            inner: Arc::new(Inner {
                core: HttpCore::new(options, profile),
                authorization: auth.header_value(),
            }),
        }
    }

    pub fn logger(&self) -> Logger {
        self.inner.core.logger()
    }

    pub fn stats(&self) -> RetryStats {
        self.inner.core.stats().snapshot()
    }

    pub fn propfind(&self, url: &str, depth: u8, body: &str) -> Result<DavHttpResponse, DavError> {
        self.execute(Verb::Propfind, url, Some((body, depth)))
    }

    pub fn report(&self, url: &str, depth: u8, body: &str) -> Result<DavHttpResponse, DavError> {
        self.execute(Verb::Report, url, Some((body, depth)))
    }

    pub fn get(&self, url: &str) -> Result<DavHttpResponse, DavError> {
        self.execute(Verb::Get, url, None)
    }

    pub fn propfind_responses(
        &self,
        url: &str,
        depth: u8,
        body: &str,
        base_url: &str,
    ) -> Result<MultiStatus, DavError> {
        self.stream_multistatus(Verb::Propfind, url, body.as_bytes(), depth, base_url)
    }

    pub fn report_responses(
        &self,
        url: &str,
        depth: u8,
        body: &str,
        base_url: &str,
    ) -> Result<MultiStatus, DavError> {
        self.stream_multistatus(Verb::Report, url, body.as_bytes(), depth, base_url)
    }

    fn status_retry(
        &self,
        attempt: &mut u32,
        retry_after: Option<Duration>,
        exhausted: impl FnOnce() -> String,
    ) -> Result<Duration, DavError> {
        let core = &self.inner.core;
        *attempt += 1;
        core.stats().note_retry();
        if *attempt > core.retry().max_retries {
            return Err(DavError::RetriesExhausted(exhausted()));
        }
        if retry_after.is_some() {
            core.stats().note_retry_after();
        }
        Ok(core.throttle(retry_after))
    }

    fn transport_retry(
        &self,
        logger: Logger,
        err: DavError,
        attempt: &mut u32,
    ) -> Result<(), DavError> {
        if err.is_connect() {
            return Err(err);
        }
        let core = &self.inner.core;
        let max_retries = core.retry().max_retries;
        *attempt += 1;
        core.stats().note_retry();
        if *attempt > max_retries {
            return Err(DavError::RetriesExhausted(err.to_string()));
        }
        let delay = core.backoff(*attempt);
        if delay >= LONG_RETRY_THRESHOLD {
            logger.warn(format_args!(
                "transient transport failure ({err}); waiting {}s before retry {attempt}/{max_retries}",
                delay.as_secs(),
            ));
        }
        thread::sleep(delay);
        Ok(())
    }

    fn send(&self, req: &WireRequest<'_>) -> Result<Response<Body>, TransportError> {
        let mut builder = Request::builder()
            .method(Method::from_bytes(req.verb.as_str().as_bytes()).map_err(DavClient::invalid)?)
            .uri(req.url)
            .header("Accept", req.accept)
            .header("User-Agent", self.inner.core.user_agent());
        if req.credentials == Credentials::Send {
            builder = builder.header("Authorization", self.inner.authorization.as_str());
        }
        if let Some(depth) = req.depth {
            builder = builder.header("Depth", u16::from(depth));
        }
        if let Some(content_type) = req.content_type {
            builder = builder.header("Content-Type", content_type);
        }
        let request = builder
            .body(req.body.unwrap_or_default())
            .map_err(DavClient::invalid)?;
        self.inner
            .core
            .agent()
            .run(request)
            .map_err(TransportError::from)
    }

    fn invalid(e: impl Into<Box<dyn Error + Send + Sync>>) -> TransportError {
        TransportError::Other(ureq::Error::Other(Box::new(io::Error::other(e))))
    }
}

#[cfg(test)]
mod tests {
    use super::wire::RedirectChain;
    use super::*;
    use crate::logging::Level;
    use crate::net::http::TlsPolicy;
    use crate::net::retry::RetryPolicy;

    fn options(retries: u32) -> ClientOptions {
        ClientOptions {
            retry: RetryPolicy::new(retries),
            tls: TlsPolicy::Verify,
            logger: Logger::new(Level::Default),
        }
    }

    #[test]
    fn client_constructs_cleanly() {
        let c = DavClient::new(
            Auth::Basic {
                user: "u".into(),
                password: "p".into(),
            },
            options(3),
        );
        assert_eq!(c.stats(), RetryStats::default());
        assert_eq!(c.logger().level(), Level::Default);
        let c = DavClient::new(
            Auth::Bearer {
                token: "abc".into(),
            },
            options(0),
        );
        assert_eq!(c.inner.authorization, "Bearer abc");
        assert_eq!(c.logger().level(), Level::Default);
    }

    #[test]
    fn redirect_chain_drops_credentials_off_origin_and_caps_hops() {
        let logger = Logger::new(Level::Quiet);
        let mut chain = RedirectChain::new("https://a.example/dav/");
        chain.follow(FOUND, "/moved/", logger).expect("same host");
        assert_eq!(chain.current, "https://a.example/moved/");
        assert_eq!(chain.credentials, Credentials::Send);
        chain
            .follow(FOUND, "https://b.example/x/", logger)
            .expect("cross host");
        assert_eq!(chain.credentials, Credentials::Withhold);
        for _ in 2..MAX_DAV_REDIRECTS {
            chain.follow(FOUND, "/y/", logger).expect("within cap");
        }
        let err = chain.follow(FOUND, "/z/", logger).expect_err("cap");
        assert_eq!(
            err.to_string(),
            "connection failure: too many redirects (5) starting from https://a.example/dav/"
        );
    }
}
