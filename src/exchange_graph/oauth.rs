/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::io::{self, Write};
use std::time::{Duration, Instant};

use encodify::base64::{Base64, Padding, URL_SAFE};
use serde_json::Value;
use ureq::config::Config;
use ureq::tls::{RootCerts, TlsConfig};

use crate::exchange_graph::error::GraphError;

pub const SCOPES: &str = "offline_access User.Read Mail.Read MailboxSettings.Read Calendars.Read Contacts.Read Files.Read";

const JWT_SEGMENT: Base64 = URL_SAFE.with_padding(Padding::Optional).any_alphabet();

pub fn default_authority(tenant: &str) -> String {
    format!("https://login.microsoftonline.com/{tenant}")
}

pub fn device_code_endpoint(authority: &str) -> String {
    format!("{authority}/oauth2/v2.0/devicecode")
}

pub fn token_endpoint(authority: &str) -> String {
    format!("{authority}/oauth2/v2.0/token")
}

#[derive(Debug, Clone)]
pub enum OAuthFlow {
    PreAcquired {
        token: String,
    },
    DeviceCode {
        authority: String,
        client_id: String,
    },
}

#[derive(Debug, Clone, Default)]
pub struct JwtClaims {
    pub tenant_id: Option<String>,
    pub upn: Option<String>,
    pub name: Option<String>,
    pub exp: Option<u64>,
}

pub fn decode_jwt_claims(token: &str) -> Option<JwtClaims> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let bytes = JWT_SEGMENT.decode(payload).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    Some(JwtClaims {
        tenant_id: value.get("tid").and_then(Value::as_str).map(str::to_owned),
        upn: value
            .get("upn")
            .and_then(Value::as_str)
            .or_else(|| value.get("preferred_username").and_then(Value::as_str))
            .map(str::to_owned),
        name: value.get("name").and_then(Value::as_str).map(str::to_owned),
        exp: value.get("exp").and_then(Value::as_u64),
    })
}

#[derive(Debug, Clone)]
pub struct AcquiredToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Option<u64>,
    pub tenant_id: Option<String>,
    pub upn: Option<String>,
    pub name: Option<String>,
}

fn build_agent(allow_invalid_certs: bool) -> ureq::Agent {
    let config: Config = Config::builder()
        .http_status_as_error(false)
        .tls_config(
            TlsConfig::builder()
                .unversioned_rustls_crypto_provider(std::sync::Arc::new(
                    rustls::crypto::aws_lc_rs::default_provider(),
                ))
                .root_certs(RootCerts::PlatformVerifier)
                .disable_verification(allow_invalid_certs)
                .build(),
        )
        .build();
    config.new_agent()
}

pub fn acquire(flow: &OAuthFlow, allow_invalid_certs: bool) -> Result<AcquiredToken, GraphError> {
    match flow {
        OAuthFlow::PreAcquired { token } => Ok(token_from_string(token.clone())),
        OAuthFlow::DeviceCode {
            authority,
            client_id,
        } => device_code_flow(authority, client_id, allow_invalid_certs),
    }
}

fn token_from_string(token: String) -> AcquiredToken {
    let claims = decode_jwt_claims(&token).unwrap_or_default();
    AcquiredToken {
        access_token: token,
        refresh_token: None,
        expires_in: None,
        tenant_id: claims.tenant_id,
        upn: claims.upn,
        name: claims.name,
    }
}

#[derive(Debug, Clone)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
    pub expires_in: u64,
    pub message: Option<String>,
}

pub fn parse_device_code_response(json: &Value) -> Result<DeviceCodeResponse, GraphError> {
    let device_code = json
        .get("device_code")
        .and_then(Value::as_str)
        .ok_or_else(|| GraphError::OAuth("device_code missing".to_owned()))?
        .to_owned();
    let user_code = json
        .get("user_code")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_owned();
    let verification_uri = json
        .get("verification_uri")
        .and_then(Value::as_str)
        .unwrap_or("https://microsoft.com/devicelogin")
        .to_owned();
    let interval = json.get("interval").and_then(Value::as_u64).unwrap_or(5);
    let expires_in = json
        .get("expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(900);
    let message = json
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(DeviceCodeResponse {
        device_code,
        user_code,
        verification_uri,
        interval,
        expires_in,
        message,
    })
}

#[derive(Debug, Clone)]
pub enum TokenResponse {
    Ok(AcquiredToken),
    Pending,
    SlowDown,
    Expired,
    Declined,
    BadCode,
    Other { error: String, description: String },
}

pub fn parse_token_response(status: u16, json: &Value) -> TokenResponse {
    if (200..300).contains(&status) {
        let Some(access) = json.get("access_token").and_then(Value::as_str) else {
            return TokenResponse::Other {
                error: "missing_access_token".to_owned(),
                description: String::new(),
            };
        };
        let claims = decode_jwt_claims(access).unwrap_or_default();
        return TokenResponse::Ok(AcquiredToken {
            access_token: access.to_owned(),
            refresh_token: json
                .get("refresh_token")
                .and_then(Value::as_str)
                .map(str::to_owned),
            expires_in: json.get("expires_in").and_then(Value::as_u64),
            tenant_id: claims.tenant_id,
            upn: claims.upn,
            name: claims.name,
        });
    }
    let error = json
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let description = json
        .get("error_description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    match error.as_str() {
        "authorization_pending" => TokenResponse::Pending,
        "slow_down" => TokenResponse::SlowDown,
        "expired_token" => TokenResponse::Expired,
        "authorization_declined" => TokenResponse::Declined,
        "bad_verification_code" => TokenResponse::BadCode,
        _ => TokenResponse::Other { error, description },
    }
}

fn device_code_flow(
    authority: &str,
    client_id: &str,
    allow_invalid_certs: bool,
) -> Result<AcquiredToken, GraphError> {
    let agent = build_agent(allow_invalid_certs);
    let body = form_encode(&[("client_id", client_id), ("scope", SCOPES)]);
    let endpoint = device_code_endpoint(authority);
    let mut resp = agent
        .post(&endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(body.as_bytes())
        .map_err(|e| GraphError::OAuth(format!("device code endpoint: {e}")))?;
    let json: Value = resp
        .body_mut()
        .read_json()
        .map_err(|e| GraphError::OAuth(format!("device code body: {e}")))?;
    let response = parse_device_code_response(&json)?;
    if let Some(m) = response.message.as_deref() {
        eprintln!("{m}");
    } else {
        eprintln!(
            "To sign in, open {} and enter the code {}",
            response.verification_uri, response.user_code
        );
    }
    let _ = io::stdout().flush();
    let token_endpoint = token_endpoint(authority);
    let deadline = Instant::now() + Duration::from_secs(response.expires_in);
    let mut delay = Duration::from_secs(response.interval);
    loop {
        std::thread::sleep(delay);
        if Instant::now() >= deadline {
            return Err(GraphError::OAuth(
                "device code expired before user completed sign-in".to_owned(),
            ));
        }
        let body = form_encode(&[
            ("client_id", client_id),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", response.device_code.as_str()),
        ]);
        let mut resp = agent
            .post(&token_endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send(body.as_bytes())
            .map_err(|e| GraphError::OAuth(format!("device code polling: {e}")))?;
        let status = resp.status().as_u16();
        let payload: Value = resp.body_mut().read_json().unwrap_or(Value::Null);
        match parse_token_response(status, &payload) {
            TokenResponse::Ok(t) => return Ok(t),
            TokenResponse::Pending => continue,
            TokenResponse::SlowDown => {
                delay = delay.saturating_add(Duration::from_secs(5));
            }
            TokenResponse::Expired => {
                return Err(GraphError::OAuth(
                    "device code expired; restart the import".to_owned(),
                ));
            }
            TokenResponse::Declined => {
                return Err(GraphError::OAuth(
                    "the user declined the device-code sign-in request".to_owned(),
                ));
            }
            TokenResponse::BadCode => {
                return Err(GraphError::OAuth("device code rejected".to_owned()));
            }
            TokenResponse::Other { error, description } => {
                return Err(GraphError::OAuth(format!(
                    "device code polling: {error}: {description}"
                )));
            }
        }
    }
}

pub fn refresh_access_token(
    authority: &str,
    client_id: &str,
    refresh_token: &str,
    allow_invalid_certs: bool,
) -> Result<AcquiredToken, GraphError> {
    let agent = build_agent(allow_invalid_certs);
    let body = form_encode(&[
        ("client_id", client_id),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("scope", SCOPES),
    ]);
    let endpoint = token_endpoint(authority);
    let mut delay = Duration::from_millis(500);
    let mut attempts: u32 = 0;
    loop {
        attempts += 1;
        let send = agent
            .post(&endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send(body.as_bytes());
        let mut resp = match send {
            Ok(r) => r,
            Err(e) if attempts < 4 => {
                std::thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_secs(8));
                eprintln!("graph token refresh transport error: {e}; retrying");
                continue;
            }
            Err(e) => return Err(GraphError::OAuth(format!("refresh endpoint: {e}"))),
        };
        let status = resp.status().as_u16();
        let payload: Value = resp.body_mut().read_json().unwrap_or(Value::Null);
        if matches!(status, 500..=599) && attempts < 4 {
            std::thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_secs(8));
            continue;
        }
        return match parse_token_response(status, &payload) {
            TokenResponse::Ok(t) => Ok(t),
            other => Err(GraphError::OAuth(format!(
                "refresh failed (http {status}): {other:?}"
            ))),
        };
    }
}

fn form_encode(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(&urlencode(k));
        out.push('=');
        out.push_str(&urlencode(v));
    }
    out
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                use std::fmt::Write;
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

pub fn run_device_code_polling_against<F>(
    authority: &str,
    client_id: &str,
    response: &DeviceCodeResponse,
    mut step: F,
) -> Result<AcquiredToken, GraphError>
where
    F: FnMut(&str, &str) -> Result<(u16, Value), GraphError>,
{
    let token_endpoint = token_endpoint(authority);
    let deadline = Instant::now() + Duration::from_secs(response.expires_in.max(1));
    let mut delay = Duration::from_millis(20);
    loop {
        if Instant::now() >= deadline {
            return Err(GraphError::OAuth(
                "device code expired before user completed sign-in".to_owned(),
            ));
        }
        std::thread::sleep(delay);
        let body = form_encode(&[
            ("client_id", client_id),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", response.device_code.as_str()),
        ]);
        let (status, payload) = step(&token_endpoint, &body)?;
        match parse_token_response(status, &payload) {
            TokenResponse::Ok(t) => return Ok(t),
            TokenResponse::Pending => continue,
            TokenResponse::SlowDown => {
                delay = delay.saturating_add(Duration::from_millis(20));
            }
            TokenResponse::Expired => {
                return Err(GraphError::OAuth("device code expired".to_owned()));
            }
            TokenResponse::Declined => {
                return Err(GraphError::OAuth("device code declined by user".to_owned()));
            }
            TokenResponse::BadCode => {
                return Err(GraphError::OAuth("device code rejected".to_owned()));
            }
            TokenResponse::Other { error, description } => {
                return Err(GraphError::OAuth(format!("{error}: {description}")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use encodify::base64::{STANDARD, STANDARD_NO_PAD, URL_SAFE_NO_PAD};

    fn make_jwt(tid: &str, upn: &str, exp: u64) -> String {
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
        let claims = format!(r#"{{"tid":"{tid}","upn":"{upn}","exp":{exp}}}"#);
        let payload = URL_SAFE_NO_PAD.encode(claims.as_bytes());
        format!("{header}.{payload}.")
    }

    // Device code consent covers every --objects surface, including the drive.
    #[test]
    fn scopes_cover_every_surface() {
        for required in [
            "offline_access",
            "Mail.Read",
            "MailboxSettings.Read",
            "Calendars.Read",
            "Contacts.Read",
            "Files.Read",
        ] {
            assert!(
                SCOPES.split_whitespace().any(|s| s == required),
                "missing {required}"
            );
        }
    }

    #[test]
    fn jwt_claim_extraction() {
        let token = make_jwt("t-1", "alice@x.com", 9999999999);
        let claims = decode_jwt_claims(&token).unwrap();
        assert_eq!(claims.tenant_id.as_deref(), Some("t-1"));
        assert_eq!(claims.upn.as_deref(), Some("alice@x.com"));
        assert_eq!(claims.exp, Some(9999999999));
    }

    #[test]
    fn jwt_payload_decodes_in_either_alphabet_with_or_without_padding() {
        let claims = r#"{"tid":"t-3","upn":"??>>@xy","exp":1}"#;
        for engine in [URL_SAFE_NO_PAD, URL_SAFE, STANDARD_NO_PAD, STANDARD] {
            let token = format!("h.{}.s", engine.encode(claims));
            let decoded = decode_jwt_claims(&token).unwrap();
            assert_eq!(decoded.upn.as_deref(), Some("??>>@xy"), "{token}");
        }
        assert!(decode_jwt_claims("h.eyJ0aWQiOiJ0LTMifQ.s").is_some());
        assert!(decode_jwt_claims("h.eyJ0aWQiOiJ0LTMifQ=.s").is_none());
        assert!(decode_jwt_claims("h.eyJ0 aWQiOiJ0LTMifQ.s").is_none());
    }

    #[test]
    fn malformed_token_returns_none() {
        assert!(decode_jwt_claims("garbage").is_none());
    }

    #[test]
    fn device_code_response_parses_required_fields() {
        let json = serde_json::json!({
            "device_code": "ABCDEF",
            "user_code": "XXX-YYY",
            "verification_uri": "https://microsoft.com/devicelogin",
            "interval": 5,
            "expires_in": 900,
            "message": "go sign in"
        });
        let r = parse_device_code_response(&json).unwrap();
        assert_eq!(r.device_code, "ABCDEF");
        assert_eq!(r.user_code, "XXX-YYY");
        assert_eq!(r.interval, 5);
        assert_eq!(r.expires_in, 900);
    }

    #[test]
    fn token_response_pending() {
        let json = serde_json::json!({
            "error": "authorization_pending",
            "error_description": "user has not yet completed"
        });
        assert!(matches!(
            parse_token_response(400, &json),
            TokenResponse::Pending
        ));
    }

    #[test]
    fn token_response_declined() {
        let json = serde_json::json!({"error": "authorization_declined"});
        assert!(matches!(
            parse_token_response(400, &json),
            TokenResponse::Declined
        ));
    }

    #[test]
    fn token_response_bad_code() {
        let json = serde_json::json!({"error": "bad_verification_code"});
        assert!(matches!(
            parse_token_response(400, &json),
            TokenResponse::BadCode
        ));
    }

    #[test]
    fn token_response_expired() {
        let json = serde_json::json!({"error": "expired_token"});
        assert!(matches!(
            parse_token_response(400, &json),
            TokenResponse::Expired
        ));
    }

    #[test]
    fn token_response_success_includes_refresh() {
        let token = make_jwt("t-2", "bob@x.com", 9999999999);
        let json = serde_json::json!({
            "access_token": token,
            "refresh_token": "REFRESH",
            "expires_in": 3599
        });
        match parse_token_response(200, &json) {
            TokenResponse::Ok(t) => {
                assert!(t.access_token.starts_with("eyJhbGciOiJub25lIn0"));
                assert_eq!(t.refresh_token.as_deref(), Some("REFRESH"));
                assert_eq!(t.expires_in, Some(3599));
                assert_eq!(t.upn.as_deref(), Some("bob@x.com"));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn urlencode_escapes_special_chars() {
        assert_eq!(urlencode("a b/c"), "a%20b%2Fc");
        assert_eq!(urlencode("plain.123_-"), "plain.123_-");
    }

    #[test]
    fn default_authority_uses_login_microsoftonline_com() {
        assert_eq!(
            default_authority("common"),
            "https://login.microsoftonline.com/common"
        );
        assert_eq!(
            default_authority("contoso.onmicrosoft.com"),
            "https://login.microsoftonline.com/contoso.onmicrosoft.com"
        );
    }
}
