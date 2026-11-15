/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt;
use std::io;

use crate::net::stream::StreamError;

#[derive(Debug, thiserror::Error)]
pub enum ImapError {
    #[error("transport error: {0}")]
    Io(#[from] io::Error),

    #[error("tls error: {0}")]
    Tls(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("response parse error: {0}")]
    Parse(String),

    #[error("authentication failed: {0}")]
    AuthFailed(String),

    #[error("server said BAD: {0}")]
    Bad(String),

    #[error("server said NO: {0}")]
    No(NoError),

    #[error("server closed connection: {0}")]
    Bye(String),

    #[error("connection is no longer usable")]
    Disconnected,

    #[error("unsupported by server: {0}")]
    Unsupported(String),
}

impl ImapError {
    pub fn into_auth_failure(self) -> ImapError {
        match self {
            ImapError::No(no) if no.is_auth_failed() => ImapError::AuthFailed(no.text),
            other => other,
        }
    }

    pub fn is_auth_failed(&self) -> bool {
        match self {
            ImapError::AuthFailed(_) => true,
            ImapError::No(no) => no.is_auth_failed(),
            _ => false,
        }
    }
}

impl From<StreamError> for ImapError {
    fn from(e: StreamError) -> ImapError {
        match e {
            StreamError::Io(e) => ImapError::Io(e),
            StreamError::Tls(m) => ImapError::Tls(m),
            StreamError::CompressedStartTls => {
                ImapError::Protocol("cannot STARTTLS after COMPRESS DEFLATE".into())
            }
        }
    }
}

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
        let lower = self.text.to_ascii_lowercase();
        const TRANSIENT_MARKERS: &[&str] = &[
            "temp",
            "try again",
            "try later",
            "unavailable",
            "over quota",
            "overquota",
            "throttle",
            "rate limit",
            "too many",
        ];
        TRANSIENT_MARKERS.iter().any(|m| lower.contains(m))
    }

    pub fn is_auth_failed(&self) -> bool {
        self.code.as_deref().is_some_and(|code| {
            hashify::set!(
                code.as_bytes(),
                "AUTHENTICATIONFAILED",
                "PRIVACYREQUIRED",
                "AUTHORIZATIONFAILED",
            )
        })
    }
}

impl fmt::Display for NoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.code {
            Some(code) => write!(f, "[{code}] {}", self.text),
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
            "OVER QUOTA",
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
    fn non_transient_markers_not_flagged() {
        for s in [
            "Mailbox does not exist",
            "Permission denied",
            "Invalid argument",
        ] {
            assert!(
                !NoError::new(s, None).is_transient(),
                "should NOT be transient: {s}"
            );
        }
    }

    #[test]
    fn auth_failed_codes_detected() {
        assert!(NoError::new("x", Some("AUTHENTICATIONFAILED".into())).is_auth_failed());
        assert!(NoError::new("x", Some("PRIVACYREQUIRED".into())).is_auth_failed());
        assert!(!NoError::new("x", Some("ALERT".into())).is_auth_failed());
        assert!(!NoError::new("x", None).is_auth_failed());
    }
}
