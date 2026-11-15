/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ureq::http::HeaderMap;

use crate::exchange::ews::error::EwsError;
use crate::exchange::ews::parse::{EnvelopeKind, SoapFault};
use crate::exchange::ews::retry::FaultDisposition;
use crate::exchange::ews::soap::{EnvelopeOptions, SoapAction};
use crate::exchange::ews::types::ServerVersion;
use crate::exchange::ews::xml::EwsRequest;
use crate::logging::{HttpCall, Level, Logger};
use crate::net::auth::Auth;
use crate::net::http::{
    AgentProfile, BodyExcerpt, ClientOptions, HttpCore, Methods, Redirects, RetryStats,
    TransportError,
};
use crate::net::retry::{Disposition, RetryAfter, RetryWait};

const MAX_BODY: u64 = 2 * 1024 * 1024 * 1024;
const LONG_RETRY_THRESHOLD: Duration = Duration::from_secs(10);
const UNAUTHORIZED: u16 = 401;
const FORBIDDEN: u16 = 403;
const ACCOUNT_LOCKED: u16 = 456;
const AFFINITY_COOKIE: &str = "X-BackEndOverrideCookie=";
const PROFILE: AgentProfile = AgentProfile {
    redirects: Redirects::Follow,
    methods: Methods::Standard,
    max_body: MAX_BODY,
};

struct Inner {
    core: HttpCore,
    auth: Mutex<Auth>,
    impersonated_smtp: Mutex<Option<String>>,
    anchor_mailbox: Mutex<Option<String>>,
    affinity_cookie: Mutex<Option<String>>,
    version: Mutex<VersionState>,
}

#[derive(Debug, Clone, Copy)]
struct VersionState {
    current: ServerVersion,
    accepted_ceiling: Option<ServerVersion>,
}

#[derive(Clone)]
pub struct EwsClient {
    inner: Arc<Inner>,
}

#[derive(Debug, Clone)]
pub struct SoapResponse {
    pub body: Vec<u8>,
    pub server_version: Option<ServerVersion>,
}

enum Attempt {
    Response {
        status: u16,
        body: Vec<u8>,
        retry_after: Option<Duration>,
    },
    Transport(EwsError),
}

enum Step {
    Done(SoapResponse),
    Fail(EwsError),
    Sleep(Duration),
    Downgrade(ServerVersion),
}

impl EwsClient {
    pub fn new(auth: Auth, options: ClientOptions) -> EwsClient {
        EwsClient {
            inner: Arc::new(Inner {
                core: HttpCore::new(options, PROFILE),
                auth: Mutex::new(auth),
                impersonated_smtp: Mutex::new(None),
                anchor_mailbox: Mutex::new(None),
                affinity_cookie: Mutex::new(None),
                version: Mutex::new(VersionState {
                    current: ServerVersion::Exchange2013Sp1,
                    accepted_ceiling: None,
                }),
            }),
        }
    }

    pub fn set_auth(&self, auth: Auth) {
        if let Ok(mut g) = self.inner.auth.lock() {
            *g = auth;
        }
    }

    pub fn set_anchor_mailbox(&self, smtp: Option<String>) {
        if let Ok(mut g) = self.inner.anchor_mailbox.lock() {
            *g = smtp;
        }
    }

    pub fn set_impersonation(&self, smtp: Option<String>) {
        if let Ok(mut g) = self.inner.impersonated_smtp.lock() {
            *g = smtp;
        }
    }

    pub fn logger(&self) -> Logger {
        self.inner.core.logger()
    }

    pub fn set_server_version(&self, version: ServerVersion) {
        if let Ok(mut g) = self.inner.version.lock() {
            g.current = g.accepted_ceiling.map_or(version, |c| version.min(c));
        }
    }

    fn pin_accepted_version(&self, version: ServerVersion) {
        if let Ok(mut g) = self.inner.version.lock() {
            g.current = version;
            g.accepted_ceiling = Some(version);
        }
    }

    pub fn server_version(&self) -> ServerVersion {
        self.inner
            .version
            .lock()
            .map(|g| g.current)
            .unwrap_or(ServerVersion::Exchange2013Sp1)
    }

    pub fn stats(&self) -> RetryStats {
        self.inner.core.stats().snapshot()
    }

    pub fn soap_calls(&self) -> u64 {
        self.inner.core.stats().requests()
    }

    pub fn call(&self, url: &str, request: &EwsRequest<'_>) -> Result<SoapResponse, EwsError> {
        self.inner.core.stats().note_request();
        let action = SoapAction(request.operation()).to_string();
        let mut attempt: u32 = 0;
        let mut downgraded = false;
        loop {
            self.inner.core.rate_limit().cooldown().wait();
            let sent_version = self.server_version();
            let envelope = self.wrap(request, sent_version);
            let step = match self.one_attempt(url, &envelope, &action) {
                Attempt::Response {
                    status,
                    body,
                    retry_after,
                } => self.on_response(
                    status,
                    body,
                    retry_after,
                    sent_version,
                    &action,
                    &mut attempt,
                ),
                Attempt::Transport(err) => self.on_transport(err, &mut attempt),
            };
            match step {
                Step::Done(response) => {
                    if downgraded {
                        self.pin_accepted_version(sent_version);
                    }
                    if let Some(v) = response.server_version {
                        self.set_server_version(v);
                    }
                    return Ok(response);
                }
                Step::Fail(e) => return Err(e),
                Step::Sleep(d) => thread::sleep(d),
                Step::Downgrade(v) => {
                    self.set_server_version(v);
                    downgraded = true;
                }
            }
        }
    }

    fn wrap(&self, request: &EwsRequest<'_>, version: ServerVersion) -> String {
        let smtp = self
            .inner
            .impersonated_smtp
            .lock()
            .ok()
            .and_then(|g| g.clone());
        EnvelopeOptions {
            version,
            impersonated_smtp: smtp.as_deref(),
        }
        .wrap(request)
    }

    fn auth_header(&self) -> String {
        self.inner
            .auth
            .lock()
            .map(|auth| auth.header_value())
            .unwrap_or_default()
    }

    fn anchor_header(&self) -> Option<String> {
        self.inner
            .anchor_mailbox
            .lock()
            .ok()
            .and_then(|g| g.clone())
    }

    fn affinity_cookie(&self) -> Option<String> {
        self.inner
            .affinity_cookie
            .lock()
            .ok()
            .and_then(|g| g.clone())
    }

    fn on_response(
        &self,
        status: u16,
        body: Vec<u8>,
        retry_after: Option<Duration>,
        sent_version: ServerVersion,
        action: &str,
        attempt: &mut u32,
    ) -> Step {
        let summary = EnvelopeKind::read(&body);
        if (200..300).contains(&status) {
            self.inner.core.rate_limit().on_success();
            return match summary {
                Ok(EnvelopeKind::Body { version }) => Step::Done(SoapResponse {
                    body,
                    server_version: version.to_server_version(),
                }),
                Ok(EnvelopeKind::Fault { fault, .. }) => {
                    self.on_fault(&fault, sent_version, action, attempt)
                }
                Err(e) => Step::Fail(e),
            };
        }
        if status != UNAUTHORIZED
            && status != FORBIDDEN
            && let Ok(EnvelopeKind::Fault { fault, .. }) = summary
        {
            return self.on_fault(&fault, sent_version, action, attempt);
        }
        match status {
            UNAUTHORIZED => Step::Fail(EwsError::Auth(format!(
                "server returned 401: {}",
                BodyExcerpt(&body)
            ))),
            ACCOUNT_LOCKED => Step::Fail(EwsError::Auth(format!(
                "account is locked (http 456); an administrator must unlock it: {}",
                BodyExcerpt(&body)
            ))),
            FORBIDDEN => Step::Fail(EwsError::HttpStatus {
                status,
                body: BodyExcerpt(&body).to_string(),
            }),
            _ => match Disposition::of_status(status) {
                Disposition::Fatal => Step::Fail(EwsError::HttpStatus {
                    status,
                    body: BodyExcerpt(&body).to_string(),
                }),
                Disposition::Retry => {
                    self.on_retryable_status(status, retry_after, action, attempt)
                }
            },
        }
    }

    fn on_retryable_status(
        &self,
        status: u16,
        retry_after: Option<Duration>,
        action: &str,
        attempt: &mut u32,
    ) -> Step {
        let core = &self.inner.core;
        let max_retries = core.retry().max_retries;
        let logger = core.logger();
        *attempt += 1;
        core.stats().note_retry();
        if *attempt > max_retries {
            return Step::Fail(EwsError::RetriesExhausted(format!(
                "{action} kept returning http {status}"
            )));
        }
        if retry_after.is_some() {
            core.stats().note_retry_after();
        }
        let chosen = core.throttle(retry_after);
        if chosen >= LONG_RETRY_THRESHOLD {
            logger.warn(format_args!(
                "EWS {action} rate-limited (http {status}); waiting {}",
                RetryWait(chosen)
            ));
        }
        logger.progress(format_args!(
            "retry {}/{max_retries} (http {status})",
            *attempt
        ));
        Step::Sleep(chosen)
    }

    fn on_transport(&self, err: EwsError, attempt: &mut u32) -> Step {
        let core = &self.inner.core;
        let max_retries = core.retry().max_retries;
        *attempt += 1;
        core.stats().note_retry();
        if *attempt > max_retries {
            return Step::Fail(err);
        }
        let delay = core.backoff(*attempt);
        if delay >= LONG_RETRY_THRESHOLD {
            core.logger().warn(format_args!(
                "transient transport failure ({err}); waiting {} before retry {}/{max_retries}",
                RetryWait(delay),
                *attempt
            ));
        }
        Step::Sleep(delay)
    }

    fn on_fault(
        &self,
        fault: &SoapFault,
        sent_version: ServerVersion,
        action: &str,
        attempt: &mut u32,
    ) -> Step {
        let core = &self.inner.core;
        let logger = core.logger();
        match fault.response_code.fault_disposition() {
            FaultDisposition::Fatal => Step::Fail(EwsError::SoapFault {
                code: fault.response_code.clone(),
                reason: fault.fault_string.clone(),
            }),
            FaultDisposition::Auth => Step::Fail(EwsError::Auth(fault.fault_string.clone())),
            FaultDisposition::VersionError => match sent_version.lower() {
                Some(next) => {
                    logger.warn(format_args!(
                        "EWS rejected schema version {} ({}); retrying as {}",
                        sent_version.as_str(),
                        fault.response_code,
                        next.as_str()
                    ));
                    Step::Downgrade(next)
                }
                None => Step::Fail(EwsError::SoapFault {
                    code: fault.response_code.clone(),
                    reason: format!(
                        "server rejected every supported EWS schema version (last tried {}): {}",
                        sent_version.as_str(),
                        fault.fault_string
                    ),
                }),
            },
            FaultDisposition::Retryable { delay } => {
                let max_retries = core.retry().max_retries;
                *attempt += 1;
                core.stats().note_retry();
                if *attempt > max_retries {
                    return Step::Fail(EwsError::RetriesExhausted(format!(
                        "{action} kept returning {code}",
                        code = fault.response_code
                    )));
                }
                let chosen = core.throttle(delay);
                if chosen >= LONG_RETRY_THRESHOLD {
                    logger.warn(format_args!(
                        "EWS SOAP fault {} ({}); waiting {} before retry {}/{max_retries}",
                        fault.response_code,
                        fault.fault_string,
                        RetryWait(chosen),
                        *attempt
                    ));
                }
                if logger.enabled(Level::Bodies) {
                    logger.progress(format_args!(
                        "retry {}/{max_retries} {action} after {chosen:?} ({})",
                        *attempt, fault.response_code,
                    ));
                }
                Step::Sleep(chosen)
            }
        }
    }

    fn one_attempt(&self, url: &str, body: &str, action: &str) -> Attempt {
        let core = &self.inner.core;
        let mut req = core
            .agent()
            .post(url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "text/xml; charset=utf-8")
            .header("Accept", "text/xml, application/soap+xml, application/xml")
            .header("Accept-Encoding", "gzip")
            .header("SOAPAction", action)
            .header("User-Agent", core.user_agent())
            .header("X-PreferServerAffinity", "True");
        if let Some(anchor) = self.anchor_header() {
            req = req.header("X-AnchorMailbox", anchor);
        }
        if let Some(cookie) = self.affinity_cookie() {
            req = req.header("X-BackEndOverrideCookie", cookie);
        }
        let logger = core.logger();
        let started = Instant::now();
        match req.send(body.as_bytes()) {
            Ok(mut resp) => {
                let status = resp.status().as_u16();
                let response_type = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(RetryAfter::parse_now);
                if let Some(cookie) = EwsClient::affinity_cookie_of(resp.headers())
                    && let Ok(mut g) = self.inner.affinity_cookie.lock()
                {
                    *g = Some(cookie);
                }
                match resp.body_mut().with_config().limit(MAX_BODY).read_to_vec() {
                    Ok(bytes) => {
                        logger.trace_http(&HttpCall {
                            proto: "EWS",
                            method: action,
                            url,
                            status,
                            elapsed: started.elapsed(),
                            note: None,
                            request: Some(body.as_bytes()),
                            request_type: Some("text/xml"),
                            response: &bytes,
                            response_type: response_type.as_deref(),
                        });
                        Attempt::Response {
                            status,
                            body: bytes,
                            retry_after,
                        }
                    }
                    Err(e) => Attempt::Transport(EwsError::Transport(format!(
                        "reading response body: {e}"
                    ))),
                }
            }
            Err(e) => {
                let err = EwsError::from(TransportError::from(e));
                logger.trace_http_error("EWS", action, url, &err.to_string(), started.elapsed());
                Attempt::Transport(err)
            }
        }
    }

    fn affinity_cookie_of(headers: &HeaderMap) -> Option<String> {
        headers
            .get_all("set-cookie")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .filter_map(|cookie| cookie.trim_start().strip_prefix(AFFINITY_COOKIE))
            .map(|rest| rest.split_once(';').map_or(rest, |(value, _)| value).trim())
            .find(|value| !value.is_empty())
            .map(str::to_owned)
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
    fn client_constructs_with_defaults() {
        let c = EwsClient::new(Auth::Bearer { token: "t".into() }, options(3));
        assert_eq!(c.server_version(), ServerVersion::Exchange2013Sp1);
        assert_eq!(c.stats(), RetryStats::default());
    }

    #[test]
    fn server_version_can_be_pinned() {
        let c = EwsClient::new(Auth::Bearer { token: "t".into() }, options(0));
        c.set_server_version(ServerVersion::Exchange2016);
        assert_eq!(c.server_version(), ServerVersion::Exchange2016);
    }

    #[test]
    fn accepted_downgrade_caps_reported_version() {
        let c = EwsClient::new(Auth::Bearer { token: "t".into() }, options(0));
        c.pin_accepted_version(ServerVersion::Exchange2013Sp1);
        c.set_server_version(ServerVersion::Exchange2016);
        assert_eq!(c.server_version(), ServerVersion::Exchange2013Sp1);
        c.set_server_version(ServerVersion::Exchange2010Sp2);
        assert_eq!(c.server_version(), ServerVersion::Exchange2010Sp2);
    }

    #[test]
    fn anchor_mailbox_round_trips() {
        let c = EwsClient::new(Auth::Bearer { token: "t".into() }, options(0));
        c.set_anchor_mailbox(Some("alice@x".to_owned()));
        assert_eq!(c.anchor_header().as_deref(), Some("alice@x"));
    }
}
