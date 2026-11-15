/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::time::Duration;

use crate::exchange::oauth::RefreshSchedule;

pub const SCOPES: &str = "offline_access User.Read Mail.Read MailboxSettings.Read Calendars.Read Contacts.Read Files.Read";
pub const REFRESH_SCHEDULE: RefreshSchedule = RefreshSchedule {
    lead: Duration::from_secs(60),
    fallback_lifetime: Duration::from_secs(50 * 60),
};
pub const REFRESH_ATTEMPTS: u32 = 4;

#[cfg(test)]
mod tests {
    use super::*;

    // Device code consent covers every --objects surface, including the drive.
    #[test]
    fn scopes_cover_every_surface() {
        for required in [
            "offline_access",
            "Mail.Read",
            "MailboxSettings.Read",
            "Calendars.Read",
            "Contacts.Read",
            "Files.Read",
        ] {
            assert!(
                SCOPES.split_whitespace().any(|s| s == required),
                "missing {required}"
            );
        }
    }
}
