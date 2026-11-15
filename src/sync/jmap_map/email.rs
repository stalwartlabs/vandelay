/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use indexmap::IndexMap;
use rusqlite::Row;
use serde::Deserialize;
use serde_json::Value;

use super::{IdRefs, KeywordNames, LocalIds, UtcText};
use crate::archive::rows::{EmailRecord, Write};
use crate::archive::{BlobId, LocalId, Store};
use crate::jmap::error::JmapError;
use crate::jmap::wire::JmapId;
use crate::jmap::wire::common::UtcDate;
use crate::jmap::wire::email::Email;
use crate::types::ObjectType;

#[derive(Debug, Clone)]
pub(crate) struct EmailRow {
    pub(crate) blob: BlobId,
    pub(crate) received_at: String,
    pub(crate) mailboxes: LocalIds,
    pub(crate) keywords: Vec<String>,
    pub(crate) message_match: String,
}

impl EmailRow {
    pub(crate) const SELECT: &str =
        "SELECT id, blob_id, received_at, mailbox_ids, keywords, message_match FROM emails";

    pub(crate) fn from_row(row: &Row<'_>) -> Result<EmailRow, JmapError> {
        let mailbox_text: String = row.get(3)?;
        let keyword_text: String = row.get(4)?;
        Ok(EmailRow {
            blob: row.get(1)?,
            received_at: row.get(2)?,
            mailboxes: LocalIds::parse(&mailbox_text)?,
            keywords: serde_json::from_str(&keyword_text)?,
            message_match: row.get(5)?,
        })
    }
}

impl Email {
    pub(crate) fn insert(
        &self,
        store: Store<'_>,
        blob: BlobId,
        message_match: &str,
        resolve: &impl Fn(ObjectType, &str) -> Option<LocalId>,
    ) -> Result<LocalId, JmapError> {
        let mailboxes = IdRefs(&self.mailbox_ids).to_local(ObjectType::Mailbox, resolve)?;
        if mailboxes.is_empty() {
            return Err(JmapError::malformed("email has no resolvable mailbox"));
        }
        let received_at = UtcText::format(&self.received_at.unwrap_or(UtcDate::UNIX_EPOCH))?;
        let keywords = KeywordNames(&self.keywords).to_json()?;
        Ok(store.emails().insert(&EmailRecord {
            blob,
            received_at: &received_at,
            mailboxes: &mailboxes,
            keywords: &keywords,
            message_match: Some(message_match),
        })?)
    }

    pub(crate) fn update_placement(
        store: Store<'_>,
        id: LocalId,
        obj: &Value,
        resolve: &impl Fn(ObjectType, &str) -> Option<LocalId>,
    ) -> Result<Write, JmapError> {
        let mailbox_ids = match obj.get("mailboxIds") {
            Some(v) => IndexMap::<JmapId, bool>::deserialize(v)?,
            None => IndexMap::new(),
        };
        let mailboxes = IdRefs(&mailbox_ids).to_local(ObjectType::Mailbox, resolve)?;
        if mailboxes.is_empty() {
            return Err(JmapError::malformed(
                "email update has no resolvable mailbox",
            ));
        }
        let keywords = match obj.get("keywords") {
            Some(v) => IndexMap::<String, bool>::deserialize(v)?,
            None => IndexMap::new(),
        };
        let keywords = KeywordNames(&keywords).to_json()?;
        Ok(store.emails().update_placement(id, &mailboxes, &keywords)?)
    }
}
