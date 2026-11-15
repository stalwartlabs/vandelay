/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::archive::rows::MemberList;
use crate::types::ObjectType;

const MAILBOX_PROPS: &[&str] = &["name", "parentId", "role", "sortOrder", "isSubscribed"];
const EMAIL_PROPS: &[&str] = &["blobId", "receivedAt", "mailboxIds", "keywords"];
const EMAIL_UPDATE_PROPS: &[&str] = &["mailboxIds", "keywords"];
const SIEVE_PROPS: &[&str] = &["name", "isActive", "blobId"];
const FILE_NODE_PROPS: &[&str] = &[
    "@type",
    "parentId",
    "blobId",
    "target",
    "name",
    "mediaType",
    "executable",
    "created",
    "modified",
    "isSubscribed",
    "role",
];

pub(super) const IMPORT_ORDER: [ObjectType; 10] = [
    ObjectType::Mailbox,
    ObjectType::AddressBook,
    ObjectType::Calendar,
    ObjectType::FileNode,
    ObjectType::Identity,
    ObjectType::SieveScript,
    ObjectType::ParticipantIdentity,
    ObjectType::Email,
    ObjectType::ContactCard,
    ObjectType::CalendarEvent,
];

impl ObjectType {
    pub(super) const fn is_queryless(self) -> bool {
        matches!(
            self,
            ObjectType::Identity
                | ObjectType::AddressBook
                | ObjectType::Calendar
                | ObjectType::ParticipantIdentity
        )
    }

    pub(super) const fn is_tree(self) -> bool {
        matches!(self, ObjectType::Mailbox | ObjectType::FileNode)
    }

    pub(super) const fn supports_changes(self) -> bool {
        !matches!(self, ObjectType::SieveScript)
    }

    pub(super) const fn get_props(self) -> Option<&'static [&'static str]> {
        match self {
            ObjectType::Mailbox => Some(MAILBOX_PROPS),
            ObjectType::Email => Some(EMAIL_PROPS),
            ObjectType::SieveScript => Some(SIEVE_PROPS),
            ObjectType::FileNode => Some(FILE_NODE_PROPS),
            _ => None,
        }
    }

    pub(super) const fn update_props(self) -> Option<&'static [&'static str]> {
        match self {
            ObjectType::Email => Some(EMAIL_UPDATE_PROPS),
            other => other.get_props(),
        }
    }

    pub(super) const fn member_list(self) -> Option<MemberList> {
        match self {
            ObjectType::Mailbox => Some(MemberList::EmailMailboxes),
            ObjectType::Calendar => Some(MemberList::EventCalendars),
            ObjectType::AddressBook => Some(MemberList::CardAddressBooks),
            _ => None,
        }
    }
}
