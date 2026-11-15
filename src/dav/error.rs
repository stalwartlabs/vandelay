/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::dav::parse::ParseError;
use crate::error::Error;
use crate::net::http::TransportError;

#[derive(Debug, thiserror::Error)]
pub enum DavError {
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
    #[error("malformed jmap response: multistatus parse: {0}")]
    Multistatus(#[from] ParseError),
}

impl DavError {
    pub fn is_connect(&self) -> bool {
        matches!(self, DavError::Connect(_))
    }
}

impl From<TransportError> for DavError {
    fn from(err: TransportError) -> DavError {
        if err.is_connect() {
            DavError::Connect(err.to_string())
        } else {
            DavError::Transport(err.to_string())
        }
    }
}

impl From<DavError> for Error {
    fn from(value: DavError) -> Error {
        match value {
            DavError::Connect(m) | DavError::Transport(m) => Error::Connection(m),
            reached @ (DavError::Auth(_)
            | DavError::HttpStatus { .. }
            | DavError::RetriesExhausted(_)) => Error::Connection(reached.to_string()),
            per_unit @ DavError::Multistatus(_) => Error::Partial(per_unit.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ExitStatus;
    use crate::jmap::error::JmapError;

    fn samples() -> [(DavError, JmapError); 6] {
        [
            (
                DavError::Connect("refused".to_owned()),
                JmapError::Connect("refused".to_owned()),
            ),
            (
                DavError::Transport("io: broken pipe".to_owned()),
                JmapError::Transport("io: broken pipe".to_owned()),
            ),
            (
                DavError::Auth("server returned 401: no".to_owned()),
                JmapError::Auth("server returned 401: no".to_owned()),
            ),
            (
                DavError::HttpStatus {
                    status: 404,
                    body: "<html/>".to_owned(),
                },
                JmapError::HttpStatus {
                    status: 404,
                    body: "<html/>".to_owned(),
                },
            ),
            (
                DavError::RetriesExhausted("gave up".to_owned()),
                JmapError::RetriesExhausted("gave up".to_owned()),
            ),
            (
                DavError::Multistatus(ParseError::MissingHref),
                JmapError::Malformed("multistatus parse: missing href in response".to_owned()),
            ),
        ]
    }

    #[test]
    fn texts_and_exit_codes_match_the_former_jmap_error() {
        for (dav, jmap) in samples() {
            assert_eq!(dav.to_string(), jmap.to_string());
            let (dav, jmap) = (Error::from(dav), Error::from(jmap));
            assert_eq!(dav.to_string(), jmap.to_string());
            assert_eq!(dav.exit_status(), jmap.exit_status());
            assert_eq!(dav.aborts_run(), jmap.aborts_run());
        }
    }

    #[test]
    fn a_malformed_multistatus_stays_per_unit() {
        let mapped = Error::from(DavError::Multistatus(ParseError::MissingHref));
        assert!(!mapped.aborts_run());
        assert_eq!(mapped.exit_status(), ExitStatus::Partial);
    }
}
