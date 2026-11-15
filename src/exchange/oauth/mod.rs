/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

pub mod refresh;
pub mod token;

use std::fmt;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use ureq::Body;
use ureq::http::Response;

use crate::net::encode::FormBody;
use crate::net::http::{AgentProfile, ClientOptions, HttpCore, Methods, Redirects};

pub use refresh::{RefreshPlan, RefreshSchedule, TokenRefresher};
pub use token::{AcquiredToken, DeviceCodeResponse, JwtClaims, TokenResponse};

const LOGIN_HOST: &str = "https://login.microsoftonline.com";
const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";
const MAX_TOKEN_BODY: u64 = 10 * 1024 * 1024;
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);
const REFRESH_BACKOFF_START: Duration = Duration::from_millis(500);
const REFRESH_BACKOFF_CAP: Duration = Duration::from_secs(8);
const PROFILE: AgentProfile = AgentProfile {
    redirects: Redirects::Follow,
    methods: Methods::Standard,
    max_body: MAX_TOKEN_BODY,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthError(pub String);

impl fmt::Display for OAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Ews,
    Graph,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authority(String);

#[derive(Debug, Clone, Copy)]
pub struct Pace {
    pub interval: Duration,
    pub slow_down: Duration,
}

#[derive(Debug)]
pub enum PollFailure {
    DeadlinePassed,
    Transport(String),
    Body(String),
    Refused { status: u16, payload: Value },
}

pub struct DeviceCodeFlow<'a> {
    pub response: &'a DeviceCodeResponse,
    pub client_id: &'a str,
    pub token_endpoint: &'a str,
    pub pace: Pace,
}

pub struct OAuthClient<'a> {
    core: HttpCore,
    authority: &'a Authority,
    client_id: &'a str,
    scope: &'static str,
    dialect: Dialect,
}

impl Authority {
    pub fn for_tenant(tenant: &str) -> Authority {
        Authority(format!("{LOGIN_HOST}/{tenant}"))
    }

    pub fn new(authority: String) -> Authority {
        Authority(authority)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    pub fn token_endpoint(&self) -> String {
        format!("{}/oauth2/v2.0/token", self.0)
    }

    pub fn device_code_endpoint(&self) -> String {
        format!("{}/oauth2/v2.0/devicecode", self.0)
    }
}

impl Pace {
    pub fn of(response: &DeviceCodeResponse) -> Pace {
        Pace {
            interval: Duration::from_secs(response.interval),
            slow_down: SLOW_DOWN_STEP,
        }
    }
}

impl Dialect {
    pub fn poll_error(self, failure: PollFailure) -> OAuthError {
        let text = match (self, failure) {
            (Dialect::Ews, PollFailure::DeadlinePassed) => {
                "device code expired before the user completed sign-in".to_owned()
            }
            (Dialect::Graph, PollFailure::DeadlinePassed) => {
                "device code expired before user completed sign-in".to_owned()
            }
            (_, PollFailure::Transport(e)) => format!("device code polling: {e}"),
            (_, PollFailure::Body(e)) => format!("token body: {e}"),
            (Dialect::Ews, PollFailure::Refused { status, payload }) => {
                Dialect::ews_refusal(status, &payload)
            }
            (Dialect::Graph, PollFailure::Refused { status, payload }) => {
                match TokenResponse::parse(status, &payload) {
                    TokenResponse::Expired => "device code expired; restart the import".to_owned(),
                    TokenResponse::Declined => {
                        "the user declined the device-code sign-in request".to_owned()
                    }
                    TokenResponse::BadCode => "device code rejected".to_owned(),
                    TokenResponse::Other { error, description } => {
                        format!("device code polling: {error}: {description}")
                    }
                    other => format!("device code polling: {other:?}"),
                }
            }
        };
        OAuthError(text)
    }

    fn ews_refusal(status: u16, payload: &Value) -> String {
        if (200..300).contains(&status) {
            return "access_token missing".to_owned();
        }
        if let TokenResponse::Expired = TokenResponse::parse(status, payload) {
            return "device code expired; restart the import".to_owned();
        }
        let error = payload
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if error.is_empty() {
            return format!("device code polling: http {status}");
        }
        let desc = payload
            .get("error_description")
            .and_then(Value::as_str)
            .unwrap_or_default();
        format!("device code polling: {error}: {desc}")
    }
}

impl DeviceCodeFlow<'_> {
    pub fn poll(
        &self,
        mut step: impl FnMut(&str, &FormBody) -> Result<(u16, Value), PollFailure>,
    ) -> Result<AcquiredToken, PollFailure> {
        let deadline = Instant::now() + Duration::from_secs(self.response.expires_in);
        let form = FormBody::from_pairs(&[
            ("client_id", self.client_id),
            ("grant_type", DEVICE_CODE_GRANT),
            ("device_code", &self.response.device_code),
        ]);
        let mut delay = self.pace.interval;
        loop {
            thread::sleep(delay);
            if Instant::now() >= deadline {
                return Err(PollFailure::DeadlinePassed);
            }
            let (status, payload) = step(self.token_endpoint, &form)?;
            match TokenResponse::parse(status, &payload) {
                TokenResponse::Ok(token) => return Ok(token),
                TokenResponse::Pending => {}
                TokenResponse::SlowDown => delay = delay.saturating_add(self.pace.slow_down),
                _ => return Err(PollFailure::Refused { status, payload }),
            }
        }
    }
}

impl<'a> OAuthClient<'a> {
    pub fn new(
        options: ClientOptions,
        authority: &'a Authority,
        client_id: &'a str,
        scope: &'static str,
        dialect: Dialect,
    ) -> OAuthClient<'a> {
        OAuthClient {
            core: HttpCore::new(options, PROFILE),
            authority,
            client_id,
            scope,
            dialect,
        }
    }

    fn post(&self, url: &str, form: &FormBody) -> Result<Response<Body>, ureq::Error> {
        self.core
            .agent()
            .post(url)
            .header("Content-Type", FORM_CONTENT_TYPE)
            .send(form.as_str().as_bytes())
    }

    fn read_json(&self, resp: &mut Response<Body>) -> Result<Value, ureq::Error> {
        resp.body_mut()
            .with_config()
            .limit(self.core.max_body())
            .read_json()
    }

    fn json(&self, resp: &mut Response<Body>) -> Value {
        self.read_json(resp).unwrap_or(Value::Null)
    }

    pub fn device_code(&self) -> Result<AcquiredToken, OAuthError> {
        let form = FormBody::from_pairs(&[("client_id", self.client_id), ("scope", self.scope)]);
        let mut resp = self
            .post(&self.authority.device_code_endpoint(), &form)
            .map_err(|e| OAuthError(format!("device code endpoint: {e}")))?;
        let json = self
            .read_json(&mut resp)
            .map_err(|e| OAuthError(format!("device code body: {e}")))?;
        let response = DeviceCodeResponse::parse(&json)?;
        response.announce(self.core.logger());
        let token_endpoint = self.authority.token_endpoint();
        DeviceCodeFlow {
            response: &response,
            client_id: self.client_id,
            token_endpoint: &token_endpoint,
            pace: Pace::of(&response),
        }
        .poll(|url, form| {
            let mut resp = self
                .post(url, form)
                .map_err(|e| PollFailure::Transport(e.to_string()))?;
            let payload = match self.dialect {
                Dialect::Ews if resp.status().is_success() => self
                    .read_json(&mut resp)
                    .map_err(|e| PollFailure::Body(e.to_string()))?,
                Dialect::Ews | Dialect::Graph => self.json(&mut resp),
            };
            Ok((resp.status().as_u16(), payload))
        })
        .map_err(|failure| self.dialect.poll_error(failure))
    }

    pub fn client_credentials(&self, secret: &str) -> Result<AcquiredToken, OAuthError> {
        let form = FormBody::from_pairs(&[
            ("client_id", self.client_id),
            ("client_secret", secret),
            ("scope", self.scope),
            ("grant_type", "client_credentials"),
        ]);
        let resp = self
            .post(&self.authority.token_endpoint(), &form)
            .map_err(|e| OAuthError(format!("token endpoint: {e}")))?;
        self.strict_token(resp)
    }

    pub fn refresh(&self, refresh_token: &str, attempts: u32) -> Result<AcquiredToken, OAuthError> {
        let form = FormBody::from_pairs(&[
            ("client_id", self.client_id),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("scope", self.scope),
        ]);
        let endpoint = self.authority.token_endpoint();
        match self.dialect {
            Dialect::Ews => {
                let resp = self
                    .post(&endpoint, &form)
                    .map_err(|e| OAuthError(format!("refresh token endpoint: {e}")))?;
                self.strict_token(resp)
            }
            Dialect::Graph => self.refresh_with_retries(&endpoint, &form, attempts),
        }
    }

    fn refresh_with_retries(
        &self,
        endpoint: &str,
        form: &FormBody,
        attempts: u32,
    ) -> Result<AcquiredToken, OAuthError> {
        let mut delay = REFRESH_BACKOFF_START;
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let mut resp = match self.post(endpoint, form) {
                Ok(r) => r,
                Err(e) if attempt < attempts => {
                    thread::sleep(delay);
                    delay = (delay * 2).min(REFRESH_BACKOFF_CAP);
                    self.core.logger().info(format_args!(
                        "Retrying the Graph token refresh after a transport error: {e}"
                    ));
                    continue;
                }
                Err(e) => return Err(OAuthError(format!("refresh endpoint: {e}"))),
            };
            let status = resp.status().as_u16();
            let payload = self.json(&mut resp);
            if resp.status().is_server_error() && attempt < attempts {
                thread::sleep(delay);
                delay = (delay * 2).min(REFRESH_BACKOFF_CAP);
                continue;
            }
            return match TokenResponse::parse(status, &payload) {
                TokenResponse::Ok(t) => Ok(t),
                other => Err(OAuthError(format!(
                    "refresh failed (http {status}): {other:?}"
                ))),
            };
        }
    }

    fn strict_token(&self, mut resp: Response<Body>) -> Result<AcquiredToken, OAuthError> {
        let status = resp.status().as_u16();
        let json = self
            .read_json(&mut resp)
            .map_err(|e| OAuthError(format!("token body: {e}")))?;
        if !(200..300).contains(&status) {
            let desc = json
                .get("error_description")
                .and_then(Value::as_str)
                .unwrap_or("(no error_description)");
            return Err(OAuthError(format!("http {status}: {desc}")));
        }
        AcquiredToken::from_json(&json).ok_or_else(|| OAuthError("access_token missing".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn response() -> DeviceCodeResponse {
        DeviceCodeResponse {
            device_code: "DC".to_owned(),
            user_code: "U".to_owned(),
            verification_uri: "x".to_owned(),
            interval: 0,
            expires_in: 5,
            message: None,
        }
    }

    fn poll(
        reply: impl FnMut(&str, &FormBody) -> Result<(u16, Value), PollFailure>,
    ) -> Result<AcquiredToken, PollFailure> {
        let response = response();
        DeviceCodeFlow {
            response: &response,
            client_id: "client-id",
            token_endpoint: "https://login.x/common/oauth2/v2.0/token",
            pace: Pace {
                interval: Duration::from_millis(1),
                slow_down: Duration::from_millis(1),
            },
        }
        .poll(reply)
    }

    #[test]
    fn authority_endpoints() {
        let authority = Authority::for_tenant("common");
        assert_eq!(
            authority.as_str(),
            "https://login.microsoftonline.com/common"
        );
        assert_eq!(
            authority.token_endpoint(),
            "https://login.microsoftonline.com/common/oauth2/v2.0/token"
        );
        assert_eq!(
            authority.device_code_endpoint(),
            "https://login.microsoftonline.com/common/oauth2/v2.0/devicecode"
        );
    }

    #[test]
    fn poll_sends_the_device_code_grant() {
        let token = poll(|endpoint, form| {
            assert!(endpoint.ends_with("/token"));
            assert_eq!(
                form.as_str(),
                "client_id=client-id&grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code=DC"
            );
            Ok((200, json!({"access_token": "a.b.c"})))
        })
        .unwrap();
        assert_eq!(token.access_token, "a.b.c");
    }

    #[test]
    fn ews_and_graph_keep_their_refusal_texts() {
        let declined = || {
            poll(|_, _| {
                Ok((
                    400,
                    json!({"error": "authorization_declined", "error_description": "no"}),
                ))
            })
            .unwrap_err()
        };
        assert_eq!(
            Dialect::Ews.poll_error(declined()).0,
            "device code polling: authorization_declined: no"
        );
        assert_eq!(
            Dialect::Graph.poll_error(declined()).0,
            "the user declined the device-code sign-in request"
        );
        let silent = poll(|_, _| Ok((500, Value::Null))).unwrap_err();
        assert_eq!(
            Dialect::Ews.poll_error(silent).0,
            "device code polling: http 500"
        );
        assert_eq!(
            Dialect::Ews.poll_error(PollFailure::DeadlinePassed).0,
            "device code expired before the user completed sign-in"
        );
        assert_eq!(
            Dialect::Graph.poll_error(PollFailure::DeadlinePassed).0,
            "device code expired before user completed sign-in"
        );
    }

    #[test]
    fn poll_slows_down_then_succeeds() {
        let mut calls = 0;
        poll(|_, _| {
            calls += 1;
            Ok(match calls {
                1 => (400, json!({"error": "slow_down"})),
                2 => (400, json!({"error": "authorization_pending"})),
                _ => (200, json!({"access_token": "t"})),
            })
        })
        .unwrap();
        assert_eq!(calls, 3);
    }
}
