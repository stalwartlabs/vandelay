/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpClass {
    Success,
    Retryable,
    Auth,
    Vanished,
    Fatal,
}

const UNAUTHORIZED: u16 = 401;
const FORBIDDEN: u16 = 403;
const NOT_FOUND: u16 = 404;
const GONE: u16 = 410;
const TOO_MANY_REQUESTS: u16 = 429;
const INTERNAL_SERVER_ERROR: u16 = 500;
const BAD_GATEWAY: u16 = 502;
const SERVICE_UNAVAILABLE: u16 = 503;
const GATEWAY_TIMEOUT: u16 = 504;

impl HttpClass {
    pub fn of_status(status: u16) -> HttpClass {
        match status {
            200..=299 => HttpClass::Success,
            UNAUTHORIZED | FORBIDDEN => HttpClass::Auth,
            NOT_FOUND | GONE => HttpClass::Vanished,
            TOO_MANY_REQUESTS
            | INTERNAL_SERVER_ERROR
            | BAD_GATEWAY
            | SERVICE_UNAVAILABLE
            | GATEWAY_TIMEOUT => HttpClass::Retryable,
            _ => HttpClass::Fatal,
        }
    }

    pub fn arms_throttle(status: u16) -> bool {
        matches!(status, TOO_MANY_REQUESTS | SERVICE_UNAVAILABLE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_status_is_retryable() {
        assert_eq!(HttpClass::of_status(429), HttpClass::Retryable);
        assert!(HttpClass::arms_throttle(429));
    }

    #[test]
    fn service_unavailable_is_retryable_and_throttle_marked() {
        assert_eq!(HttpClass::of_status(503), HttpClass::Retryable);
        assert!(HttpClass::arms_throttle(503));
    }

    #[test]
    fn bad_gateway_and_timeout_retry() {
        assert_eq!(HttpClass::of_status(502), HttpClass::Retryable);
        assert_eq!(HttpClass::of_status(504), HttpClass::Retryable);
    }

    #[test]
    fn insufficient_storage_is_per_item_fatal() {
        assert_eq!(HttpClass::of_status(507), HttpClass::Fatal);
    }

    #[test]
    fn auth_failure_is_marked_distinctly() {
        assert_eq!(HttpClass::of_status(401), HttpClass::Auth);
        assert_eq!(HttpClass::of_status(403), HttpClass::Auth);
    }

    #[test]
    fn not_found_and_gone_are_vanished() {
        assert_eq!(HttpClass::of_status(404), HttpClass::Vanished);
        assert_eq!(HttpClass::of_status(410), HttpClass::Vanished);
    }

    #[test]
    fn unknown_4xx_is_fatal() {
        assert_eq!(HttpClass::of_status(400), HttpClass::Fatal);
        assert_eq!(HttpClass::of_status(405), HttpClass::Fatal);
        assert_eq!(HttpClass::of_status(408), HttpClass::Fatal);
        assert_eq!(HttpClass::of_status(422), HttpClass::Fatal);
        assert_eq!(HttpClass::of_status(423), HttpClass::Fatal);
    }
}
