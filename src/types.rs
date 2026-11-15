/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::ops::Deref;
use std::str::FromStr;

use crate::error::Error;

macro_rules! object_types {
    ($($variant:ident => $token:literal, $jmap:literal, $table:literal, $urn:literal, $label:literal, $noun:literal;)+) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum ObjectType {
            $($variant,)+
        }

        impl ObjectType {
            pub const ALL: [ObjectType; 10] = [$(ObjectType::$variant,)+];

            pub const fn token(self) -> &'static str {
                match self {
                    $(ObjectType::$variant => $token,)+
                }
            }

            pub const fn jmap_name(self) -> &'static str {
                match self {
                    $(ObjectType::$variant => $jmap,)+
                }
            }

            pub const fn label(self) -> &'static str {
                match self {
                    $(ObjectType::$variant => $label,)+
                }
            }

            pub const fn noun(self) -> &'static str {
                match self {
                    $(ObjectType::$variant => $noun,)+
                }
            }

            pub const fn table(self) -> &'static str {
                match self {
                    $(ObjectType::$variant => $table,)+
                }
            }

            pub const fn capability_urn(self) -> &'static str {
                match self {
                    $(ObjectType::$variant => $urn,)+
                }
            }

            fn from_token(token: &str) -> Option<ObjectType> {
                hashify::map_ignore_case!(
                    token.as_bytes(),
                    ObjectType,
                    $($token => ObjectType::$variant,)+
                )
                .copied()
            }
        }
    };
}

object_types! {
    Mailbox => "mailbox", "Mailbox", "mailboxes", "urn:ietf:params:jmap:mail", "Mailboxes", "mailbox";
    Email => "email", "Email", "emails", "urn:ietf:params:jmap:mail", "Emails", "email";
    Identity => "identity", "Identity", "identities", "urn:ietf:params:jmap:submission", "Identities", "identity";
    SieveScript => "sievescript", "SieveScript", "sieve_scripts", "urn:ietf:params:jmap:sieve", "Sieve scripts", "sieve script";
    AddressBook => "addressbook", "AddressBook", "address_books", "urn:ietf:params:jmap:contacts", "Address books", "address book";
    ContactCard => "contactcard", "ContactCard", "contact_cards", "urn:ietf:params:jmap:contacts", "Contact cards", "contact card";
    Calendar => "calendar", "Calendar", "calendars", "urn:ietf:params:jmap:calendars", "Calendars", "calendar";
    CalendarEvent => "calendarevent", "CalendarEvent", "calendar_events", "urn:ietf:params:jmap:calendars", "Calendar events", "calendar event";
    ParticipantIdentity => "participantidentity", "ParticipantIdentity", "participant_identities", "urn:ietf:params:jmap:calendars", "Participant identities", "participant identity";
    FileNode => "filenode", "FileNode", "file_nodes", "urn:ietf:params:jmap:filenode", "File nodes", "file node";
}

impl FromStr for ObjectType {
    type Err = Error;

    fn from_str(token: &str) -> Result<ObjectType, Error> {
        ObjectType::from_token(token.trim())
            .ok_or_else(|| Error::Usage(format!("unknown object type: {token}")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectTypes(Vec<ObjectType>);

impl FromStr for ObjectTypes {
    type Err = Error;

    fn from_str(list: &str) -> Result<ObjectTypes, Error> {
        let mut out = Vec::new();
        for token in list.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            let parsed = token.parse::<ObjectType>()?;
            if !out.contains(&parsed) {
                out.push(parsed);
            }
        }
        if out.is_empty() {
            return Err(Error::Usage(
                "--objects given but resolved to an empty type list".to_owned(),
            ));
        }
        Ok(ObjectTypes(out))
    }
}

impl From<Vec<ObjectType>> for ObjectTypes {
    fn from(types: Vec<ObjectType>) -> ObjectTypes {
        let mut out = Vec::with_capacity(types.len());
        for ty in types {
            if !out.contains(&ty) {
                out.push(ty);
            }
        }
        ObjectTypes(out)
    }
}

impl Deref for ObjectTypes {
    type Target = [ObjectType];

    fn deref(&self) -> &[ObjectType] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_roundtrip() {
        for t in ObjectType::ALL {
            assert_eq!(t.token().parse::<ObjectType>().unwrap(), t);
        }
    }

    #[test]
    fn parse_list_dedups_and_trims() {
        let parsed: ObjectTypes = " mailbox, email ,mailbox".parse().unwrap();
        assert_eq!(*parsed, [ObjectType::Mailbox, ObjectType::Email]);
    }

    #[test]
    fn parse_list_case_insensitive() {
        let parsed: ObjectTypes = "Mailbox,EMAIL".parse().unwrap();
        assert_eq!(*parsed, [ObjectType::Mailbox, ObjectType::Email]);
    }

    #[test]
    fn parse_list_rejects_unknown() {
        assert!("mailbox,bogus".parse::<ObjectTypes>().is_err());
    }

    #[test]
    fn parse_list_rejects_empty() {
        assert!("  ,  ".parse::<ObjectTypes>().is_err());
    }

    #[test]
    fn tables_match_schema_names() {
        assert_eq!(ObjectType::Mailbox.table(), "mailboxes");
        assert_eq!(
            ObjectType::ParticipantIdentity.table(),
            "participant_identities"
        );
        assert_eq!(ObjectType::FileNode.table(), "file_nodes");
    }

    #[test]
    fn unknown_type_reports_the_raw_token() {
        let err = " Bogus ".parse::<ObjectType>().unwrap_err();
        assert_eq!(err.to_string(), "unknown object type:  Bogus ");
    }
}
