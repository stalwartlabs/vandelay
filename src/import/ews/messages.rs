/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use encodify::base64::LENIENT;

use crate::archive::rows::EmailRecord;
use crate::error::Error;
use crate::exchange::ews::parse::MessageItem;
use crate::exchange::ews::types::ItemClass;
use crate::sync::TypeCounts;
use crate::sync::dates::Rfc3339;
use crate::sync::emailmeta::EmailMeta;
use crate::sync::keywords::{Keywords, MessageFlags};
use crate::types::ObjectType;

use super::coordinator::EwsCoordinator;
use super::folder_sync::Placement;
use super::items::EnumerationOutcome;

const FLAGGED: &str = "Flagged";

impl EwsCoordinator<'_> {
    pub fn import_message(
        &self,
        xml: &str,
        at: &Placement<'_>,
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        let item =
            MessageItem::parse(xml).map_err(|e| Error::Partial(format!("unparsable item: {e}")))?;
        if item.id.id.is_empty() {
            counts.failed += 1;
            return Ok(());
        }
        let logger = self.items.logger;
        let Some(mime_b64) = item.mime_content.as_ref() else {
            counts.skipped += 1;
            logger.warn(format_args!(
                "message {} has no MimeContent; skipping",
                item.id.id
            ));
            return Ok(());
        };
        let bytes = match LENIENT.decode(mime_b64) {
            Ok(b) => b,
            Err(e) => {
                counts.failed += 1;
                logger.warn(format_args!(
                    "message {} MimeContent base64 decode failed: {e}",
                    item.id.id
                ));
                return Ok(());
            }
        };
        let message_match = EmailMeta::from_blob(&bytes).index.to_json();
        let keywords = item.keywords().to_json();
        let mailboxes = [at.folder];
        let received_at = item.date_time_received.as_deref().unwrap_or(Rfc3339::EPOCH);

        let tx = self.store.begin().map_err(Error::partial)?;
        let store = tx.store();
        let ids = self.ids_in(store);
        let record = EmailRecord {
            blob: store.blobs().intern(&bytes).map_err(Error::partial)?,
            received_at,
            mailboxes: &mailboxes,
            keywords: &keywords,
            message_match: Some(&message_match),
        };
        match at.existing.get(&item.id.id) {
            Some(&id) => {
                store.emails().update(id, &record).map_err(Error::partial)?;
                ids.update_change_key(ObjectType::Email, &item.id.id, &item.id.change_key)
                    .map_err(Error::partial)?;
                counts.fetched += 1;
            }
            None => {
                let id = store.emails().insert(&record).map_err(Error::partial)?;
                ids.insert(
                    ObjectType::Email,
                    at.folder_id,
                    &item.id.id,
                    &item.id.change_key,
                    id,
                )
                .map_err(Error::partial)?;
                counts.created += 1;
            }
        }
        tx.commit().map_err(Error::partial)
    }
}

impl MessageItem {
    fn keywords(&self) -> Keywords {
        let mut keywords = MessageFlags {
            read: self.is_read == Some(true),
            draft: self.is_draft == Some(true),
            flagged: self.flag_status.as_deref() == Some(FLAGGED),
        }
        .keywords();
        for category in &self.categories {
            keywords.push_category(category);
        }
        keywords
    }
}

impl EnumerationOutcome {
    pub fn drop_non_mail(&mut self) -> Vec<(String, u64)> {
        let mut skipped: Vec<(String, u64)> = Vec::new();
        self.items.retain(|item| {
            let Some(class) = item
                .item_class
                .as_deref()
                .filter(|c| ItemClass(c).is_non_mail())
            else {
                return true;
            };
            match skipped
                .iter_mut()
                .find(|(c, _)| c.eq_ignore_ascii_case(class))
            {
                Some((_, n)) => *n += 1,
                None => skipped.push((class.to_owned(), 1)),
            }
            false
        });
        skipped
    }
}

#[cfg(test)]
mod tests {
    use crate::exchange::ews::parse::MessageItem;
    use crate::exchange::ews::types::ItemId;
    use crate::import::ews::items::{EnumeratedItem, EnumerationMode, EnumerationOutcome};

    #[test]
    fn non_mail_items_are_dropped_and_counted_per_class() {
        let mut outcome = EnumerationOutcome {
            items: [
                ("M1", Some("IPM.Note")),
                ("C1", Some("IPM.Contact")),
                ("R1", Some("IPM.Schedule.Meeting.Request")),
                ("C2", Some("IPM.Contact")),
                ("T1", Some("IPM.Task")),
                ("P1", Some("IPM.Post")),
                ("X1", None),
            ]
            .into_iter()
            .map(|(id, class)| EnumeratedItem {
                element: "Item".to_owned(),
                id: ItemId::new(id, "K"),
                item_class: class.map(str::to_owned),
            })
            .collect(),
            mode: EnumerationMode::Full,
        };
        let skipped = outcome.drop_non_mail();
        let kept: Vec<&str> = outcome.items.iter().map(|i| i.id.id.as_str()).collect();
        assert_eq!(kept, ["M1", "R1", "P1", "X1"]);
        assert_eq!(
            skipped,
            [("IPM.Contact".to_owned(), 2), ("IPM.Task".to_owned(), 1)]
        );
    }

    #[test]
    fn read_draft_flagged_and_categories_map_to_jmap_keywords() {
        let item = MessageItem {
            is_read: Some(true),
            is_draft: Some(true),
            flag_status: Some("Flagged".to_owned()),
            categories: vec!["Red Category".to_owned(), "VIP".to_owned()],
            ..MessageItem::default()
        };
        assert_eq!(
            item.keywords().to_json(),
            r#"["$seen","$draft","$flagged","red_category","vip"]"#
        );
    }

    #[test]
    fn categories_become_valid_deduplicated_keywords() {
        let item = MessageItem {
            categories: vec![
                "Catégorie Rouge".to_owned(),
                "catégorie rouge".to_owned(),
                "$seen".to_owned(),
                "(  )".to_owned(),
            ],
            ..MessageItem::default()
        };
        assert_eq!(item.keywords().to_json(), r#"["cat_gorie_rouge","seen"]"#);
    }

    #[test]
    fn unread_unflagged_message_has_no_seen_or_flagged() {
        let item = MessageItem {
            is_read: Some(false),
            flag_status: Some("NotFlagged".to_owned()),
            ..MessageItem::default()
        };
        assert_eq!(item.keywords().to_json(), "[]");
    }
}
