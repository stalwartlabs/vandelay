/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt;

use serde_json::{Map, Value};

use crate::exchange::address::SmtpAddress;

const MAILTO: &str = "mailto:";
const SYNTHETIC_ATTENDEE_PREFIX: &str = "urn:x-vandelay:attendee:";
const PARTICIPANTS: &str = "participants";
const CALENDAR_ADDRESS: &str = "calendarAddress";

const CALENDAR_ADDRESS_DEPENDENTS: &[&str] = &[
    "calendarAddress",
    "delegatedFrom",
    "delegatedTo",
    "email",
    "expectReply",
    "kind",
    "memberOf",
    "participationStatus",
    "progress",
    "roles",
    "sentBy",
];

#[derive(Debug, Clone, Copy)]
pub struct SyntheticAttendee<'a>(pub &'a str);

impl fmt::Display for SyntheticAttendee<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{SYNTHETIC_ATTENDEE_PREFIX}{}",
            blake3::hash(self.0.as_bytes()).to_hex()
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routing {
    Smtp,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAddress {
    pub uri: String,
    pub email: Option<String>,
}

impl ResolvedAddress {
    pub fn resolve(
        address: Option<&str>,
        routing: Routing,
        name: Option<&str>,
    ) -> Option<ResolvedAddress> {
        if let Some(addr) = address.filter(|a| !a.trim().is_empty()) {
            let smtp = match routing {
                Routing::Smtp => SmtpAddress::parse(addr),
                Routing::Other => SmtpAddress::extract(addr),
            };
            return Some(match smtp {
                Some(smtp) => ResolvedAddress {
                    uri: format!("{MAILTO}{}", smtp.as_str()),
                    email: Some(smtp.into_string()),
                },
                None => ResolvedAddress::synthetic(addr),
            });
        }
        name.filter(|n| !n.trim().is_empty())
            .map(ResolvedAddress::synthetic)
    }

    fn synthetic(identifier: &str) -> ResolvedAddress {
        ResolvedAddress {
            uri: SyntheticAttendee(identifier).to_string(),
            email: None,
        }
    }
}

pub struct ParticipantMap<'a>(pub &'a mut Map<String, Value>);

impl ParticipantMap<'_> {
    pub fn drop_calendar_address_dependents(self) {
        for object in self.0.values_mut().filter_map(Value::as_object_mut) {
            for key in CALENDAR_ADDRESS_DEPENDENTS {
                object.remove(*key);
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct OverridePointer<'a>(pub &'a str);

impl OverridePointer<'_> {
    pub fn is_ignored(self) -> bool {
        let pointer = self.0;
        if hashify::set!(
            pointer.as_bytes(),
            "@type",
            "method",
            "organizerCalendarAddress",
            "privacy",
            "prodId",
            "recurrenceId",
            "recurrenceIdTimeZone",
            "sentBy",
            "uid",
        ) || self.is_participant_calendar_address()
        {
            return true;
        }
        let first = pointer.split('/').next().unwrap_or(pointer);
        hashify::set!(
            first.as_bytes(),
            "recurrenceOverrides",
            "recurrenceRule",
            "relatedTo",
        )
    }

    fn is_participant_calendar_address(self) -> bool {
        let mut tokens = self.0.split('/');
        tokens.next() == Some(PARTICIPANTS)
            && tokens.next().is_some()
            && tokens.next() == Some(CALENDAR_ADDRESS)
            && tokens.next().is_none()
    }
}

pub struct OverridePatch;

impl OverridePatch {
    pub fn from_event(event: &Value) -> Value {
        let Some(object) = event.as_object() else {
            return event.clone();
        };
        Value::Object(
            object
                .iter()
                .filter(|(key, _)| !OverridePointer(key).is_ignored())
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exact_pointers_are_ignored() {
        for pointer in [
            "@type",
            "method",
            "organizerCalendarAddress",
            "privacy",
            "prodId",
            "recurrenceId",
            "recurrenceIdTimeZone",
            "sentBy",
            "uid",
        ] {
            assert!(
                OverridePointer(pointer).is_ignored(),
                "{pointer} must be ignored"
            );
        }
    }

    #[test]
    fn first_token_pointers_are_ignored_at_any_depth() {
        assert!(OverridePointer("recurrenceRule").is_ignored());
        assert!(OverridePointer("recurrenceRule/frequency").is_ignored());
        assert!(OverridePointer("recurrenceOverrides/2026-01-01T09:00:00").is_ignored());
        assert!(OverridePointer("relatedTo/uid-1/relation").is_ignored());
    }

    #[test]
    fn participant_calendar_address_is_ignored_only_at_its_own_depth() {
        assert!(OverridePointer("participants/att-1/calendarAddress").is_ignored());
        assert!(!OverridePointer("participants").is_ignored());
        assert!(!OverridePointer("participants/att-1").is_ignored());
        assert!(!OverridePointer("participants/att-1/participationStatus").is_ignored());
        assert!(!OverridePointer("participants/att-1/calendarAddress/extra").is_ignored());
    }

    #[test]
    fn patchable_pointers_survive() {
        for pointer in [
            "start",
            "duration",
            "title",
            "excluded",
            "participants",
            "created",
            "updated",
            "status",
        ] {
            assert!(
                !OverridePointer(pointer).is_ignored(),
                "{pointer} must survive"
            );
        }
    }

    #[test]
    fn an_event_is_reduced_to_a_patch() {
        let event = json!({
            "@type": "Event",
            "uid": "uid-1",
            "prodId": "vandelay",
            "privacy": "public",
            "organizerCalendarAddress": "mailto:alice@example.com",
            "recurrenceRule": {"frequency": "daily"},
            "start": "2026-03-04T09:00:00",
            "duration": "PT1H",
            "title": "Moved"
        });
        let patch = OverridePatch::from_event(&event);
        let patch = patch.as_object().expect("patch object");
        assert_eq!(patch.len(), 3);
        assert_eq!(patch["start"], "2026-03-04T09:00:00");
        assert_eq!(patch["duration"], "PT1H");
        assert_eq!(patch["title"], "Moved");
    }

    #[test]
    fn an_exclusion_patch_is_preserved() {
        let patch = OverridePatch::from_event(&json!({"excluded": true}));
        assert_eq!(patch, json!({"excluded": true}));
    }

    #[test]
    fn a_synthetic_address_is_stable_per_identifier_and_never_a_mailto() {
        let addr = SyntheticAttendee("Jane Doe").to_string();
        assert_eq!(addr, SyntheticAttendee("Jane Doe").to_string());
        assert_ne!(addr, SyntheticAttendee("John Doe").to_string());
        assert!(addr.starts_with("urn:x-vandelay:attendee:"));
        assert!(
            !addr.starts_with("mailto:"),
            "export must never invite a fabricated address"
        );
    }

    #[test]
    fn calendar_address_dependents_are_dropped_and_identity_is_kept() {
        let mut participants = json!({
            "1": {
                "@type": "Participant",
                "name": "Room 4",
                "calendarAddress": "urn:x-vandelay:attendee:abc",
                "email": "room4@example.com",
                "roles": {"required": true},
                "kind": "resource",
                "participationStatus": "accepted",
                "expectReply": true,
                "description": "Third floor"
            }
        })
        .as_object()
        .expect("participants object")
        .clone();
        ParticipantMap(&mut participants).drop_calendar_address_dependents();
        let p = &participants["1"];
        assert_eq!(p["@type"], "Participant");
        assert_eq!(p["name"], "Room 4");
        assert_eq!(p["description"], "Third floor");
        for key in CALENDAR_ADDRESS_DEPENDENTS {
            assert!(p.get(*key).is_none(), "{key} must be dropped");
        }
    }
}
