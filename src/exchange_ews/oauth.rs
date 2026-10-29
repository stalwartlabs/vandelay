/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::io::{self, Write};
use std::time::Duration;

use encodify::base64::{Base64, Padding, URL_SAFE};
use serde_json::Value;
use ureq::config::Config;
use ureq::tls::{RootCerts, TlsConfig};

use crate::exchange_ews::error::EwsError;

pub const SCOPE_APP_ONLY: &str = "https://outlook.office365.com/.default";
pub const SCOPE_DELEGATED: &str =
    "https://outlook.office365.com/EWS.AccessAsUser.All offline_access";

const JWT_SEGMENT: Base64 = URL_SAFE.with_padding(Padding::Optional).any_alphabet();

#[derive(Debug, Clone)]
pub struct AcquiredToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Option<u64>,
    pub tenant_id: Option<String>,
    pub upn: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone)]
pub enum OAuthFlow {
    PreAcquired {
        token: String,
    },
    ClientCredentials {
        tenant: String,
        client_id: String,
        client_secret: String,
    },
    DeviceCode {
        tenant: String,
        client_id: String,
    },
}

pub fn acquire(flow: &OAuthFlow, allow_invalid_certs: bool) -> Result<AcquiredToken, EwsError> {
    match flow {
        OAuthFlow::PreAcquired { token } => {
            let claims = decode_jwt_claims(token).unwrap_or_default();
            Ok(AcquiredToken {
                access_token: token.clone(),
                refresh_token: None,
                expires_in: None,
                tenant_id: claims.tenant_id,
                upn: claims.upn,
                name: claims.name,
            })
        }
        OAuthFlow::ClientCredentials {
            tenant,
            client_id,
            client_secret,
        } => client_credentials(tenant, client_id, client_secret, allow_invalid_certs),
        OAuthFlow::DeviceCode { tenant, client_id } => {
            device_code_flow(tenant, client_id, allow_invalid_certs)
        }
    }
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

fn token_endpoint(tenant: &str) -> String {
    format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token")
}

fn device_code_endpoint(tenant: &str) -> String {
    format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/devicecode")
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

fn client_credentials(
    tenant: &str,
    client_id: &str,
    client_secret: &str,
    allow_invalid_certs: bool,
) -> Result<AcquiredToken, EwsError> {
    let agent = build_agent(allow_invalid_certs);
    let body = form_encode(&[
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("scope", SCOPE_APP_ONLY),
        ("grant_type", "client_credentials"),
    ]);
    let endpoint = token_endpoint(tenant);
    let resp = agent
        .post(&endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(body.as_bytes())
        .map_err(|e| EwsError::OAuth(format!("token endpoint: {e}")))?;
    parse_token_response(resp)
}

fn device_code_flow(
    tenant: &str,
    client_id: &str,
    allow_invalid_certs: bool,
) -> Result<AcquiredToken, EwsError> {
    let agent = build_agent(allow_invalid_certs);
    let body = form_encode(&[("client_id", client_id), ("scope", SCOPE_DELEGATED)]);
    let endpoint = device_code_endpoint(tenant);
    let mut resp = agent
        .post(&endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(body.as_bytes())
        .map_err(|e| EwsError::OAuth(format!("device code endpoint: {e}")))?;
    let json: Value = resp
        .body_mut()
        .read_json()
        .map_err(|e| EwsError::OAuth(format!("device code body: {e}")))?;
    let device_code = json
        .get("device_code")
        .and_then(Value::as_str)
        .ok_or_else(|| EwsError::OAuth("device_code missing".to_owned()))?
        .to_owned();
    let user_code = json.get("user_code").and_then(Value::as_str).unwrap_or("?");
    let verify_uri = json
        .get("verification_uri")
        .and_then(Value::as_str)
        .unwrap_or("https://microsoft.com/devicelogin");
    let interval = json.get("interval").and_then(Value::as_u64).unwrap_or(5);
    let message = json.get("message").and_then(Value::as_str);
    if let Some(m) = message {
        eprintln!("{m}");
    } else {
        eprintln!("To sign in, open {verify_uri} and enter the code {user_code}");
    }
    io::stdout()
        .flush()
        .map_err(|e| EwsError::OAuth(format!("flushing stdout: {e}")))?;
    let endpoint = token_endpoint(tenant);
    let mut delay = Duration::from_secs(interval);
    let deadline = std::time::Instant::now()
        + Duration::from_secs(
            json.get("expires_in")
                .and_then(Value::as_u64)
                .unwrap_or(900),
        );
    loop {
        std::thread::sleep(delay);
        if std::time::Instant::now() >= deadline {
            return Err(EwsError::OAuth(
                "device code expired before the user completed sign-in".to_owned(),
            ));
        }
        let body = form_encode(&[
            ("client_id", client_id),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", &device_code),
        ]);
        let mut resp = agent
            .post(&endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send(body.as_bytes())
            .map_err(|e| EwsError::OAuth(format!("device code polling: {e}")))?;
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            return parse_token_response(resp);
        }
        let payload: Value = resp.body_mut().read_json().unwrap_or(Value::Null);
        let error = payload.get("error").and_then(Value::as_str).unwrap_or("");
        match error {
            "authorization_pending" => continue,
            "slow_down" => {
                delay = delay.saturating_add(Duration::from_secs(5));
                continue;
            }
            "expired_token" => {
                return Err(EwsError::OAuth(
                    "device code expired; restart the import".to_owned(),
                ));
            }
            "" => {
                return Err(EwsError::OAuth(format!(
                    "device code polling: http {status}"
                )));
            }
            other => {
                let desc = payload
                    .get("error_description")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                return Err(EwsError::OAuth(format!(
                    "device code polling: {other}: {desc}"
                )));
            }
        }
    }
}

fn parse_token_response(
    mut resp: ureq::http::Response<ureq::Body>,
) -> Result<AcquiredToken, EwsError> {
    let status = resp.status().as_u16();
    let json: Value = resp
        .body_mut()
        .read_json()
        .map_err(|e| EwsError::OAuth(format!("token body: {e}")))?;
    if !(200..300).contains(&status) {
        let desc = json
            .get("error_description")
            .and_then(Value::as_str)
            .unwrap_or("(no error_description)");
        return Err(EwsError::OAuth(format!("http {status}: {desc}")));
    }
    let access = json
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| EwsError::OAuth("access_token missing".to_owned()))?
        .to_owned();
    let refresh_token = json
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let expires_in = json.get("expires_in").and_then(Value::as_u64);
    let claims = decode_jwt_claims(&access).unwrap_or_default();
    Ok(AcquiredToken {
        access_token: access,
        refresh_token,
        expires_in,
        tenant_id: claims.tenant_id,
        upn: claims.upn,
        name: claims.name,
    })
}

pub fn refresh_with_token(
    tenant: &str,
    client_id: &str,
    refresh_token: &str,
    allow_invalid_certs: bool,
) -> Result<AcquiredToken, EwsError> {
    let agent = build_agent(allow_invalid_certs);
    let body = form_encode(&[
        ("client_id", client_id),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("scope", SCOPE_DELEGATED),
    ]);
    let endpoint = token_endpoint(tenant);
    let resp = agent
        .post(&endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(body.as_bytes())
        .map_err(|e| EwsError::OAuth(format!("refresh token endpoint: {e}")))?;
    parse_token_response(resp)
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
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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

    #[test]
    fn jwt_decoder_extracts_tid_and_upn() {
        let token = make_jwt("tenant-1", "alice@x.com", 9999999999);
        let claims = decode_jwt_claims(&token).unwrap();
        assert_eq!(claims.tenant_id.as_deref(), Some("tenant-1"));
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
        assert!(decode_jwt_claims("only.two").is_none());
    }

    #[test]
    fn pre_acquired_flow_decodes_claims() {
        let token = make_jwt("t-2", "bob@x", 9999999999);
        let acq = acquire(
            &OAuthFlow::PreAcquired {
                token: token.clone(),
            },
            false,
        )
        .unwrap();
        assert_eq!(acq.access_token, token);
        assert_eq!(acq.tenant_id.as_deref(), Some("t-2"));
        assert_eq!(acq.upn.as_deref(), Some("bob@x"));
    }

    #[test]
    fn urlencode_handles_special_chars() {
        assert_eq!(urlencode("a b/c"), "a%20b%2Fc");
        assert_eq!(urlencode("a.b-c_d"), "a.b-c_d");
    }
}
