/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::db::init::OpenError;
use crate::error::Error;
use crate::jmap::blob::BlobWalkError;

#[derive(Debug, thiserror::Error)]
pub enum JmapError {
    #[error("transport failure: {0}")]
    Transport(String),

    #[error("connection failure: {0}")]
    Connect(String),

    #[error("authentication rejected: {0}")]
    Auth(String),

    #[error("http status {status}: {body}")]
    HttpStatus { status: u16, body: String },

    #[error("retries exhausted: {0}")]
    RetriesExhausted(String),

    #[error("request too large")]
    RequestTooLarge,

    #[error("single object exceeds the server size limit and cannot be split: {0}")]
    SingleObjectTooLarge(String),

    #[error("query anchor not found")]
    AnchorNotFound,

    #[error("server cannot calculate changes from the stored state")]
    CannotCalculateChanges,

    #[error("server does not implement the requested method")]
    UnknownMethod,

    #[error("jmap method error in call {call_id}: {error_type}{}", .description.as_deref().map(|d| format!(" ({d})")).unwrap_or_default())]
    Method {
        call_id: String,
        error_type: String,
        description: Option<String>,
    },

    #[error("malformed jmap response: {0}")]
    Malformed(String),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("blob walk error: {0}")]
    Blob(#[from] BlobWalkError),
}

impl JmapError {
    pub fn malformed(context: impl Into<String>) -> JmapError {
        JmapError::Malformed(context.into())
    }
}

impl From<JmapError> for Error {
    fn from(value: JmapError) -> Self {
        match value {
            JmapError::Connect(m) | JmapError::Transport(m) => Error::Connection(m),
            JmapError::Auth(m) => Error::Connection(format!("authentication rejected: {m}")),
            JmapError::Sqlite(e) => Error::Db(OpenError::Sqlite(e)),
            reached @ (JmapError::HttpStatus { .. } | JmapError::RetriesExhausted(_)) => {
                Error::Connection(reached.to_string())
            }
            per_unit => Error::Partial(per_unit.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreachable_endpoint_aborts_the_run() {
        for e in [
            JmapError::Connect("refused".to_owned()),
            JmapError::Transport("io: broken pipe".to_owned()),
            JmapError::Auth("bad password".to_owned()),
            JmapError::HttpStatus {
                status: 404,
                body: "<html>404 Not Found</html>".to_owned(),
            },
            JmapError::RetriesExhausted("gave up".to_owned()),
        ] {
            let mapped = Error::from(e);
            assert!(mapped.aborts_run(), "{mapped} must abort the run");
            assert_eq!(mapped.exit_code(), 2);
        }
    }

    #[test]
    fn protocol_level_errors_stay_per_unit() {
        for e in [
            JmapError::Method {
                call_id: "q".to_owned(),
                error_type: "unknownMethod".to_owned(),
                description: None,
            },
            JmapError::UnknownMethod,
            JmapError::CannotCalculateChanges,
            JmapError::AnchorNotFound,
            JmapError::RequestTooLarge,
            JmapError::SingleObjectTooLarge("one blob".to_owned()),
            JmapError::Malformed("not a list".to_owned()),
        ] {
            let mapped = Error::from(e);
            assert!(!mapped.aborts_run(), "{mapped} must not abort the run");
            assert_eq!(mapped.exit_code(), 5);
        }
    }

    #[test]
    fn archive_failures_are_local_io() {
        let mapped = Error::from(JmapError::Sqlite(rusqlite::Error::QueryReturnedNoRows));
        assert!(mapped.aborts_run());
        assert_eq!(mapped.exit_code(), 7);
    }
}
