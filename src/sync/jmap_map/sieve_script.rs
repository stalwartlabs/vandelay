/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::Row;

use crate::archive::rows::{SieveScriptRecord, Write};
use crate::archive::{BlobId, LocalId, Store};
use crate::jmap::error::JmapError;
use crate::jmap::wire::sieve_script::SieveScript;

#[derive(Debug, Clone)]
pub(crate) struct SieveRow {
    pub(crate) name: Option<String>,
    pub(crate) is_active: bool,
    pub(crate) blob: BlobId,
}

impl SieveRow {
    pub(crate) const SELECT: &str = "SELECT id, name, is_active, blob_id FROM sieve_scripts";

    pub(crate) fn from_row(row: &Row<'_>) -> Result<SieveRow, JmapError> {
        Ok(SieveRow {
            name: row.get(1)?,
            is_active: row.get::<_, i64>(2)? != 0,
            blob: row.get(3)?,
        })
    }
}

impl SieveScript {
    fn record(&self, blob: BlobId) -> SieveScriptRecord<'_> {
        SieveScriptRecord {
            name: self.name.as_deref(),
            is_active: self.is_active,
            blob,
        }
    }

    pub(crate) fn insert(&self, store: Store<'_>, blob: BlobId) -> Result<LocalId, JmapError> {
        Ok(store.sieve_scripts().insert(&self.record(blob))?)
    }

    pub(crate) fn update(
        &self,
        store: Store<'_>,
        id: LocalId,
        blob: BlobId,
    ) -> Result<Write, JmapError> {
        Ok(store.sieve_scripts().update(id, &self.record(blob))?)
    }
}
