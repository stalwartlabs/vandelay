/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::time::Duration;

use ureq::Body;
use ureq::http::Response;
use url::Url;

use super::{
    FOUND, MAX_DAV_REDIRECTS, MOVED_PERMANENTLY, PERMANENT_REDIRECT, SEE_OTHER, TEMPORARY_REDIRECT,
};
use crate::dav::error::DavError;
use crate::dav::href::Href;
use crate::logging::Logger;
use crate::net::retry::RetryAfter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Credentials {
    Send,
    Withhold,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Verb {
    Propfind,
    Report,
    Get,
}

impl Verb {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Verb::Propfind => "PROPFIND",
            Verb::Report => "REPORT",
            Verb::Get => "GET",
        }
    }
}

pub(super) struct WireRequest<'a> {
    pub(super) verb: Verb,
    pub(super) url: &'a str,
    pub(super) body: Option<&'a [u8]>,
    pub(super) content_type: Option<&'a str>,
    pub(super) depth: Option<u8>,
    pub(super) accept: &'a str,
    pub(super) credentials: Credentials,
}

pub(super) struct ResponseHeaders {
    pub(super) etag: Option<String>,
    pub(super) content_type: Option<String>,
    pub(super) last_modified: Option<String>,
    pub(super) location: Option<String>,
    pub(super) retry_after: Option<Duration>,
}

impl ResponseHeaders {
    pub(super) fn of(resp: &Response<Body>) -> ResponseHeaders {
        ResponseHeaders {
            etag: ResponseHeaders::text(resp, "etag"),
            content_type: ResponseHeaders::text(resp, "content-type"),
            last_modified: ResponseHeaders::text(resp, "last-modified"),
            location: ResponseHeaders::text(resp, "location"),
            retry_after: ResponseHeaders::retry_after(resp),
        }
    }

    pub(super) fn text(resp: &Response<Body>, name: &str) -> Option<String> {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    }

    pub(super) fn retry_after(resp: &Response<Body>) -> Option<Duration> {
        resp.headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(RetryAfter::parse_now)
    }

    pub(super) fn is_redirect(status: u16) -> bool {
        matches!(
            status,
            MOVED_PERMANENTLY | FOUND | SEE_OTHER | TEMPORARY_REDIRECT | PERMANENT_REDIRECT
        )
    }
}

pub(super) struct RedirectChain<'a> {
    pub(super) origin: &'a str,
    pub(super) current: String,
    pub(super) hops: u32,
    pub(super) credentials: Credentials,
}

impl<'a> RedirectChain<'a> {
    pub(super) fn new(origin: &'a str) -> RedirectChain<'a> {
        RedirectChain {
            origin,
            current: origin.to_owned(),
            hops: 0,
            credentials: Credentials::Send,
        }
    }

    pub(super) fn follow(
        &mut self,
        status: u16,
        location: &str,
        logger: Logger,
    ) -> Result<(), DavError> {
        if self.hops >= MAX_DAV_REDIRECTS {
            return Err(DavError::Connect(format!(
                "too many redirects ({}) starting from {}",
                self.hops, self.origin
            )));
        }
        self.hops += 1;
        let next = Href::join_absolute(&self.current, location)
            .map_err(|e| DavError::Connect(format!("redirect: {e}")))?;
        if self.credentials == Credentials::Send && !self.same_host(&next) {
            self.credentials = Credentials::Withhold;
            logger.progress(format_args!(
                "DAV redirect leaves origin host {}; dropping Authorization for {next}",
                self.origin
            ));
        }
        logger.progress(format_args!(
            "DAV redirect {status}: {} -> {next}",
            self.current
        ));
        self.current = next;
        Ok(())
    }

    pub(super) fn same_host(&self, next: &str) -> bool {
        match (Url::parse(self.origin), Url::parse(next)) {
            (Ok(a), Ok(b)) => match (a.host_str(), b.host_str()) {
                (Some(x), Some(y)) => {
                    x == y && a.port_or_known_default() == b.port_or_known_default()
                }
                _ => false,
            },
            _ => false,
        }
    }
}
