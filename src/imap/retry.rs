/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use super::error::ImapError;
use crate::net::backoff::{Fault, Retryable};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    TransportDrop,
    Transient,
    Permanent,
    FeatureNegotiation,
    AuthFailed,
}

impl ImapError {
    pub fn disposition(&self) -> Disposition {
        match self {
            ImapError::Io(_) | ImapError::Disconnected | ImapError::Bye(_) => {
                Disposition::TransportDrop
            }
            ImapError::AuthFailed(_) => Disposition::AuthFailed,
            ImapError::Bad(_) => Disposition::FeatureNegotiation,
            ImapError::No(no) if no.is_auth_failed() => Disposition::AuthFailed,
            ImapError::No(no) if no.is_transient() => Disposition::Transient,
            ImapError::No(_)
            | ImapError::Tls(_)
            | ImapError::Protocol(_)
            | ImapError::Parse(_)
            | ImapError::Unsupported(_) => Disposition::Permanent,
        }
    }

    pub fn is_negotiation_failure(&self) -> bool {
        match self {
            ImapError::Bad(_) | ImapError::Unsupported(_) => true,
            ImapError::No(no) => !no.is_transient(),
            _ => false,
        }
    }
}

impl Retryable for ImapError {
    fn fault(&self) -> Fault {
        match self.disposition() {
            Disposition::TransportDrop => Fault::TransportDrop,
            Disposition::Transient => Fault::Transient,
            Disposition::Permanent | Disposition::FeatureNegotiation | Disposition::AuthFailed => {
                Fault::Fatal
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::error::NoError;
    use super::*;
    use std::io::{self, ErrorKind};

    fn no(text: &str, code: Option<&str>) -> ImapError {
        ImapError::No(NoError::new(text, code.map(str::to_owned)))
    }

    #[test]
    fn transport_errors_are_transport_drop() {
        for k in [
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::BrokenPipe,
            ErrorKind::UnexpectedEof,
            ErrorKind::TimedOut,
        ] {
            let e = ImapError::Io(io::Error::new(k, "x"));
            assert_eq!(e.disposition(), Disposition::TransportDrop, "{k:?}");
        }
        assert_eq!(
            ImapError::Bye("going away".into()).disposition(),
            Disposition::TransportDrop
        );
        assert_eq!(ImapError::Disconnected.fault(), Fault::TransportDrop);
    }

    #[test]
    fn auth_failed_classified_separately() {
        assert_eq!(
            ImapError::AuthFailed("bad creds".into()).disposition(),
            Disposition::AuthFailed
        );
        assert_eq!(
            no("x", Some("AUTHENTICATIONFAILED")).disposition(),
            Disposition::AuthFailed
        );
    }

    #[test]
    fn no_responses_split_by_text() {
        assert_eq!(
            no("Try again later", None).disposition(),
            Disposition::Transient
        );
        assert_eq!(no("user over quota", None).fault(), Fault::Transient);
        assert_eq!(
            no("mailbox does not exist", None).disposition(),
            Disposition::Permanent
        );
    }

    #[test]
    fn bad_protocol_and_parse_are_fatal() {
        assert_eq!(
            ImapError::Bad("unrecognised command".into()).disposition(),
            Disposition::FeatureNegotiation
        );
        for e in [
            ImapError::Protocol("x".into()),
            ImapError::Parse("x".into()),
        ] {
            assert_eq!(e.disposition(), Disposition::Permanent, "{e:?}");
            assert_eq!(e.fault(), Fault::Fatal, "{e:?}");
        }
    }

    #[test]
    fn negotiation_failure_matches_bad_unsupported_and_permanent_no() {
        assert!(ImapError::Bad("x".into()).is_negotiation_failure());
        assert!(ImapError::Unsupported("x".into()).is_negotiation_failure());
        assert!(no("no such mailbox", None).is_negotiation_failure());
        assert!(!no("try again later", None).is_negotiation_failure());
        assert!(!ImapError::Bye("x".into()).is_negotiation_failure());
        assert!(!ImapError::Disconnected.is_negotiation_failure());
    }
}
