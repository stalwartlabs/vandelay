/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::Row;

use super::JsonColumn;
use crate::archive::rows::{IdentityRecord, Write};
use crate::archive::{LocalId, Store};
use crate::jmap::error::JmapError;
use crate::jmap::wire::identity::Identity;

impl Identity {
    pub(crate) const SELECT: &str =
        "SELECT id, name, email, reply_to, bcc, text_signature, html_signature FROM identities";

    fn with_record<R>(
        &self,
        write: impl FnOnce(&IdentityRecord<'_>) -> rusqlite::Result<R>,
    ) -> Result<R, JmapError> {
        let reply_to = JsonColumn::encode(self.reply_to.as_ref())?;
        let bcc = JsonColumn::encode(self.bcc.as_ref())?;
        Ok(write(&IdentityRecord {
            name: &self.name,
            email: &self.email,
            reply_to: reply_to.as_deref(),
            bcc: bcc.as_deref(),
            text_signature: &self.text_signature,
            html_signature: &self.html_signature,
        })?)
    }

    pub(crate) fn insert(&self, store: Store<'_>) -> Result<LocalId, JmapError> {
        self.with_record(|r| store.identities().insert(r))
    }

    pub(crate) fn update(&self, store: Store<'_>, id: LocalId) -> Result<Write, JmapError> {
        self.with_record(|r| store.identities().update(id, r))
    }

    pub(crate) fn from_row(row: &Row<'_>) -> Result<Identity, JmapError> {
        Ok(Identity {
            id: None,
            name: row.get(1)?,
            email: row.get(2)?,
            reply_to: JsonColumn::decode(row.get(3)?)?,
            bcc: JsonColumn::decode(row.get(4)?)?,
            text_signature: row.get(5)?,
            html_signature: row.get(6)?,
        })
    }
}
