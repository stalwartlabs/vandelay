/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::net::backoff::{Fault, Retryable};

use super::error::SieveError;

impl SieveError {
    pub fn is_referral(&self) -> bool {
        match self {
            SieveError::No(no) => no.is_referral(),
            SieveError::Referral(_) => true,
            _ => false,
        }
    }

    pub fn is_negotiation_failure(&self) -> bool {
        match self {
            SieveError::No(no) => !no.is_transient() && !no.is_referral(),
            SieveError::Unsupported(_) => true,
            _ => false,
        }
    }
}

impl Retryable for SieveError {
    fn fault(&self) -> Fault {
        match self {
            SieveError::Io(_) | SieveError::Bye(_) => Fault::TransportDrop,
            SieveError::No(no) if !no.is_referral() && no.is_transient() => Fault::Transient,
            _ => Fault::Fatal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::error::NoError;
    use super::*;
    use std::io::{self, ErrorKind};

    #[test]
    fn io_errors_are_transport_drop() {
        for k in [
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::BrokenPipe,
            ErrorKind::UnexpectedEof,
            ErrorKind::TimedOut,
        ] {
            let e = SieveError::Io(io::Error::new(k, "x"));
            assert_eq!(e.fault(), Fault::TransportDrop, "{k:?}");
        }
    }

    #[test]
    fn bye_is_transport_drop() {
        assert_eq!(
            SieveError::Bye("going away".into()).fault(),
            Fault::TransportDrop
        );
    }

    #[test]
    fn transient_no_text_marked_transient() {
        let e = SieveError::No(NoError::new("Try again later", None));
        assert_eq!(e.fault(), Fault::Transient);
    }

    #[test]
    fn trylater_code_marked_transient() {
        let e = SieveError::No(NoError::new("x", Some("TRYLATER".into())));
        assert_eq!(e.fault(), Fault::Transient);
    }

    #[test]
    fn referral_is_fatal_and_flagged() {
        let e = SieveError::No(NoError::new("temp", Some("REFERRAL".into())));
        assert_eq!(e.fault(), Fault::Fatal);
        assert!(e.is_referral());
        assert!(SieveError::Referral("x".into()).is_referral());
    }

    #[test]
    fn nonexistent_no_and_tls_are_fatal() {
        let e = SieveError::No(NoError::new("no such script", Some("NONEXISTENT".into())));
        assert_eq!(e.fault(), Fault::Fatal);
        assert_eq!(SieveError::Tls("x".into()).fault(), Fault::Fatal);
    }

    #[test]
    fn negotiation_failure_matches_non_transient_no_and_unsupported() {
        assert!(SieveError::Unsupported("x".into()).is_negotiation_failure());
        assert!(SieveError::No(NoError::new("no such mech", None)).is_negotiation_failure());
        assert!(!SieveError::No(NoError::new("try again", None)).is_negotiation_failure());
        assert!(
            !SieveError::No(NoError::new("x", Some("REFERRAL".into()))).is_negotiation_failure()
        );
    }
}
