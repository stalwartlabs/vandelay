/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::time::Duration;

use crate::exchange::ews::types::ResponseCode;

#[derive(Debug, Clone)]
pub enum FaultDisposition {
    Fatal,
    Auth,
    VersionError,
    Retryable { delay: Option<Duration> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemAction {
    Success,
    Vanished,
    Skip,
    Retryable,
    RetryDiscardSyncState,
}

impl ResponseCode {
    pub fn fault_disposition(&self) -> FaultDisposition {
        match self {
            ResponseCode::ServerBusy { back_off_ms } => FaultDisposition::Retryable {
                delay: back_off_ms.map(Duration::from_millis),
            },
            ResponseCode::TimeoutExpired
            | ResponseCode::InternalServerTransientError
            | ResponseCode::MailboxStoreUnavailable
            | ResponseCode::ConnectionFailed
            | ResponseCode::AdUnavailable
            | ResponseCode::BatchProcessingStopped => FaultDisposition::Retryable { delay: None },
            ResponseCode::AuthenticationRequired => FaultDisposition::Auth,
            ResponseCode::InvalidServerVersion
            | ResponseCode::IncorrectSchemaVersion
            | ResponseCode::InvalidRequest
            | ResponseCode::InvalidSchemaVersionForMailboxVersion => FaultDisposition::VersionError,
            _ => FaultDisposition::Fatal,
        }
    }

    pub fn item_action(&self) -> ItemAction {
        match self {
            ResponseCode::NoError => ItemAction::Success,
            ResponseCode::ItemNotFound | ResponseCode::AttachmentNotFound => ItemAction::Vanished,
            ResponseCode::ServerBusy { .. }
            | ResponseCode::TimeoutExpired
            | ResponseCode::InternalServerTransientError
            | ResponseCode::MailboxStoreUnavailable
            | ResponseCode::ConnectionFailed
            | ResponseCode::AdUnavailable
            | ResponseCode::BatchProcessingStopped => ItemAction::Retryable,
            ResponseCode::InvalidSyncStateData => ItemAction::RetryDiscardSyncState,
            ResponseCode::AccessDenied
            | ResponseCode::InvalidIdMalformed
            | ResponseCode::MimeContentConversionFailed
            | ResponseCode::ImpersonationFailed
            | ResponseCode::AuthenticationRequired
            | ResponseCode::InvalidServerVersion
            | ResponseCode::IncorrectSchemaVersion
            | ResponseCode::InvalidRequest
            | ResponseCode::InvalidSchemaVersionForMailboxVersion
            | ResponseCode::Other(_) => ItemAction::Skip,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::retry::Disposition;

    #[test]
    fn server_busy_with_backoff_is_retryable_with_delay() {
        let code = ResponseCode::ServerBusy {
            back_off_ms: Some(297749),
        };
        match code.fault_disposition() {
            FaultDisposition::Retryable { delay: Some(d) } => {
                assert_eq!(d, Duration::from_millis(297749));
            }
            _ => panic!("expected retryable with delay"),
        }
    }

    #[test]
    fn item_responses_are_classified() {
        assert_eq!(ResponseCode::NoError.item_action(), ItemAction::Success);
        assert_eq!(
            ResponseCode::ItemNotFound.item_action(),
            ItemAction::Vanished
        );
        assert_eq!(
            ResponseCode::AttachmentNotFound.item_action(),
            ItemAction::Vanished
        );
        assert_eq!(ResponseCode::AccessDenied.item_action(), ItemAction::Skip);
        assert_eq!(
            ResponseCode::InvalidSyncStateData.item_action(),
            ItemAction::RetryDiscardSyncState
        );
        assert_eq!(
            ResponseCode::ServerBusy { back_off_ms: None }.item_action(),
            ItemAction::Retryable
        );
    }

    #[test]
    fn http_status_table() {
        assert_eq!(Disposition::of_status(429), Disposition::Retry);
        assert_eq!(Disposition::of_status(503), Disposition::Retry);
        assert_eq!(Disposition::of_status(500), Disposition::Fatal);
        assert_eq!(Disposition::of_status(401), Disposition::Fatal);
    }

    #[test]
    fn schema_version_codes_request_a_downgrade() {
        for code in [
            ResponseCode::InvalidServerVersion,
            ResponseCode::IncorrectSchemaVersion,
            ResponseCode::InvalidRequest,
            ResponseCode::InvalidSchemaVersionForMailboxVersion,
        ] {
            assert!(matches!(
                code.fault_disposition(),
                FaultDisposition::VersionError
            ));
        }
    }

    #[test]
    fn transient_codes_are_retryable_at_item_and_fault_level() {
        for code in [
            ResponseCode::InternalServerTransientError,
            ResponseCode::MailboxStoreUnavailable,
            ResponseCode::ConnectionFailed,
            ResponseCode::AdUnavailable,
            ResponseCode::BatchProcessingStopped,
        ] {
            assert_eq!(code.item_action(), ItemAction::Retryable);
            assert!(matches!(
                code.fault_disposition(),
                FaultDisposition::Retryable { .. }
            ));
        }
    }

    #[test]
    fn mime_content_conversion_failure_skips_item() {
        assert_eq!(
            ResponseCode::MimeContentConversionFailed.item_action(),
            ItemAction::Skip
        );
    }

    #[test]
    fn other_unknown_codes_skip_at_item_level() {
        assert_eq!(
            ResponseCode::Other("ErrorWhoKnows".to_owned()).item_action(),
            ItemAction::Skip
        );
    }
}
