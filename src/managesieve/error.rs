/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt;
use std::io;

#[derive(Debug, thiserror::Error)]
pub enum SieveError {
    #[error("transport error: {0}")]
    Io(#[from] io::Error),

    #[error("tls error: {0}")]
    Tls(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("response parse error: {0}")]
    Parse(String),

    #[error("server said NO: {0}")]
    No(NoError),

    #[error("server closed connection: {0}")]
    Bye(String),

    #[error("unsupported by server: {0}")]
    Unsupported(String),

    #[error("referral not supported: {0}")]
    Referral(String),
}

const TRANSIENT_MARKERS: &[&str] = &[
    "temp",
    "try again",
    "try later",
    "unavailable",
    "throttle",
    "rate limit",
    "too many",
];

#[derive(Debug, Clone)]
pub struct NoError {
    pub text: String,
    pub code: Option<String>,
}

impl NoError {
    pub fn new(text: impl Into<String>, code: Option<String>) -> Self {
        Self {
            text: text.into(),
            code,
        }
    }

    pub fn is_transient(&self) -> bool {
        if let Some(code) = &self.code {
            if code.eq_ignore_ascii_case("TRYLATER") {
                return true;
            }
            if hashify::set_ignore_case!(
                code.as_bytes(),
                "QUOTA",
                "QUOTA/MAXSCRIPTS",
                "AUTH-TOO-WEAK",
                "ENCRYPT-NEEDED",
                "TRANSITION-NEEDED",
                "NONEXISTENT",
                "ALREADYEXISTS",
                "ACTIVE",
                "SASL",
                "TAG",
                "WARNINGS",
            ) {
                return false;
            }
        }
        let lower = self.text.to_ascii_lowercase();
        TRANSIENT_MARKERS.iter().any(|m| lower.contains(m))
    }

    pub fn is_referral(&self) -> bool {
        self.code
            .as_deref()
            .is_some_and(|c| c.eq_ignore_ascii_case("REFERRAL"))
    }
}

impl fmt::Display for NoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.code {
            Some(code) => write!(f, "({code}) {}", self.text),
            None => f.write_str(&self.text),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_markers_detected_case_insensitively() {
        for s in [
            "Temporary failure",
            "Try again later",
            "Server unavailable",
            "user is rate limited",
            "Too many connections",
        ] {
            assert!(
                NoError::new(s, None).is_transient(),
                "should be transient: {s}"
            );
        }
    }

    #[test]
    fn trylater_code_is_transient() {
        assert!(NoError::new("x", Some("TRYLATER".into())).is_transient());
    }

    #[test]
    fn quota_code_is_permanent_per_spec() {
        assert!(!NoError::new("x", Some("QUOTA".into())).is_transient());
        assert!(!NoError::new("x", Some("QUOTA/MAXSCRIPTS".into())).is_transient());
    }

    #[test]
    fn permanent_codes_override_text_match() {
        assert!(!NoError::new("try again later", Some("QUOTA".into())).is_transient());
        assert!(!NoError::new("temporarily", Some("AUTH-TOO-WEAK".into())).is_transient());
        assert!(!NoError::new("rate limit", Some("TRANSITION-NEEDED".into())).is_transient());
    }

    #[test]
    fn referral_code_detected() {
        assert!(NoError::new("x", Some("REFERRAL".into())).is_referral());
        assert!(!NoError::new("x", None).is_referral());
    }

    #[test]
    fn non_transient_no_not_flagged() {
        assert!(!NoError::new("Permission denied", None).is_transient());
    }
}
