/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use serde_json::Value;

use super::{IdRefs, JsData, LocalIds};
use crate::archive::rows::{ContactCardRecord, Write};
use crate::archive::{BlobId, LocalId, Store};
use crate::jmap::blob::InlineShape;
use crate::jmap::error::JmapError;
use crate::jmap::wire::JmapId;
use crate::jmap::wire::contact_card::ContactCard;
use crate::types::ObjectType;

impl ContactCard {
    pub(crate) const SELECT: &str = "SELECT id, uid, address_book_ids, data FROM contact_cards";

    fn with_record<R>(
        self,
        resolve: &impl Fn(ObjectType, &str) -> Option<LocalId>,
        intern: &mut impl FnMut(&str) -> Result<BlobId, JmapError>,
        write: impl FnOnce(&ContactCardRecord<'_>) -> rusqlite::Result<R>,
    ) -> Result<R, JmapError> {
        let address_books =
            IdRefs(&self.address_book_ids).to_local(ObjectType::AddressBook, resolve)?;
        let mut data = JsData::from_rest(self.rest);
        let uid = data
            .take_string("uid")
            .ok_or_else(|| JmapError::malformed("ContactCard has no uid"))?;
        data.intern_blobs(intern)?;
        Ok(write(&ContactCardRecord {
            uid: &uid,
            address_books: &address_books,
            data: &data.text(),
        })?)
    }

    pub(crate) fn insert(
        self,
        store: Store<'_>,
        resolve: &impl Fn(ObjectType, &str) -> Option<LocalId>,
        intern: &mut impl FnMut(&str) -> Result<BlobId, JmapError>,
    ) -> Result<LocalId, JmapError> {
        self.with_record(resolve, intern, |r| store.contact_cards().insert(r))
    }

    pub(crate) fn update(
        self,
        store: Store<'_>,
        id: LocalId,
        resolve: &impl Fn(ObjectType, &str) -> Option<LocalId>,
        intern: &mut impl FnMut(&str) -> Result<BlobId, JmapError>,
    ) -> Result<Write, JmapError> {
        self.with_record(resolve, intern, |r| store.contact_cards().update(id, r))
    }

    pub(crate) fn export_json(
        uid: &str,
        address_book_ids: &str,
        data: &str,
        resolve: &impl Fn(ObjectType, LocalId) -> Option<JmapId>,
        bytes: &impl Fn(BlobId) -> Result<Vec<u8>, JmapError>,
    ) -> Result<Value, JmapError> {
        let mut value = JsData::parse(data)?;
        value.inline_blobs(InlineShape::JsContactResource, bytes)?;
        let books =
            LocalIds::parse(address_book_ids)?.to_target(ObjectType::AddressBook, resolve)?;
        value.insert("uid", Value::from(uid));
        value.insert("addressBookIds", books);
        Ok(value.0)
    }
}
