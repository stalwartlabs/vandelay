/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use serde_json::{Map, Value, json};

use crate::archive::rows::ContactCardRecord;
use crate::error::Error;
use crate::exchange::ews::parse::ContactItemRaw;
use crate::sync::TypeCounts;
use crate::types::ObjectType;

use super::coordinator::EwsCoordinator;
use super::folder_sync::Placement;

impl EwsCoordinator<'_> {
    pub fn import_contact(
        &self,
        xml: &str,
        at: &Placement<'_>,
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        let item = ContactItemRaw::parse(xml)
            .map_err(|e| Error::Partial(format!("unparsable item: {e}")))?;
        if item.id.id.is_empty() {
            counts.failed += 1;
            return Ok(());
        }
        let mut card = item.to_jscontact();
        if let Some(photo) = item.attachments.iter().find(|a| a.is_contact_photo)
            && let Err(e) = self.attach_photo(&photo.attachment_id, &mut card)
        {
            self.items
                .logger
                .warn(format_args!("contact photo fetch failed: {e}; continuing"));
        }
        let uid = ContactItemRaw::synthetic_uid(&item.id.id);
        let books = [at.folder];
        let data = card.to_string();
        let record = ContactCardRecord {
            uid: &uid,
            address_books: &books,
            data: &data,
        };

        let tx = self.store.begin().map_err(Error::partial)?;
        let store = tx.store();
        let ids = self.ids_in(store);
        match at.existing.get(&item.id.id) {
            Some(&id) => {
                store
                    .contact_cards()
                    .update(id, &record)
                    .map_err(Error::partial)?;
                ids.update_change_key(ObjectType::ContactCard, &item.id.id, &item.id.change_key)
                    .map_err(Error::partial)?;
                counts.fetched += 1;
            }
            None => {
                let id = store
                    .contact_cards()
                    .insert(&record)
                    .map_err(Error::partial)?;
                ids.insert(
                    ObjectType::ContactCard,
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

    fn attach_photo(&self, attachment_id: &str, card: &mut Value) -> Result<(), Error> {
        let Some(photo) = self.items.fetch_attachment(attachment_id)? else {
            return Ok(());
        };
        let blob = self
            .store
            .blobs()
            .intern(&photo.bytes)
            .map_err(Error::partial)?;
        let mut media = Map::new();
        media.insert(
            "photo".to_owned(),
            json!({
                "@type": "Media",
                "kind": "photo",
                "@blob": blob.get(),
                "mediaType": photo.media_type,
            }),
        );
        if let Some(m) = card.as_object_mut() {
            m.insert("media".to_owned(), Value::Object(media));
        }
        Ok(())
    }
}
