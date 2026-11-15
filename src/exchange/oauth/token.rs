/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use encodify::base64::{Base64, Padding, URL_SAFE};
use serde_json::Value;

use crate::exchange::oauth::OAuthError;
use crate::logging::Logger;

const JWT_SEGMENT: Base64 = URL_SAFE.with_padding(Padding::Optional).any_alphabet();
const DEFAULT_USER_CODE: &str = "?";
const DEFAULT_VERIFICATION_URI: &str = "https://microsoft.com/devicelogin";
const DEFAULT_INTERVAL_SECS: u64 = 5;
const DEFAULT_DEVICE_CODE_LIFETIME_SECS: u64 = 900;

#[derive(Debug, Clone, Default)]
pub struct JwtClaims {
    pub tenant_id: Option<String>,
    pub upn: Option<String>,
    pub name: Option<String>,
    pub exp: Option<u64>,
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

#[derive(Debug, Clone)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
    pub expires_in: u64,
    pub message: Option<String>,
}

impl JwtClaims {
    pub fn decode(token: &str) -> Option<JwtClaims> {
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
}

impl AcquiredToken {
    pub fn from_bearer(token: String) -> AcquiredToken {
        AcquiredToken::with_claims(token, None, None)
    }

    fn with_claims(
        access_token: String,
        refresh_token: Option<String>,
        expires_in: Option<u64>,
    ) -> AcquiredToken {
        let claims = JwtClaims::decode(&access_token).unwrap_or_default();
        AcquiredToken {
            access_token,
            refresh_token,
            expires_in,
            tenant_id: claims.tenant_id,
            upn: claims.upn,
            name: claims.name,
        }
    }

    pub fn from_json(json: &Value) -> Option<AcquiredToken> {
        let access = json.get("access_token").and_then(Value::as_str)?;
        Some(AcquiredToken::with_claims(
            access.to_owned(),
            json.get("refresh_token")
                .and_then(Value::as_str)
                .map(str::to_owned),
            json.get("expires_in").and_then(Value::as_u64),
        ))
    }

    pub fn deadline_unix(&self, now: u64) -> Option<u64> {
        match self.expires_in {
            Some(expires_in) => Some(now.saturating_add(expires_in)),
            None => JwtClaims::decode(&self.access_token).and_then(|c| c.exp),
        }
    }
}

impl TokenResponse {
    pub fn parse(status: u16, json: &Value) -> TokenResponse {
        if (200..300).contains(&status) {
            return AcquiredToken::from_json(json).map_or_else(
                || TokenResponse::Other {
                    error: "missing_access_token".to_owned(),
                    description: String::new(),
                },
                TokenResponse::Ok,
            );
        }
        let error = json
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_default();
        hashify::map!(
            error.as_bytes(),
            TokenResponse,
            "authorization_pending" => TokenResponse::Pending,
            "slow_down" => TokenResponse::SlowDown,
            "expired_token" => TokenResponse::Expired,
            "authorization_declined" => TokenResponse::Declined,
            "bad_verification_code" => TokenResponse::BadCode,
        )
        .cloned()
        .unwrap_or_else(|| TokenResponse::Other {
            error: error.to_owned(),
            description: json
                .get("error_description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        })
    }
}

impl DeviceCodeResponse {
    pub fn parse(json: &Value) -> Result<DeviceCodeResponse, OAuthError> {
        let text = |key: &str, default: &str| {
            json.get(key)
                .and_then(Value::as_str)
                .unwrap_or(default)
                .to_owned()
        };
        let device_code = json
            .get("device_code")
            .and_then(Value::as_str)
            .ok_or_else(|| OAuthError("device_code missing".to_owned()))?
            .to_owned();
        Ok(DeviceCodeResponse {
            device_code,
            user_code: text("user_code", DEFAULT_USER_CODE),
            verification_uri: text("verification_uri", DEFAULT_VERIFICATION_URI),
            interval: json
                .get("interval")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_INTERVAL_SECS),
            expires_in: json
                .get("expires_in")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_DEVICE_CODE_LIFETIME_SECS),
            message: json
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }

    pub fn announce(&self, logger: Logger) {
        match self.message.as_deref() {
            Some(message) => logger.notice(format_args!("{message}")),
            None => logger.notice(format_args!(
                "To sign in, open {} and enter the code {}",
                self.verification_uri, self.user_code
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use encodify::base64::{STANDARD, STANDARD_NO_PAD, URL_SAFE_NO_PAD};
    use serde_json::json;

    fn make_jwt(tid: &str, upn: &str, exp: u64) -> String {
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
        let claims = format!(r#"{{"tid":"{tid}","upn":"{upn}","exp":{exp}}}"#);
        let payload = URL_SAFE_NO_PAD.encode(claims.as_bytes());
        format!("{header}.{payload}.")
    }

    #[test]
    fn jwt_decoder_extracts_tid_upn_and_exp() {
        let claims = JwtClaims::decode(&make_jwt("tenant-1", "alice@x.com", 9999999999)).unwrap();
        assert_eq!(claims.tenant_id.as_deref(), Some("tenant-1"));
        assert_eq!(claims.upn.as_deref(), Some("alice@x.com"));
        assert_eq!(claims.exp, Some(9999999999));
    }

    #[test]
    fn jwt_payload_decodes_in_either_alphabet_with_or_without_padding() {
        let claims = r#"{"tid":"t-3","upn":"??>>@xy","exp":1}"#;
        for engine in [URL_SAFE_NO_PAD, URL_SAFE, STANDARD_NO_PAD, STANDARD] {
            let token = format!("h.{}.s", engine.encode(claims));
            let decoded = JwtClaims::decode(&token).unwrap();
            assert_eq!(decoded.upn.as_deref(), Some("??>>@xy"), "{token}");
        }
        assert!(JwtClaims::decode("h.eyJ0aWQiOiJ0LTMifQ.s").is_some());
        assert!(JwtClaims::decode("h.eyJ0aWQiOiJ0LTMifQ=.s").is_none());
        assert!(JwtClaims::decode("h.eyJ0 aWQiOiJ0LTMifQ.s").is_none());
    }

    #[test]
    fn malformed_token_returns_none() {
        assert!(JwtClaims::decode("garbage").is_none());
        assert!(JwtClaims::decode("only.two").is_none());
    }

    #[test]
    fn bearer_token_carries_its_claims() {
        let token = make_jwt("t-2", "bob@x", 9999999999);
        let acq = AcquiredToken::from_bearer(token.clone());
        assert_eq!(acq.access_token, token);
        assert_eq!(acq.tenant_id.as_deref(), Some("t-2"));
        assert_eq!(acq.upn.as_deref(), Some("bob@x"));
        assert_eq!(acq.deadline_unix(0), Some(9999999999));
    }

    #[test]
    fn expires_in_wins_over_the_jwt_expiry() {
        let token = make_jwt("t", "u", 9999999999);
        let acq =
            AcquiredToken::from_json(&json!({"access_token": token, "expires_in": 60})).unwrap();
        assert_eq!(acq.deadline_unix(1000), Some(1060));
    }

    #[test]
    fn token_response_classifies_errors() {
        let classify = |error: &str| TokenResponse::parse(400, &json!({"error": error}));
        assert!(matches!(
            classify("authorization_pending"),
            TokenResponse::Pending
        ));
        assert!(matches!(classify("slow_down"), TokenResponse::SlowDown));
        assert!(matches!(classify("expired_token"), TokenResponse::Expired));
        assert!(matches!(
            classify("authorization_declined"),
            TokenResponse::Declined
        ));
        assert!(matches!(
            classify("bad_verification_code"),
            TokenResponse::BadCode
        ));
        assert!(matches!(
            classify("invalid_grant"),
            TokenResponse::Other { ref error, .. } if error == "invalid_grant"
        ));
    }

    #[test]
    fn token_response_success_includes_refresh() {
        let json = json!({
            "access_token": make_jwt("t-2", "bob@x.com", 9999999999),
            "refresh_token": "REFRESH",
            "expires_in": 3599
        });
        match TokenResponse::parse(200, &json) {
            TokenResponse::Ok(t) => {
                assert_eq!(t.refresh_token.as_deref(), Some("REFRESH"));
                assert_eq!(t.expires_in, Some(3599));
                assert_eq!(t.upn.as_deref(), Some("bob@x.com"));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
        assert!(matches!(
            TokenResponse::parse(200, &json!({})),
            TokenResponse::Other { ref error, .. } if error == "missing_access_token"
        ));
    }

    #[test]
    fn device_code_response_defaults() {
        let parsed = DeviceCodeResponse::parse(&json!({"device_code": "D"})).unwrap();
        assert_eq!(parsed.user_code, "?");
        assert_eq!(parsed.verification_uri, "https://microsoft.com/devicelogin");
        assert_eq!(parsed.interval, 5);
        assert_eq!(parsed.expires_in, 900);
        assert!(DeviceCodeResponse::parse(&json!({})).is_err());
    }
}
