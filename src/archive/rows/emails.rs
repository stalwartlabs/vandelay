/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::{OptionalExtension, params};

use super::{Emails, LocalIdArray, Write};
use crate::archive::{BlobId, LocalId};

const EMPTY_MATCH: &str = "{}";

#[derive(Debug, Clone, Copy)]
pub struct EmailRecord<'r> {
    pub blob: BlobId,
    pub received_at: &'r str,
    pub mailboxes: &'r [LocalId],
    pub keywords: &'r str,
    pub message_match: Option<&'r str>,
}

impl Emails<'_> {
    pub fn insert(&self, e: &EmailRecord<'_>) -> rusqlite::Result<LocalId> {
        self.conn.execute(
            "INSERT INTO emails (blob_id, received_at, mailbox_ids, keywords, message_match)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                e.blob,
                e.received_at,
                LocalIdArray(e.mailboxes),
                e.keywords,
                e.message_match.unwrap_or(EMPTY_MATCH)
            ],
        )?;
        Ok(LocalId(self.conn.last_insert_rowid()))
    }

    pub fn update(&self, id: LocalId, e: &EmailRecord<'_>) -> rusqlite::Result<Write> {
        self.conn
            .prepare_cached(
                "UPDATE emails SET blob_id = ?1, received_at = ?2, mailbox_ids = ?3, keywords = ?4,
                    message_match = ?5
                 WHERE id = ?6 AND (blob_id IS NOT ?1 OR received_at IS NOT ?2
                    OR mailbox_ids IS NOT ?3 OR keywords IS NOT ?4 OR message_match IS NOT ?5)",
            )?
            .execute(params![
                e.blob,
                e.received_at,
                LocalIdArray(e.mailboxes),
                e.keywords,
                e.message_match.unwrap_or(EMPTY_MATCH),
                id
            ])
            .map(Write::from_rows)
    }

    pub fn update_placement(
        &self,
        id: LocalId,
        mailboxes: &[LocalId],
        keywords: &str,
    ) -> rusqlite::Result<Write> {
        self.conn
            .execute(
                "UPDATE emails SET mailbox_ids = ?1, keywords = ?2
                 WHERE id = ?3 AND (mailbox_ids IS NOT ?1 OR keywords IS NOT ?2)",
                params![LocalIdArray(mailboxes), keywords, id],
            )
            .map(Write::from_rows)
    }
}

impl Emails<'_> {
    pub fn keywords(&self, id: LocalId) -> rusqlite::Result<Option<(BlobId, String)>> {
        self.conn
            .prepare_cached("SELECT blob_id, keywords FROM emails WHERE id = ?1")?
            .query_row([id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
    }

    pub fn update_keywords(&self, id: LocalId, keywords: &str) -> rusqlite::Result<Write> {
        self.conn
            .prepare_cached("UPDATE emails SET keywords = ?1 WHERE id = ?2 AND keywords IS NOT ?1")?
            .execute(params![keywords, id])
            .map(Write::from_rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Archive;

    #[test]
    fn insert_then_update_placement() {
        let archive = Archive::open_in_memory().unwrap();
        let store = archive.store();
        let blob = store.blobs().intern(b"raw message").unwrap();
        let inbox = [LocalId::from_raw(1)];
        let emails = store.emails();
        let id = emails
            .insert(&EmailRecord {
                blob,
                received_at: "2020-01-01T00:00:00Z",
                mailboxes: &inbox,
                keywords: r#"["$seen"]"#,
                message_match: None,
            })
            .unwrap();
        let (mailbox_ids, matched): (String, String) = archive
            .connection()
            .query_row(
                "SELECT mailbox_ids, message_match FROM emails WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(mailbox_ids, "[1]");
        assert_eq!(matched, "{}");
        assert_eq!(
            emails.update_placement(id, &inbox, r#"["$seen"]"#).unwrap(),
            Write::Unchanged
        );
        assert_eq!(
            emails.update_placement(id, &inbox, "[]").unwrap(),
            Write::Changed
        );
        assert_eq!(emails.keywords(id).unwrap(), Some((blob, "[]".to_owned())));
        assert_eq!(
            emails.update_keywords(id, r#"["$flagged"]"#).unwrap(),
            Write::Changed
        );
        assert_eq!(
            emails.update_keywords(id, r#"["$flagged"]"#).unwrap(),
            Write::Unchanged
        );
        emails.delete(id).unwrap();
        assert_eq!(emails.keywords(id).unwrap(), None);
    }

    #[test]
    fn update_rewrites_the_whole_row_and_is_guarded() {
        let archive = Archive::open_in_memory().unwrap();
        let store = archive.store();
        let old_blob = store.blobs().intern(b"old").unwrap();
        let new_blob = store.blobs().intern(b"new").unwrap();
        let inbox = [LocalId::from_raw(1)];
        let both = [LocalId::from_raw(1), LocalId::from_raw(2)];
        let emails = store.emails();
        let first = EmailRecord {
            blob: old_blob,
            received_at: "2020-01-01T00:00:00Z",
            mailboxes: &inbox,
            keywords: "[]",
            message_match: None,
        };
        let id = emails.insert(&first).unwrap();
        assert_eq!(emails.update(id, &first).unwrap(), Write::Unchanged);
        let second = EmailRecord {
            blob: new_blob,
            received_at: "2021-02-02T00:00:00Z",
            mailboxes: &both,
            keywords: r#"["$seen"]"#,
            message_match: Some(r#"{"f":"00","m":["a@b"]}"#),
        };
        assert_eq!(emails.update(id, &second).unwrap(), Write::Changed);
        assert_eq!(emails.update(id, &second).unwrap(), Write::Unchanged);
        let row: (BlobId, String, String, String, String) = archive
            .connection()
            .query_row(
                "SELECT blob_id, received_at, mailbox_ids, keywords, message_match
                 FROM emails WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (
                new_blob,
                "2021-02-02T00:00:00Z".to_owned(),
                "[1,2]".to_owned(),
                r#"["$seen"]"#.to_owned(),
                r#"{"f":"00","m":["a@b"]}"#.to_owned()
            )
        );
        assert_eq!(
            emails.update(LocalId::from_raw(999), &second).unwrap(),
            Write::Unchanged
        );
    }
}
