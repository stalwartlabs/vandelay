/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::thread;
use std::time::Instant;

use super::wire::{RedirectChain, ResponseHeaders, Verb, WireRequest};
use super::{
    ACCEPT_BINARY, ACCEPT_DAV, DavClient, DavHttpResponse, ERROR_BODY_LIMIT, LONG_RETRY_THRESHOLD,
    MAX_BODY, PROTO, XML_BODY_TYPE,
};
use crate::dav::error::DavError;
use crate::dav::retry::DavOutcome;
use crate::logging::{HttpCall, Level};
use crate::net::http::BodyExcerpt;

impl DavClient {
    pub(super) fn execute(
        &self,
        verb: Verb,
        url: &str,
        xml: Option<(&str, u8)>,
    ) -> Result<DavHttpResponse, DavError> {
        let logger = self.logger();
        let method = verb.as_str();
        let max_retries = self.inner.core.retry().max_retries;
        let mut attempt: u32 = 0;
        let mut chain = RedirectChain::new(url);
        loop {
            self.inner.core.rate_limit().cooldown().wait();
            let request = WireRequest {
                verb,
                url: &chain.current,
                body: xml.map(|(body, _)| body.as_bytes()),
                content_type: xml.map(|_| XML_BODY_TYPE),
                depth: xml.map(|(_, depth)| depth),
                accept: if xml.is_some() {
                    ACCEPT_DAV
                } else {
                    ACCEPT_BINARY
                },
                credentials: chain.credentials,
            };
            let (status, body, headers) = match self.buffered_attempt(&request) {
                Ok(landed) => landed,
                Err(err) => {
                    self.transport_retry(logger, err, &mut attempt)?;
                    continue;
                }
            };
            if ResponseHeaders::is_redirect(status) {
                let Some(location) = headers.location.as_deref() else {
                    return Err(DavError::Connect(format!(
                        "{method} {} returned {status} without Location: {}",
                        chain.current,
                        BodyExcerpt(&body)
                    )));
                };
                chain.follow(status, location, logger)?;
                continue;
            }
            match DavOutcome::classify(status, &body) {
                DavOutcome::Success => {
                    self.inner.core.rate_limit().on_success();
                    return Ok(DavHttpResponse {
                        status,
                        body,
                        etag: headers.etag,
                        content_type: headers.content_type,
                        last_modified: headers.last_modified,
                    });
                }
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
                    return Err(DavError::Auth(format!(
                        "server returned {status}: {}",
                        BodyExcerpt(&body)
                    )));
                }
                DavOutcome::Fatal => {
                    return Err(DavError::HttpStatus {
                        status,
                        body: BodyExcerpt(&body).to_string(),
                    });
                }
                DavOutcome::Retryable => {
                    let delay = self.status_retry(&mut attempt, headers.retry_after, || {
                        format!("{method} {} kept returning {status}", chain.current)
                    })?;
                    if delay >= LONG_RETRY_THRESHOLD {
                        logger.warn(format_args!(
                            "DAV {method} rate-limited (HTTP {status}); waiting {}s before retry {attempt}/{max_retries}",
                            delay.as_secs(),
                        ));
                    }
                    if logger.enabled(Level::Bodies) {
                        logger.progress(format_args!(
                            "Retry {attempt}/{max_retries}: {method} {} after {delay:?} (HTTP {status}), body {}",
                            chain.current,
                            BodyExcerpt(&body),
                        ));
                    } else {
                        logger.progress(format_args!(
                            "Retry {attempt}/{max_retries} (HTTP {status})"
                        ));
                    }
                    thread::sleep(delay);
                }
            }
        }
    }

    fn buffered_attempt(
        &self,
        request: &WireRequest<'_>,
    ) -> Result<(u16, Vec<u8>, ResponseHeaders), DavError> {
        let logger = self.logger();
        let method = request.verb.as_str();
        let started = Instant::now();
        let mut resp = match self.send(request) {
            Ok(resp) => resp,
            Err(e) => {
                let err = DavError::from(e);
                logger.trace_http_error(
                    PROTO,
                    method,
                    request.url,
                    &err.to_string(),
                    started.elapsed(),
                );
                return Err(err);
            }
        };
        let status = resp.status().as_u16();
        let headers = ResponseHeaders::of(&resp);
        if ResponseHeaders::is_redirect(status) && headers.location.is_some() {
            logger.trace_http(&HttpCall {
                proto: PROTO,
                method,
                url: request.url,
                status,
                elapsed: started.elapsed(),
                note: headers.location.as_deref(),
                request: request.body,
                request_type: request.content_type,
                response: b"",
                response_type: None,
            });
            return Ok((status, Vec::new(), headers));
        }
        let limit = if (200..300).contains(&status) {
            MAX_BODY
        } else {
            ERROR_BODY_LIMIT
        };
        let body = resp
            .body_mut()
            .with_config()
            .limit(limit)
            .read_to_vec()
            .map_err(|e| DavError::Transport(format!("reading response body: {e}")))?;
        logger.trace_http(&HttpCall {
            proto: PROTO,
            method,
            url: request.url,
            status,
            elapsed: started.elapsed(),
            note: None,
            request: request.body,
            request_type: request.content_type,
            response: &body,
            response_type: headers.content_type.as_deref(),
        });
        Ok((status, body, headers))
    }
}
