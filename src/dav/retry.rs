/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use serde_json::Value;

const BAD_REQUEST: u16 = 400;
const UNAUTHORIZED: u16 = 401;
const FORBIDDEN: u16 = 403;
const NOT_FOUND: u16 = 404;
const GONE: u16 = 410;
const LOCKED: u16 = 423;
const TOO_MANY_REQUESTS: u16 = 429;
const BAD_GATEWAY: u16 = 502;
const SERVICE_UNAVAILABLE: u16 = 503;
const GATEWAY_TIMEOUT: u16 = 504;
const INSUFFICIENT_STORAGE: u16 = 507;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DavOutcome {
    Success,
    Retryable,
    Vanished,
    Auth,
    Fatal,
}

impl DavOutcome {
    pub fn classify(status: u16, body: &[u8]) -> DavOutcome {
        match status {
            200..=299 => DavOutcome::Success,
            UNAUTHORIZED => DavOutcome::Auth,
            FORBIDDEN if DavOutcome::is_google_usage_limit(body) => DavOutcome::Retryable,
            FORBIDDEN => DavOutcome::Auth,
            NOT_FOUND | GONE => DavOutcome::Vanished,
            LOCKED | TOO_MANY_REQUESTS | BAD_GATEWAY | SERVICE_UNAVAILABLE | GATEWAY_TIMEOUT
            | INSUFFICIENT_STORAGE => DavOutcome::Retryable,
            _ => DavOutcome::Fatal,
        }
    }

    pub fn is_gone(status: u16) -> bool {
        matches!(status, NOT_FOUND | GONE)
    }

    pub fn is_error(status: u16) -> bool {
        status >= BAD_REQUEST
    }

    fn is_google_usage_limit(body: &[u8]) -> bool {
        let Ok(value) = serde_json::from_slice::<Value>(body) else {
            return false;
        };
        value
            .get("error")
            .and_then(|e| e.get("errors"))
            .and_then(Value::as_array)
            .is_some_and(|errors| {
                errors
                    .iter()
                    .any(|entry| entry.get("domain").and_then(Value::as_str) == Some("usageLimits"))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_range() {
        assert_eq!(DavOutcome::classify(200, b""), DavOutcome::Success);
        assert_eq!(DavOutcome::classify(207, b""), DavOutcome::Success);
        assert_eq!(DavOutcome::classify(299, b""), DavOutcome::Success);
    }

    #[test]
    fn auth_status() {
        assert_eq!(DavOutcome::classify(401, b""), DavOutcome::Auth);
        assert_eq!(DavOutcome::classify(403, b""), DavOutcome::Auth);
    }

    #[test]
    fn vanished_status() {
        assert_eq!(DavOutcome::classify(404, b""), DavOutcome::Vanished);
        assert_eq!(DavOutcome::classify(410, b""), DavOutcome::Vanished);
    }

    #[test]
    fn locked_and_quota_are_retryable() {
        assert_eq!(DavOutcome::classify(423, b""), DavOutcome::Retryable);
        assert_eq!(DavOutcome::classify(429, b""), DavOutcome::Retryable);
        assert_eq!(DavOutcome::classify(503, b""), DavOutcome::Retryable);
        assert_eq!(DavOutcome::classify(507, b""), DavOutcome::Retryable);
    }

    #[test]
    fn google_usage_limit_403_is_retryable() {
        let body =
            br#"{"error":{"errors":[{"domain":"usageLimits","reason":"rateLimitExceeded"}]}}"#;
        assert_eq!(DavOutcome::classify(403, body), DavOutcome::Retryable);
    }

    #[test]
    fn non_google_403_is_auth() {
        let body = br#"{"error":{"errors":[{"domain":"global","reason":"forbidden"}]}}"#;
        assert_eq!(DavOutcome::classify(403, body), DavOutcome::Auth);
    }

    #[test]
    fn fatal_4xx_other() {
        assert_eq!(DavOutcome::classify(400, b""), DavOutcome::Fatal);
        assert_eq!(DavOutcome::classify(405, b""), DavOutcome::Fatal);
        assert_eq!(DavOutcome::classify(412, b""), DavOutcome::Fatal);
    }
}
