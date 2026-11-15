/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::borrow::Cow;
use std::fmt::Write;

use calcard::Entry;
use calcard::icalendar::ICalendar;
use calcard::vcard::VCard;
use serde_json::{Map, Value};

use crate::archive::rows::EventDataType;

const MAX_INVALID_LINE_CHARS: usize = 80;

#[derive(Debug, thiserror::Error)]
pub enum CalcardError {
    #[error("vcard parse failed: {0}")]
    VCardParse(String),
    #[error("icalendar parse failed: {0}")]
    ICalParse(String),
    #[error("jscontact serialisation: {0}")]
    Json(#[from] serde_json::Error),
    #[error("no parseable entries in iCalendar")]
    NoEntries,
    #[error("calcard output is not a JSON object")]
    NotAnObject,
}

#[derive(Debug)]
pub struct JsContact {
    pub uid: String,
    pub data: Value,
}

impl JsContact {
    pub fn from_vcard(text: &str, item_href: &str) -> Result<JsContact, CalcardError> {
        let vcard = VCard::parse(LineBreaks::normalize(text))
            .map_err(|e| CalcardError::VCardParse(CalcardError::describe(e)))?;
        let js = vcard.into_jscontact::<String, String>();
        let value: Value = serde_json::from_str(&js.to_string_pretty())?;
        let mut obj = match value {
            Value::Object(map) => map,
            _ => return Err(CalcardError::NotAnObject),
        };
        let uid = match obj.remove("uid") {
            Some(Value::String(uid)) if !uid.trim().is_empty() => {
                if uid.trim().len() == uid.len() {
                    uid
                } else {
                    uid.trim().to_owned()
                }
            }
            _ => SyntheticUid::derive(item_href),
        };
        Ok(JsContact {
            uid,
            data: Value::Object(obj),
        })
    }
}

pub struct SyntheticUid;

impl SyntheticUid {
    pub fn derive(seed: &str) -> String {
        let hash = blake3::hash(seed.as_bytes());
        format!("vandelay-syn-{}", hash.to_hex())
    }
}

#[derive(Debug)]
pub struct JsCalendarEntry {
    pub data: Value,
    pub data_type: EventDataType,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ExtractedFields {
    pub is_draft: bool,
    pub use_default_alerts: bool,
    pub uid: Option<String>,
}

#[derive(Clone, Copy)]
enum EntryKind {
    Item(EventDataType),
    Group,
}

impl JsCalendarEntry {
    pub fn parse_all(text: &str) -> Result<Vec<JsCalendarEntry>, CalcardError> {
        let ical = ICalendar::parse(LineBreaks::normalize(text))
            .map_err(|e| CalcardError::ICalParse(CalcardError::describe(e)))?;
        let js = ical.into_jscalendar::<String, String>();
        let value: Value = serde_json::from_str(&js.to_string_pretty())?;
        let mut out = Vec::new();
        JsCalendarEntry::collect(value, &mut out);
        if out.is_empty() {
            return Err(CalcardError::NoEntries);
        }
        Ok(out)
    }

    fn collect(value: Value, out: &mut Vec<JsCalendarEntry>) {
        match value {
            Value::Object(mut map) => {
                let kind = map
                    .get("@type")
                    .and_then(Value::as_str)
                    .and_then(|t| {
                        hashify::map!(t.as_bytes(), EntryKind,
                            "Event" => EntryKind::Item(EventDataType::Event),
                            "Task" => EntryKind::Item(EventDataType::Task),
                            "Note" => EntryKind::Item(EventDataType::Note),
                            "Group" => EntryKind::Group,
                        )
                    })
                    .copied();
                match kind {
                    Some(EntryKind::Item(data_type)) if JsCalendarEntry::has_uid(&map) => {
                        out.push(JsCalendarEntry {
                            data: Value::Object(map),
                            data_type,
                        })
                    }
                    Some(EntryKind::Group) => {
                        if let Some(Value::Array(entries)) = map.remove("entries") {
                            for entry in entries {
                                JsCalendarEntry::collect(entry, out);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Value::Array(items) => {
                for item in items {
                    JsCalendarEntry::collect(item, out);
                }
            }
            _ => {}
        }
    }

    fn has_uid(map: &Map<String, Value>) -> bool {
        map.get("uid")
            .and_then(Value::as_str)
            .is_some_and(|u| !u.trim().is_empty())
    }

    pub fn take_extracted(&mut self) -> ExtractedFields {
        let Value::Object(map) = &mut self.data else {
            return ExtractedFields::default();
        };
        let is_draft = map
            .remove("isDraft")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let use_default_alerts = map
            .remove("useDefaultAlerts")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        for key in [
            "calendarIds",
            "method",
            "utcStart",
            "utcEnd",
            "isOrigin",
            "baseEventId",
        ] {
            map.remove(key);
        }
        let uid = map.get("uid").and_then(Value::as_str).map(str::to_owned);
        ExtractedFields {
            is_draft,
            use_default_alerts,
            uid,
        }
    }
}

struct LineBreaks;

impl LineBreaks {
    fn normalize(text: &str) -> Cow<'_, str> {
        let mut after_cr = text.split('\r').skip(1).peekable();
        let mut has_bare_cr = false;
        while let Some(rest) = after_cr.next() {
            if LineBreaks::is_bare_cr(rest, after_cr.peek().is_some()) {
                has_bare_cr = true;
                break;
            }
        }
        if !has_bare_cr {
            return Cow::Borrowed(text);
        }
        let mut parts = text.split('\r').peekable();
        let mut out = String::with_capacity(text.len());
        out.push_str(parts.next().unwrap_or_default());
        while let Some(rest) = parts.next() {
            if LineBreaks::is_bare_cr(rest, parts.peek().is_some()) {
                out.push('\n');
            }
            out.push_str(rest);
        }
        Cow::Owned(out)
    }

    fn is_bare_cr(rest: &str, more: bool) -> bool {
        let followed_by_cr = rest.is_empty() && more;
        !followed_by_cr && !rest.starts_with('\n')
    }
}

impl CalcardError {
    fn describe(entry: Entry) -> String {
        let mut out = String::new();
        match entry {
            Entry::InvalidLine(line) => {
                out.push_str("invalid line ");
                let mut chars = line.chars();
                let head: String = chars.by_ref().take(MAX_INVALID_LINE_CHARS).collect();
                let _ = write!(out, "{head:?}");
                if chars.next().is_some() {
                    out.push_str(" (truncated)");
                }
            }
            Entry::UnexpectedComponentEnd { expected, found } => {
                let _ = write!(
                    out,
                    "found END:{} where END:{} was expected",
                    found.as_str(),
                    expected.as_str()
                );
            }
            Entry::UnterminatedComponent(name) => {
                let _ = write!(out, "{name} is never closed");
            }
            Entry::TooManyComponents => out.push_str("too many components"),
            Entry::Eof => out.push_str("no BEGIN line found"),
            Entry::VCard(_) => out.push_str("found a vCard where an iCalendar was expected"),
            Entry::ICalendar(_) => out.push_str("found an iCalendar where a vCard was expected"),
            _ => out.push_str("unrecognised content"),
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_VCARD: &str = "BEGIN:VCARD\r\n\
VERSION:4.0\r\n\
UID:abc-123\r\n\
FN:Alice Smith\r\n\
EMAIL:alice@example.com\r\n\
END:VCARD\r\n";

    const SAMPLE_ICAL: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//Test//EN\r\n\
BEGIN:VEVENT\r\n\
UID:event-1@example.com\r\n\
DTSTAMP:20260101T000000Z\r\n\
DTSTART:20260101T090000Z\r\n\
DTEND:20260101T100000Z\r\n\
SUMMARY:Test Event\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

    #[test]
    fn vcard_extracts_uid_into_separate_field() {
        let jc = JsContact::from_vcard(SAMPLE_VCARD, "/dav/card/u/d/a.vcf").expect("parse");
        assert_eq!(jc.uid, "abc-123");
        let obj = jc.data.as_object().unwrap();
        assert!(!obj.contains_key("uid"), "uid stripped from data");
    }

    #[test]
    fn ical_extracts_at_least_one_event() {
        let events = JsCalendarEntry::parse_all(SAMPLE_ICAL).expect("parse");
        assert!(!events.is_empty());
        assert_eq!(events[0].data_type, EventDataType::Event);
        let uid = events[0]
            .data
            .as_object()
            .and_then(|m| m.get("uid"))
            .and_then(Value::as_str)
            .unwrap();
        assert!(uid.starts_with("event-1"));
    }

    #[test]
    fn vcard_without_uid_gets_synthetic_uid_from_href() {
        let no_uid = "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Anon\r\nEND:VCARD\r\n";
        let href = "/dav/card/u/d/no-uid.vcf";
        let jc = JsContact::from_vcard(no_uid, href).expect("synthetic uid");
        assert_eq!(jc.uid, SyntheticUid::derive(href));
        assert!(jc.uid.starts_with("vandelay-syn-"));
    }

    #[test]
    fn synthetic_uid_is_stable_per_href() {
        assert_eq!(SyntheticUid::derive("/a"), SyntheticUid::derive("/a"));
        assert_ne!(SyntheticUid::derive("/a"), SyntheticUid::derive("/b"));
    }

    #[test]
    fn ical_with_empty_calendar_fails_no_entries() {
        let empty = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nEND:VCALENDAR\r\n";
        let err = JsCalendarEntry::parse_all(empty).unwrap_err();
        assert!(matches!(err, CalcardError::NoEntries));
    }

    #[test]
    fn vcard_garbage_fails_parse() {
        let err = JsContact::from_vcard("not a vcard", "/x").unwrap_err();
        assert!(matches!(err, CalcardError::VCardParse(_)));
    }

    // Kerio Connect sends calendar-data with bare CR line breaks (issue #48).
    #[test]
    fn ical_with_bare_cr_line_breaks_parses() {
        let ical = SAMPLE_ICAL.replace("\r\n", "\r");
        let entries = JsCalendarEntry::parse_all(&ical).expect("bare CR parses");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].data["title"], "Test Event");
    }

    // A folded line split by a bare CR still unfolds.
    #[test]
    fn ical_with_bare_cr_unfolds_continuation_lines() {
        let ical = "BEGIN:VCALENDAR\rVERSION:2.0\rBEGIN:VEVENT\rUID:k1@example\r\
            DTSTAMP:20260101T000000Z\rDTSTART:20260101T090000Z\r\
            SUMMARY:Quarterly\r  review\rEND:VEVENT\rEND:VCALENDAR\r";
        let entries = JsCalendarEntry::parse_all(ical).expect("parse");
        assert_eq!(entries[0].data["title"], "Quarterly review");
    }

    #[test]
    fn vcard_with_bare_cr_line_breaks_parses() {
        let vcard = SAMPLE_VCARD.replace("\r\n", "\r");
        let jc = JsContact::from_vcard(&vcard, "/x").expect("bare CR parses");
        assert_eq!(jc.uid, "abc-123");
    }

    #[test]
    fn line_break_normalization_borrows_unless_bare_cr() {
        for text in ["A\nB\n", "A\r\nB\r\n", "A\r\r\nB\r\r\n", "no breaks"] {
            assert!(
                matches!(LineBreaks::normalize(text), Cow::Borrowed(_)),
                "{text:?}"
            );
        }
        assert_eq!(LineBreaks::normalize("A\rB\r\nC\r"), "A\nB\nC\n");
    }

    #[test]
    fn ical_parse_error_names_the_offending_line() {
        let err = JsCalendarEntry::parse_all("Content-Type: text/calendar\r\nBEGIN:VCALENDAR\r\n")
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid line"), "{msg}");
        assert!(msg.contains("Content-Type"), "{msg}");
    }

    #[test]
    fn ical_vtodo_classified_as_task() {
        let vtodo = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
            BEGIN:VTODO\r\nUID:t1@example\r\nDTSTAMP:20260101T000000Z\r\n\
            SUMMARY:Buy milk\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let entries = JsCalendarEntry::parse_all(vtodo).expect("parse vtodo");
        assert_eq!(entries[0].data_type, EventDataType::Task);
    }

    #[test]
    fn ical_recurrence_overrides_collapse_to_single_event() {
        let ical = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
            BEGIN:VEVENT\r\nUID:r1@example\r\nDTSTAMP:20260101T000000Z\r\n\
            DTSTART:20260101T090000Z\r\nDTEND:20260101T100000Z\r\n\
            RRULE:FREQ=DAILY;COUNT=3\r\nSUMMARY:Standup\r\nEND:VEVENT\r\n\
            BEGIN:VEVENT\r\nUID:r1@example\r\nDTSTAMP:20260101T000000Z\r\n\
            RECURRENCE-ID:20260102T090000Z\r\nDTSTART:20260102T093000Z\r\nDTEND:20260102T103000Z\r\n\
            SUMMARY:Standup (moved)\r\nEND:VEVENT\r\n\
            END:VCALENDAR\r\n";
        let entries = JsCalendarEntry::parse_all(ical).expect("parse");
        assert_eq!(entries.len(), 1, "calcard merges overrides into master");
        let master = entries[0].data.as_object().unwrap();
        assert!(master.contains_key("recurrenceOverrides"));
    }

    #[test]
    fn strip_extracted_fields_pulls_isdraft_use_default_alerts() {
        let data = serde_json::json!({
            "@type": "Event",
            "uid": "x",
            "title": "T",
            "isDraft": true,
            "useDefaultAlerts": true,
            "calendarIds": {"a": true},
            "utcStart": "2026-01-01T09:00:00Z",
            "method": "PUBLISH"
        });
        let mut entry = JsCalendarEntry {
            data,
            data_type: EventDataType::Event,
        };
        let fields = entry.take_extracted();
        assert!(fields.is_draft);
        assert!(fields.use_default_alerts);
        assert_eq!(fields.uid.as_deref(), Some("x"));
        let m = entry.data.as_object().unwrap();
        assert!(!m.contains_key("calendarIds"));
        assert!(!m.contains_key("isDraft"));
        assert!(!m.contains_key("useDefaultAlerts"));
        assert!(!m.contains_key("method"));
        assert!(!m.contains_key("utcStart"));
    }
}
