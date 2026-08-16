/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use serde_json::{Map, Value};

const IGNORED_EXACT: &[&str] = &[
    "@type",
    "method",
    "organizerCalendarAddress",
    "privacy",
    "prodId",
    "recurrenceId",
    "recurrenceIdTimeZone",
    "sentBy",
    "uid",
];

const IGNORED_FIRST_TOKEN: &[&str] = &["recurrenceOverrides", "recurrenceRule", "relatedTo"];

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

pub fn synthetic_attendee_address(identifier: &str) -> String {
    format!(
        "urn:x-vandelay:attendee:{}",
        blake3::hash(identifier.as_bytes()).to_hex()
    )
}

pub fn drop_calendar_address_dependents(participants: &mut Map<String, Value>) {
    for participant in participants.values_mut() {
        let Some(object) = participant.as_object_mut() else {
            continue;
        };
        for key in CALENDAR_ADDRESS_DEPENDENTS {
            object.remove(*key);
        }
    }
}

pub fn is_override_ignored(pointer: &str) -> bool {
    if IGNORED_EXACT.contains(&pointer) || is_participant_calendar_address(pointer) {
        return true;
    }
    let first = pointer.split('/').next().unwrap_or(pointer);
    IGNORED_FIRST_TOKEN.contains(&first)
}

fn is_participant_calendar_address(pointer: &str) -> bool {
    let mut tokens = pointer.split('/');
    tokens.next() == Some("participants")
        && tokens.next().is_some()
        && tokens.next() == Some("calendarAddress")
        && tokens.next().is_none()
}

pub fn override_patch_from_event(event: &Value) -> Value {
    let Some(object) = event.as_object() else {
        return event.clone();
    };
    let patch: Map<String, Value> = object
        .iter()
        .filter(|(key, _)| !is_override_ignored(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    Value::Object(patch)
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
            assert!(is_override_ignored(pointer), "{pointer} must be ignored");
        }
    }

    #[test]
    fn first_token_pointers_are_ignored_at_any_depth() {
        assert!(is_override_ignored("recurrenceRule"));
        assert!(is_override_ignored("recurrenceRule/frequency"));
        assert!(is_override_ignored(
            "recurrenceOverrides/2026-01-01T09:00:00"
        ));
        assert!(is_override_ignored("relatedTo/uid-1/relation"));
    }

    #[test]
    fn participant_calendar_address_is_ignored_only_at_its_own_depth() {
        assert!(is_override_ignored("participants/att-1/calendarAddress"));
        assert!(!is_override_ignored("participants"));
        assert!(!is_override_ignored("participants/att-1"));
        assert!(!is_override_ignored(
            "participants/att-1/participationStatus"
        ));
        assert!(!is_override_ignored(
            "participants/att-1/calendarAddress/extra"
        ));
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
            assert!(!is_override_ignored(pointer), "{pointer} must survive");
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
        let patch = override_patch_from_event(&event);
        let patch = patch.as_object().expect("patch object");
        assert_eq!(patch.len(), 3);
        assert_eq!(patch["start"], "2026-03-04T09:00:00");
        assert_eq!(patch["duration"], "PT1H");
        assert_eq!(patch["title"], "Moved");
    }

    #[test]
    fn an_exclusion_patch_is_preserved() {
        let patch = override_patch_from_event(&json!({"excluded": true}));
        assert_eq!(patch, json!({"excluded": true}));
    }

    #[test]
    fn a_synthetic_address_is_stable_per_identifier_and_never_a_mailto() {
        let addr = synthetic_attendee_address("Jane Doe");
        assert_eq!(addr, synthetic_attendee_address("Jane Doe"));
        assert_ne!(addr, synthetic_attendee_address("John Doe"));
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
        drop_calendar_address_dependents(&mut participants);
        let p = &participants["1"];
        assert_eq!(p["@type"], "Participant");
        assert_eq!(p["name"], "Room 4");
        assert_eq!(p["description"], "Third floor");
        for key in CALENDAR_ADDRESS_DEPENDENTS {
            assert!(p.get(*key).is_none(), "{key} must be dropped");
        }
    }
}
