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

pub fn vcard_to_jscontact(text: &str, item_href: &str) -> Result<JsContact, CalcardError> {
    let vcard = VCard::parse(normalize_line_breaks(text))
        .map_err(|e| CalcardError::VCardParse(describe_parse_error(e)))?;
    let js = vcard.into_jscontact::<String, String>();
    let value: Value = serde_json::from_str(&js.to_string_pretty())?;
    let uid = value
        .get("uid")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| synthesise_uid(item_href));
    let mut obj = match value {
        Value::Object(map) => map,
        _ => return Err(CalcardError::NotAnObject),
    };
    obj.remove("uid");
    Ok(JsContact {
        uid,
        data: Value::Object(obj),
    })
}

pub fn synthesise_uid(item_href: &str) -> String {
    let hash = blake3::hash(item_href.as_bytes());
    format!("vandelay-syn-{}", hash.to_hex())
}

#[derive(Debug)]
pub struct JsCalendarEntry {
    pub data: Value,
    pub data_type: EntryType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryType {
    Event,
    Task,
    Note,
    Group,
}

impl EntryType {
    pub fn as_column(self) -> &'static str {
        match self {
            EntryType::Event => "Event",
            EntryType::Task => "Task",
            EntryType::Note => "Note",
            EntryType::Group => "Group",
        }
    }
}

pub fn ical_to_jscalendar_entries(text: &str) -> Result<Vec<JsCalendarEntry>, CalcardError> {
    let ical = ICalendar::parse(normalize_line_breaks(text))
        .map_err(|e| CalcardError::ICalParse(describe_parse_error(e)))?;
    let js = ical.into_jscalendar::<String, String>();
    let value: Value = serde_json::from_str(&js.to_string_pretty())?;
    let mut out = Vec::new();
    collect_entries(value, &mut out);
    if out.is_empty() {
        return Err(CalcardError::NoEntries);
    }
    Ok(out)
}

fn normalize_line_breaks(text: &str) -> Cow<'_, str> {
    let mut after_cr = text.split('\r').skip(1).peekable();
    let mut has_bare_cr = false;
    while let Some(rest) = after_cr.next() {
        if is_bare_cr(rest, after_cr.peek().is_some()) {
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
        if is_bare_cr(rest, parts.peek().is_some()) {
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

fn describe_parse_error(entry: Entry) -> String {
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

fn collect_entries(value: Value, out: &mut Vec<JsCalendarEntry>) {
    match value {
        Value::Object(map) => {
            let at_type = map.get("@type").and_then(Value::as_str).map(str::to_owned);
            match at_type.as_deref() {
                Some("Event") if has_nonempty_uid(&map) => out.push(JsCalendarEntry {
                    data: Value::Object(map),
                    data_type: EntryType::Event,
                }),
                Some("Task") if has_nonempty_uid(&map) => out.push(JsCalendarEntry {
                    data: Value::Object(map),
                    data_type: EntryType::Task,
                }),
                Some("Note") if has_nonempty_uid(&map) => out.push(JsCalendarEntry {
                    data: Value::Object(map),
                    data_type: EntryType::Note,
                }),
                Some("Group") => {
                    if let Some(entries) = map.get("entries").and_then(Value::as_array) {
                        for entry in entries {
                            collect_entries(entry.clone(), out);
                        }
                    }
                }
                _ => {}
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_entries(item, out);
            }
        }
        _ => {}
    }
}

fn has_nonempty_uid(map: &Map<String, Value>) -> bool {
    map.get("uid")
        .and_then(Value::as_str)
        .map(|u| !u.trim().is_empty())
        .unwrap_or(false)
}

pub fn strip_extracted_fields_from_event(value: &mut Value) -> (bool, bool, Option<String>) {
    let Value::Object(map) = value else {
        return (false, false, None);
    };
    let is_draft = map
        .remove("isDraft")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let use_default_alerts = map
        .remove("useDefaultAlerts")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    map.remove("calendarIds");
    map.remove("method");
    map.remove("utcStart");
    map.remove("utcEnd");
    map.remove("isOrigin");
    map.remove("baseEventId");
    let uid = map.get("uid").and_then(Value::as_str).map(str::to_owned);
    (is_draft, use_default_alerts, uid)
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
        let jc = vcard_to_jscontact(SAMPLE_VCARD, "/dav/card/u/d/a.vcf").expect("parse");
        assert_eq!(jc.uid, "abc-123");
        let obj = jc.data.as_object().unwrap();
        assert!(!obj.contains_key("uid"), "uid stripped from data");
    }

    #[test]
    fn ical_extracts_at_least_one_event() {
        let events = ical_to_jscalendar_entries(SAMPLE_ICAL).expect("parse");
        assert!(!events.is_empty());
        assert_eq!(events[0].data_type, EntryType::Event);
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
        let jc = vcard_to_jscontact(no_uid, href).expect("synthetic uid");
        assert_eq!(jc.uid, synthesise_uid(href));
        assert!(jc.uid.starts_with("vandelay-syn-"));
    }

    #[test]
    fn synthetic_uid_is_stable_per_href() {
        assert_eq!(synthesise_uid("/a"), synthesise_uid("/a"));
        assert_ne!(synthesise_uid("/a"), synthesise_uid("/b"));
    }

    #[test]
    fn ical_with_empty_calendar_fails_no_entries() {
        let empty = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nEND:VCALENDAR\r\n";
        let err = ical_to_jscalendar_entries(empty).unwrap_err();
        assert!(matches!(err, CalcardError::NoEntries));
    }

    #[test]
    fn vcard_garbage_fails_parse() {
        let err = vcard_to_jscontact("not a vcard", "/x").unwrap_err();
        assert!(matches!(err, CalcardError::VCardParse(_)));
    }

    // Kerio Connect sends calendar-data with bare CR line breaks (issue #48).
    #[test]
    fn ical_with_bare_cr_line_breaks_parses() {
        let ical = SAMPLE_ICAL.replace("\r\n", "\r");
        let entries = ical_to_jscalendar_entries(&ical).expect("bare CR parses");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].data["title"], "Test Event");
    }

    // A folded line split by a bare CR still unfolds.
    #[test]
    fn ical_with_bare_cr_unfolds_continuation_lines() {
        let ical = "BEGIN:VCALENDAR\rVERSION:2.0\rBEGIN:VEVENT\rUID:k1@example\r\
            DTSTAMP:20260101T000000Z\rDTSTART:20260101T090000Z\r\
            SUMMARY:Quarterly\r  review\rEND:VEVENT\rEND:VCALENDAR\r";
        let entries = ical_to_jscalendar_entries(ical).expect("parse");
        assert_eq!(entries[0].data["title"], "Quarterly review");
    }

    #[test]
    fn vcard_with_bare_cr_line_breaks_parses() {
        let vcard = SAMPLE_VCARD.replace("\r\n", "\r");
        let jc = vcard_to_jscontact(&vcard, "/x").expect("bare CR parses");
        assert_eq!(jc.uid, "abc-123");
    }

    #[test]
    fn line_break_normalization_borrows_unless_bare_cr() {
        for text in ["A\nB\n", "A\r\nB\r\n", "A\r\r\nB\r\r\n", "no breaks"] {
            assert!(
                matches!(normalize_line_breaks(text), Cow::Borrowed(_)),
                "{text:?}"
            );
        }
        assert_eq!(normalize_line_breaks("A\rB\r\nC\r"), "A\nB\nC\n");
    }

    #[test]
    fn ical_parse_error_names_the_offending_line() {
        let err = ical_to_jscalendar_entries("Content-Type: text/calendar\r\nBEGIN:VCALENDAR\r\n")
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
        let entries = ical_to_jscalendar_entries(vtodo).expect("parse vtodo");
        assert_eq!(entries[0].data_type, EntryType::Task);
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
        let entries = ical_to_jscalendar_entries(ical).expect("parse");
        assert_eq!(entries.len(), 1, "calcard merges overrides into master");
        let master = entries[0].data.as_object().unwrap();
        assert!(master.contains_key("recurrenceOverrides"));
    }

    #[test]
    fn strip_extracted_fields_pulls_isdraft_use_default_alerts() {
        let mut v = serde_json::json!({
            "@type": "Event",
            "uid": "x",
            "title": "T",
            "isDraft": true,
            "useDefaultAlerts": true,
            "calendarIds": {"a": true},
            "utcStart": "2026-01-01T09:00:00Z",
            "method": "PUBLISH"
        });
        let (d, u, uid) = strip_extracted_fields_from_event(&mut v);
        assert!(d);
        assert!(u);
        assert_eq!(uid.as_deref(), Some("x"));
        let m = v.as_object().unwrap();
        assert!(!m.contains_key("calendarIds"));
        assert!(!m.contains_key("isDraft"));
        assert!(!m.contains_key("useDefaultAlerts"));
        assert!(!m.contains_key("method"));
        assert!(!m.contains_key("utcStart"));
    }
}
