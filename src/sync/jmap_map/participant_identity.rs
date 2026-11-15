/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::archive::rows::{ParticipantIdentityRecord, Write};
use crate::archive::{LocalId, Store};
use crate::jmap::error::JmapError;
use crate::jmap::wire::participant_identity::ParticipantIdentity;

impl ParticipantIdentity {
    fn record(&self) -> ParticipantIdentityRecord<'_> {
        ParticipantIdentityRecord {
            name: &self.name,
            calendar_address: &self.calendar_address,
            is_default: self.is_default,
        }
    }

    pub(crate) fn insert(&self, store: Store<'_>) -> Result<LocalId, JmapError> {
        Ok(store.participant_identities().insert(&self.record())?)
    }

    pub(crate) fn update(&self, store: Store<'_>, id: LocalId) -> Result<Write, JmapError> {
        Ok(store.participant_identities().update(id, &self.record())?)
    }
}
