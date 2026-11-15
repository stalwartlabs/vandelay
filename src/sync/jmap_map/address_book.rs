/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::Row;

use crate::archive::rows::{AddressBookRecord, Subscribed, Write};
use crate::archive::{LocalId, Store};
use crate::jmap::error::JmapError;
use crate::jmap::wire::address_book::AddressBook;

impl AddressBook {
    pub(crate) const SELECT: &str =
        "SELECT id, name, description, sort_order, is_default, is_subscribed FROM address_books";

    fn record(&self) -> AddressBookRecord<'_> {
        AddressBookRecord {
            name: &self.name,
            description: self.description.as_deref(),
            sort_order: i64::from(self.sort_order),
            is_default: self.is_default,
            subscribed: Subscribed::from(self.is_subscribed),
        }
    }

    pub(crate) fn insert(&self, store: Store<'_>) -> Result<LocalId, JmapError> {
        Ok(store.address_books().insert(&self.record())?)
    }

    pub(crate) fn update(&self, store: Store<'_>, id: LocalId) -> Result<Write, JmapError> {
        Ok(store.address_books().update(id, &self.record())?)
    }

    pub(crate) fn from_row(row: &Row<'_>) -> Result<AddressBook, JmapError> {
        Ok(AddressBook {
            id: None,
            name: row.get(1)?,
            description: row.get(2)?,
            sort_order: row.get::<_, i64>(3)? as u32,
            is_default: row.get::<_, i64>(4)? != 0,
            is_subscribed: row.get::<_, i64>(5)? != 0,
        })
    }
}
