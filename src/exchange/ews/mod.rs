/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

pub mod autodiscover;
pub mod calendar_map;
pub mod client;
pub mod contact_map;
pub mod error;
pub mod oauth;
pub mod parse;
pub mod recurrence;
pub mod retry;
pub mod soap;
pub mod srv;
pub mod types;
pub mod xml;

pub use client::EwsClient;
pub use error::EwsError;
pub use types::{
    DistinguishedFolderId, FolderId, ItemId, MailboxKind, ResponseCode, ServerVersion,
};
