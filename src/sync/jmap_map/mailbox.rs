/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::Row;

use super::IdRefs;
use crate::archive::rows::{MailboxRecord, Subscribed, Write};
use crate::archive::{LocalId, Store};
use crate::jmap::error::JmapError;
use crate::jmap::wire::JmapId;
use crate::jmap::wire::mailbox::Mailbox;
use crate::types::ObjectType;

impl Mailbox {
    pub(crate) const SELECT: &str =
        "SELECT id, name, parent_id, role, sort_order, is_subscribed FROM mailboxes";

    fn record(&self, resolve: &impl Fn(ObjectType, &str) -> Option<LocalId>) -> MailboxRecord<'_> {
        MailboxRecord {
            name: &self.name,
            parent: IdRefs::parent(self.parent_id.as_ref(), ObjectType::Mailbox, resolve),
            role: self.role.as_deref(),
            sort_order: i64::from(self.sort_order),
            subscribed: Subscribed::from(self.is_subscribed),
        }
    }

    pub(crate) fn insert(
        &self,
        store: Store<'_>,
        resolve: &impl Fn(ObjectType, &str) -> Option<LocalId>,
    ) -> Result<LocalId, JmapError> {
        Ok(store.mailboxes().insert(&self.record(resolve))?)
    }

    pub(crate) fn update(
        &self,
        store: Store<'_>,
        id: LocalId,
        resolve: &impl Fn(ObjectType, &str) -> Option<LocalId>,
    ) -> Result<Write, JmapError> {
        Ok(store.mailboxes().update(id, &self.record(resolve))?)
    }

    pub(crate) fn from_row(
        row: &Row<'_>,
        resolve: &impl Fn(ObjectType, LocalId) -> Option<JmapId>,
    ) -> Result<Mailbox, JmapError> {
        let parent_id = row
            .get::<_, Option<LocalId>>(2)?
            .map(|p| {
                resolve(ObjectType::Mailbox, p)
                    .ok_or_else(|| JmapError::malformed("unresolved mailbox parent on export"))
            })
            .transpose()?;
        Ok(Mailbox {
            id: None,
            name: row.get(1)?,
            parent_id,
            role: row.get(3)?,
            sort_order: row.get::<_, i64>(4)? as u32,
            is_subscribed: row.get::<_, i64>(5)? != 0,
        })
    }
}
