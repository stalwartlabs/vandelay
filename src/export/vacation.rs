/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use serde_json::json;

use super::Exporter;
use crate::error::Error;
use crate::jmap::request::SetRequest;
use crate::jmap::wire::vacation_response::{SINGLETON_ID, TYPE_NAME, VacationResponse};
use crate::sync::TypeCounts;

impl Exporter<'_> {
    pub(super) fn reconcile_vacation(&self, counts: &mut TypeCounts) -> Result<bool, Error> {
        let Some(stored) = self.store.vacation().load().map_err(Error::partial)? else {
            return Ok(false);
        };
        if self.is_dry_run() {
            counts.updated += 1;
            return Ok(true);
        }
        let vacation = VacationResponse::from(stored);
        let outcome = self.account.set(
            TYPE_NAME,
            SetRequest {
                update: Some(json!({ SINGLETON_ID: vacation })),
                ..Default::default()
            },
        )?;
        counts.updated += outcome.updated.len() as u64;
        for (id, err) in &outcome.not_updated {
            self.logger
                .warn(format_args!("vacation response {id} not updated: {err}"));
            counts.failed += 1;
        }
        Ok(true)
    }
}
