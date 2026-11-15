/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::io::{BufReader, Read};
use std::thread;
use std::time::Instant;

use super::wire::{RedirectChain, ResponseHeaders, Verb, WireRequest};
use super::{
    ACCEPT_BINARY, ACCEPT_DAV, DavClient, DavStream, ERROR_BODY_LIMIT, GONE, LONG_RETRY_THRESHOLD,
    MultiStatus, NOT_FOUND, PROTO, XML_BODY_TYPE, XML_TRACE_TYPE,
};
use crate::dav::error::DavError;
use crate::dav::parse::{ControlStrippingReader, MultistatusParser};
use crate::dav::retry::DavOutcome;
use crate::logging::HttpCall;
use crate::net::http::BodyExcerpt;

impl DavClient {
    pub fn get_stream(&self, url: &str) -> Result<DavStream, DavError> {
        let logger = self.logger();
        let mut attempt: u32 = 0;
        let mut chain = RedirectChain::new(url);
        loop {
            self.inner.core.rate_limit().cooldown().wait();
            let request = WireRequest {
                verb: Verb::Get,
                url: &chain.current,
                body: None,
                content_type: None,
                depth: None,
                accept: ACCEPT_BINARY,
                credentials: chain.credentials,
            };
            let started = Instant::now();
            let resp = match self.send(&request) {
                Ok(resp) => resp,
                Err(e) => {
                    let err = DavError::from(e);
                    logger.trace_http_error(
                        PROTO,
                        "GET",
                        &chain.current,
                        &err.to_string(),
                        started.elapsed(),
                    );
                    self.transport_retry(logger, err, &mut attempt)?;
                    continue;
                }
            };
            let status = resp.status().as_u16();
            let headers = ResponseHeaders::of(&resp);
            logger.trace_http(&HttpCall {
                proto: PROTO,
                method: "GET",
                url: &chain.current,
                status,
                elapsed: started.elapsed(),
                note: Some("streamed download"),
                request: None,
                request_type: None,
                response: b"",
                response_type: None,
            });
            match status {
                200..=299 => {
                    self.inner.core.rate_limit().on_success();
                    return Ok(DavStream {
                        status,
                        etag: headers.etag,
                        content_type: headers.content_type,
                        last_modified: headers.last_modified,
                        body: Some(resp.into_body().into_reader()),
                    });
                }
                NOT_FOUND | GONE => {
                    self.inner.core.rate_limit().on_success();
                    return Ok(DavStream {
                        status,
                        etag: None,
                        content_type: None,
                        last_modified: None,
                        body: None,
                    });
                }
                _ => {}
            }
            if let Some(location) = headers.location.as_deref()
                && ResponseHeaders::is_redirect(status)
            {
                chain.follow(status, location, logger)?;
                continue;
            }
            let mut body = Vec::new();
            let _ = resp
                .into_body()
                .into_reader()
                .take(ERROR_BODY_LIMIT)
                .read_to_end(&mut body);
            match DavOutcome::classify(status, &body) {
                DavOutcome::Success => {}
                DavOutcome::Auth => {
                    return Err(DavError::Auth(format!("server returned {status}")));
                }
                DavOutcome::Vanished | DavOutcome::Fatal => {
                    return Err(DavError::HttpStatus {
                        status,
                        body: BodyExcerpt(&body).to_string(),
                    });
                }
                DavOutcome::Retryable => {
                    let delay = self.status_retry(&mut attempt, headers.retry_after, || {
                        format!("GET stream kept returning {status}")
                    })?;
                    if delay >= LONG_RETRY_THRESHOLD {
                        logger.warn(format_args!(
                            "DAV stream rate-limited (HTTP {status}); waiting {}s",
                            delay.as_secs()
                        ));
                    }
                    thread::sleep(delay);
                }
            }
        }
    }

    pub(super) fn stream_multistatus(
        &self,
        verb: Verb,
        url: &str,
        body: &[u8],
        depth: u8,
        base_url: &str,
    ) -> Result<MultiStatus, DavError> {
        let logger = self.logger();
        let method = verb.as_str();
        let mut attempt: u32 = 0;
        let mut chain = RedirectChain::new(url);
        loop {
            self.inner.core.rate_limit().cooldown().wait();
            let started = Instant::now();
            let request = WireRequest {
                verb,
                url: &chain.current,
                body: Some(body),
                content_type: Some(XML_BODY_TYPE),
                depth: Some(depth),
                accept: ACCEPT_DAV,
                credentials: chain.credentials,
            };
            let mut resp = match self.send(&request) {
                Ok(resp) => resp,
                Err(e) => {
                    self.transport_retry(logger, DavError::from(e), &mut attempt)?;
                    continue;
                }
            };
            let status = resp.status().as_u16();
            if ResponseHeaders::is_redirect(status) {
                let Some(location) = ResponseHeaders::text(&resp, "location") else {
                    return Err(DavError::Connect(format!(
                        "{method} {} returned {status} without Location",
                        chain.current
                    )));
                };
                chain.follow(status, &location, logger)?;
                continue;
            }
            let retry_after = ResponseHeaders::retry_after(&resp);
            if (200..300).contains(&status) {
                self.inner.core.rate_limit().on_success();
                logger.trace_http(&HttpCall {
                    proto: PROTO,
                    method,
                    url: &chain.current,
                    status,
                    elapsed: started.elapsed(),
                    note: Some("streamed multistatus"),
                    request: Some(body),
                    request_type: Some(XML_TRACE_TYPE),
                    response: b"",
                    response_type: None,
                });
                let parse_base = if chain.current == url {
                    base_url
                } else {
                    &chain.current
                };
                let reader =
                    BufReader::new(ControlStrippingReader::new(resp.into_body().into_reader()));
                let responses = MultistatusParser::parse(reader, parse_base)?;
                return Ok(MultiStatus {
                    status,
                    responses,
                    final_url: chain.current,
                });
            }
            let error_body = resp
                .body_mut()
                .with_config()
                .limit(ERROR_BODY_LIMIT)
                .read_to_vec()
                .map_err(|e| DavError::Transport(format!("reading error body: {e}")))?;
            logger.trace_http(&HttpCall {
                proto: PROTO,
                method,
                url: &chain.current,
                status,
                elapsed: started.elapsed(),
                note: None,
                request: Some(body),
                request_type: Some(XML_TRACE_TYPE),
                response: &error_body,
                response_type: None,
            });
            match DavOutcome::classify(status, &error_body) {
                DavOutcome::Success | DavOutcome::Vanished => {
                    return Ok(MultiStatus {
                        status,
                        responses: Vec::new(),
                        final_url: chain.current,
                    });
                }
                DavOutcome::Auth => {
                    return Err(DavError::Auth(format!(
                        "server returned {status}: {}",
                        BodyExcerpt(&error_body)
                    )));
                }
                DavOutcome::Fatal => {
                    return Err(DavError::HttpStatus {
                        status,
                        body: BodyExcerpt(&error_body).to_string(),
                    });
                }
                DavOutcome::Retryable => {
                    let delay = self.status_retry(&mut attempt, retry_after, || {
                        format!("{method} {url} kept returning {status}")
                    })?;
                    if delay >= LONG_RETRY_THRESHOLD {
                        logger.warn(format_args!(
                            "DAV {method} rate-limited (HTTP {status}); waiting {}s before retry {attempt}/{}",
                            delay.as_secs(),
                            self.inner.core.retry().max_retries,
                        ));
                    }
                    thread::sleep(delay);
                }
            }
        }
    }
}
