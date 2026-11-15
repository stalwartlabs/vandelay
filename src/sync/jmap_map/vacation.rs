/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::archive::rows::{StoredVacation, VacationRecord};
use crate::jmap::wire::vacation_response::VacationResponse;

impl VacationResponse {
    pub(crate) fn record(&self) -> VacationRecord<'_> {
        VacationRecord {
            is_enabled: self.is_enabled,
            from_date: self.from_date.as_deref(),
            to_date: self.to_date.as_deref(),
            subject: self.subject.as_deref(),
            text_body: self.text_body.as_deref(),
            html_body: self.html_body.as_deref(),
        }
    }
}

impl From<StoredVacation> for VacationResponse {
    fn from(stored: StoredVacation) -> VacationResponse {
        VacationResponse {
            is_enabled: stored.is_enabled,
            from_date: stored.from_date,
            to_date: stored.to_date,
            subject: stored.subject,
            text_body: stored.text_body,
            html_body: stored.html_body,
        }
    }
}
