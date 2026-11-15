/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::error::Error;
use crate::exchange::ews::types::ResponseCode;
use crate::exchange::oauth::OAuthError;
use crate::jmap::error::JmapError;
use crate::net::http::TransportError;

#[derive(Debug, thiserror::Error)]
pub enum EwsError {
    #[error("transport: {0}")]
    Transport(String),

    #[error("connection: {0}")]
    Connect(String),

    #[error("authentication rejected: {0}")]
    Auth(String),

    #[error("http {status}: {body}")]
    HttpStatus { status: u16, body: String },

    #[error("retries exhausted: {0}")]
    RetriesExhausted(String),

    #[error("soap fault {code}: {reason}")]
    SoapFault { code: ResponseCode, reason: String },

    #[error("malformed EWS response: {0}")]
    Malformed(String),

    #[error("EWS schema unsupported: {0}")]
    Unsupported(String),

    #[error("autodiscover failed: {0}")]
    AutodiscoverFailed(String),

    #[error("autodiscover redirect loop")]
    AutodiscoverLoop,

    #[error("oauth: {0}")]
    OAuth(String),

    #[error("xml: {0}")]
    Xml(String),
}

impl EwsError {
    pub fn is_per_batch(&self) -> bool {
        matches!(
            self,
            EwsError::SoapFault { .. } | EwsError::HttpStatus { .. } | EwsError::Malformed(_)
        )
    }
}

impl From<OAuthError> for EwsError {
    fn from(value: OAuthError) -> Self {
        EwsError::OAuth(value.0)
    }
}

impl From<quick_xml::Error> for EwsError {
    fn from(value: quick_xml::Error) -> Self {
        EwsError::Xml(value.to_string())
    }
}

impl From<TransportError> for EwsError {
    fn from(value: TransportError) -> Self {
        if value.is_connect() {
            EwsError::Connect(value.to_string())
        } else {
            EwsError::Transport(value.to_string())
        }
    }
}

impl From<JmapError> for EwsError {
    fn from(value: JmapError) -> Self {
        match value {
            JmapError::Transport(m) => EwsError::Transport(m),
            JmapError::Connect(m) => EwsError::Connect(m),
            JmapError::Auth(m) => EwsError::Auth(m),
            JmapError::HttpStatus { status, body } => EwsError::HttpStatus { status, body },
            JmapError::RetriesExhausted(m) => EwsError::RetriesExhausted(m),
            JmapError::Malformed(m) => EwsError::Malformed(m),
            other => EwsError::Connect(other.to_string()),
        }
    }
}

impl From<EwsError> for Error {
    fn from(value: EwsError) -> Self {
        match value {
            EwsError::Auth(m) => Error::Connection(format!("authentication rejected: {m}")),
            EwsError::Connect(m) | EwsError::Transport(m) => Error::Connection(m),
            EwsError::AutodiscoverFailed(m) => {
                Error::Connection(format!("autodiscover failed: {m}"))
            }
            EwsError::AutodiscoverLoop => {
                Error::Connection("autodiscover redirect loop".to_owned())
            }
            EwsError::OAuth(m) => Error::Connection(format!("oauth: {m}")),
            EwsError::Unsupported(m) => Error::Usage(m),
            other => Error::Connection(other.to_string()),
        }
    }
}
