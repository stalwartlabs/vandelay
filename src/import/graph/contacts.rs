/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashSet;
use std::mem;

use serde_json::{Value, json};

use crate::archive::Store;
use crate::archive::rows::ContactCardRecord;
use crate::console::Counted;
use crate::error::Error;
use crate::exchange::graph::api::Endpoints;
use crate::exchange::graph::client::{Accept, GraphClient};
use crate::exchange::graph::contact_map::{ConvertedContact, GraphContact};
use crate::exchange::graph::error::GraphError;
use crate::sync::TypeCounts;
use crate::sync::diff::IdDiff;
use crate::sync::pool::Pool;
use crate::types::ObjectType;

use super::coordinator::{CHUNK_SIZE, ContactPhotos, GraphCoordinator};
use super::folders::GraphFolder;

const OCTET_STREAM: &str = "application/octet-stream";

#[derive(Clone, Copy)]
struct ImageBytes<'a>(&'a [u8]);

impl ImageBytes<'_> {
    fn media_type(self) -> Option<&'static str> {
        let bytes = self.0;
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some("image/png")
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some("image/jpeg")
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some("image/gif")
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            Some("image/webp")
        } else if bytes.starts_with(b"BM") {
            Some("image/bmp")
        } else {
            None
        }
    }
}

struct ContactPhoto {
    bytes: Vec<u8>,
    media_type: String,
}

impl ContactPhoto {
    fn fetch(client: &GraphClient, endpoints: &Endpoints, id: &str) -> Option<ContactPhoto> {
        let response = client
            .get_with_prefer(&endpoints.contact_photo(id), Accept::Binary, &[])
            .ok()?;
        if response.body.is_empty() {
            return None;
        }
        let media_type = match ImageBytes(&response.body).media_type() {
            Some(sniffed) => sniffed.to_owned(),
            None => response
                .content_type
                .unwrap_or_else(|| OCTET_STREAM.to_owned()),
        };
        Some(ContactPhoto {
            bytes: response.body,
            media_type,
        })
    }

    fn attach(&self, store: Store<'_>, card: &mut Value) -> rusqlite::Result<()> {
        let blob = store.blobs().intern(&self.bytes)?;
        if let Some(map) = card.as_object_mut() {
            map.insert(
                "media".to_owned(),
                json!({
                    "photo": {
                        "@type": "Media",
                        "kind": "photo",
                        "@blob": blob.get(),
                        "mediaType": self.media_type,
                    }
                }),
            );
        }
        Ok(())
    }
}

struct FetchedContact {
    graph_id: String,
    contact: Result<Value, GraphError>,
    photo: Option<ContactPhoto>,
}

struct NewContact {
    graph_id: String,
    converted: ConvertedContact,
    photo: Option<ContactPhoto>,
}

impl GraphCoordinator<'_> {
    pub fn reconcile_contacts(
        &self,
        books: &[GraphFolder],
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        let local = self.known(ObjectType::ContactCard)?;
        let mut server_total: HashSet<String> = HashSet::new();
        let mut any_failure = false;

        for book in books {
            let url = self
                .endpoints
                .contact_folder_contacts_ids(&book.graph_id, self.top);
            let ids = match self.client.collect_all_ids(&url, &[]) {
                Ok(ids) => ids,
                Err(e) => {
                    self.logger.warn(format_args!(
                        "address book {} enumeration failed: {e}",
                        book.graph_id
                    ));
                    counts.failed += 1;
                    any_failure = true;
                    continue;
                }
            };
            self.logger.progress(format_args!(
                "Address book {}: {} listed",
                book.graph_id,
                Counted::new(ids.len() as u64, "contact card", "contact cards")
            ));
            let mut wanted = Vec::new();
            for id in ids {
                if local.contains_key(&id) {
                    counts.fetched += 1;
                    server_total.insert(id);
                } else if server_total.insert(id.clone()) {
                    wanted.push(id);
                }
            }
            if wanted.is_empty() {
                continue;
            }
            let mut converted = Vec::with_capacity(wanted.len());
            for fetched in self.fetch_contacts(wanted) {
                let graph_id = fetched.graph_id;
                match fetched.contact {
                    Ok(raw) => match GraphContact(&raw).convert() {
                        Ok(contact) => converted.push(NewContact {
                            graph_id,
                            converted: contact,
                            photo: fetched.photo,
                        }),
                        Err(e) => {
                            counts.failed += 1;
                            self.logger.warn(format_args!(
                                "contact card {graph_id} could not be converted: {e}"
                            ));
                        }
                    },
                    Err(GraphError::Vanished) => counts.skipped += 1,
                    Err(e) => {
                        counts.failed += 1;
                        self.logger
                            .warn(format_args!("contact card {graph_id} fetch failed: {e}"));
                    }
                }
            }
            self.insert_contacts(book, converted, counts)?;
        }

        if any_failure {
            self.logger.warn(format_args!(
                "vanished contact cards not removed: one or more address books failed to \
                 enumerate; a clean re-run will reconcile deletions"
            ));
            return Ok(());
        }
        let vanished = IdDiff::compute(server_total, &local).vanished;
        self.sweep(ObjectType::ContactCard, &vanished, counts)
    }

    fn fetch_contacts(&self, ids: Vec<String>) -> Vec<FetchedContact> {
        let client = self.client.clone();
        let endpoints = self.endpoints.clone();
        let want_photos = self.contact_photos == ContactPhotos::Fetch;
        let pool: Pool<String, FetchedContact> = Pool::new(self.workers, move |id: String| {
            let contact = client.get_json_with_prefer(&endpoints.contact(&id), &[]);
            let photo = if want_photos && contact.is_ok() {
                ContactPhoto::fetch(&client, &endpoints, &id)
            } else {
                None
            };
            FetchedContact {
                graph_id: id,
                contact,
                photo,
            }
        });
        let total = ids.len();
        for id in ids {
            pool.submit(id);
        }
        pool.results().iter().take(total).collect()
    }

    fn insert_contacts(
        &self,
        book: &GraphFolder,
        mut contacts: Vec<NewContact>,
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        let books = [book.local_id];
        for chunk in contacts.chunks_mut(CHUNK_SIZE) {
            let tx = self.store.begin()?;
            let store = tx.store();
            for contact in chunk {
                let mut card = mem::take(&mut contact.converted.data);
                if let Some(photo) = &contact.photo {
                    photo.attach(store, &mut card)?;
                }
                let id = store.contact_cards().insert(&ContactCardRecord {
                    uid: &contact.converted.uid,
                    address_books: &books,
                    data: &card.to_string(),
                })?;
                self.ids_in(store)
                    .insert(ObjectType::ContactCard, &contact.graph_id, id)?;
                counts.created += 1;
            }
            tx.commit()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn photo_type_comes_from_the_bytes_not_the_header() {
        let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
        assert_eq!(ImageBytes(png).media_type(), Some("image/png"));
        assert_eq!(
            ImageBytes(&[0xFF, 0xD8, 0xFF, 0xE0]).media_type(),
            Some("image/jpeg")
        );
        assert_eq!(ImageBytes(b"GIF89a...").media_type(), Some("image/gif"));
        assert_eq!(
            ImageBytes(b"RIFF____WEBPVP8 ").media_type(),
            Some("image/webp")
        );
        assert_eq!(ImageBytes(b"RIFF").media_type(), None);
        assert_eq!(ImageBytes(b"not an image").media_type(), None);
    }
}
