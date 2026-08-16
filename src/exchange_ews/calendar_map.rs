/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use serde_json::{Map, Value, json};

use crate::exchange::jscalendar::{
    drop_calendar_address_dependents, is_override_ignored, synthetic_attendee_address,
};
use crate::exchange_ews::parse::{CalendarItemRaw, RawAttendee, RawOccurrence};
use crate::exchange_ews::recurrence::to_jscalendar_rule;
use crate::exchange_ews::tz::resolve_to_iana;

pub struct EventValue {
    pub data: Value,
    pub is_draft: bool,
    pub use_default_alerts: bool,
}

pub fn to_jscalendar(raw: &CalendarItemRaw) -> EventValue {
    to_jscalendar_with_exceptions(raw, &[])
}

pub fn to_jscalendar_with_exceptions(
    raw: &CalendarItemRaw,
    modified_full: &[CalendarItemRaw],
) -> EventValue {
    let iana = raw
        .start_tz
        .as_deref()
        .map(|tz| resolve_to_iana(tz).unwrap_or_else(|| "Etc/UTC".to_owned()));
    let mut event = build_event_map(raw, iana.as_deref());
    if let Some(overrides) = build_recurrence_overrides(&event, raw, modified_full, iana.as_deref())
    {
        event.insert("recurrenceOverrides".to_owned(), overrides);
    }
    EventValue {
        data: Value::Object(event),
        is_draft: false,
        use_default_alerts: false,
    }
}

fn build_event_map(raw: &CalendarItemRaw, iana: Option<&str>) -> Map<String, Value> {
    let mut event = Map::new();
    event.insert("@type".to_owned(), Value::String("Event".to_owned()));
    if let Some(uid) = raw.uid.as_ref() {
        event.insert("uid".to_owned(), Value::String(uid.clone()));
    } else {
        event.insert(
            "uid".to_owned(),
            Value::String(format!(
                "vandelay-ews-event-{}",
                blake3::hash(raw.id.id.as_bytes()).to_hex()
            )),
        );
    }
    if let Some(subject) = raw.subject.as_ref() {
        event.insert("title".to_owned(), Value::String(subject.clone()));
    }
    if let Some(loc) = raw.location.as_ref() {
        event.insert(
            "locations".to_owned(),
            Value::Object(map_singleton(
                "1",
                json!({"@type": "Location", "name": loc}),
            )),
        );
    }
    if let Some(body) = raw.body_text.as_ref() {
        event.insert("description".to_owned(), Value::String(body.clone()));
    } else if let Some(body) = raw.body_html.as_ref() {
        event.insert("description".to_owned(), Value::String(body.clone()));
        event.insert(
            "descriptionContentType".to_owned(),
            Value::String("text/html".to_owned()),
        );
    }
    if let Some(created) = raw.created.as_ref() {
        event.insert(
            "created".to_owned(),
            Value::String(normalise_utc_datetime(created)),
        );
    }
    if let Some(updated) = raw.last_modified.as_ref().or(raw.created.as_ref()) {
        event.insert(
            "updated".to_owned(),
            Value::String(normalise_utc_datetime(updated)),
        );
    }
    if !raw.categories.is_empty() {
        let mut kw = Map::new();
        for c in &raw.categories {
            kw.insert(c.to_ascii_lowercase(), Value::Bool(true));
        }
        event.insert("keywords".to_owned(), Value::Object(kw));
    }
    if let Some(true) = raw.is_all_day_event {
        if let Some(start) = raw.start.as_ref() {
            let date_only = start.split('T').next().unwrap_or(start.as_str());
            event.insert(
                "start".to_owned(),
                Value::String(format!("{date_only}T00:00:00")),
            );
        }
        event.insert("showWithoutTime".to_owned(), Value::Bool(true));
        let days = match (raw.start.as_ref(), raw.end.as_ref()) {
            (Some(s), Some(e)) => all_day_span_days(s, e).max(1),
            _ => 1,
        };
        event.insert("duration".to_owned(), Value::String(format!("P{days}D")));
    } else {
        if let Some(start) = raw.start.as_ref() {
            event.insert(
                "start".to_owned(),
                Value::String(to_local_datetime_in(start, iana)),
            );
        }
        if let (Some(start), Some(end)) = (raw.start.as_ref(), raw.end.as_ref())
            && let Some(dur) = duration_iso8601(start, end)
        {
            event.insert("duration".to_owned(), Value::String(dur));
        }
        if let Some(tz) = iana {
            event.insert("timeZone".to_owned(), Value::String(tz.to_owned()));
        }
    }
    if let Some(status) = raw.legacy_free_busy_status.as_ref() {
        let mapped = match status.as_str() {
            "Free" => "free",
            _ => "busy",
        };
        event.insert(
            "freeBusyStatus".to_owned(),
            Value::String(mapped.to_owned()),
        );
    }
    let mut participants = Map::new();
    let mut next_id = 1;
    if let Some((cal_addr, smtp)) = resolve_calendar_address(
        raw.organizer_smtp.as_deref(),
        raw.organizer_routing_type.as_deref(),
        raw.organizer_name.as_deref(),
    ) {
        let key = next_id.to_string();
        next_id += 1;
        event.insert(
            "organizerCalendarAddress".to_owned(),
            Value::String(cal_addr.clone()),
        );
        let mut p = Map::new();
        p.insert("@type".to_owned(), Value::String("Participant".to_owned()));
        p.insert("calendarAddress".to_owned(), Value::String(cal_addr));
        if let Some(email) = smtp {
            p.insert("email".to_owned(), Value::String(email));
        }
        let mut roles = Map::new();
        roles.insert("owner".to_owned(), Value::Bool(true));
        roles.insert("chair".to_owned(), Value::Bool(true));
        p.insert("roles".to_owned(), Value::Object(roles));
        if let Some(name) = raw.organizer_name.as_ref() {
            p.insert("name".to_owned(), Value::String(name.clone()));
        }
        participants.insert(key, Value::Object(p));
    }
    add_attendees(
        &mut participants,
        &mut next_id,
        &raw.required_attendees,
        AttendeeRole::Required,
    );
    add_attendees(
        &mut participants,
        &mut next_id,
        &raw.optional_attendees,
        AttendeeRole::Optional,
    );
    add_attendees(
        &mut participants,
        &mut next_id,
        &raw.resources,
        AttendeeRole::Resource,
    );
    if !participants.is_empty() {
        if !event.contains_key("organizerCalendarAddress") {
            drop_calendar_address_dependents(&mut participants);
        }
        event.insert("participants".to_owned(), Value::Object(participants));
    }
    if let Some(rec) = raw.recurrence.as_ref()
        && let Some(rule) = to_jscalendar_rule(rec)
    {
        event.insert("recurrenceRule".to_owned(), rule);
    }
    if raw.reminder_is_set == Some(true) {
        let minutes = raw.reminder_minutes_before_start.unwrap_or(0).max(0);
        let offset = if minutes == 0 {
            "PT0S".to_owned()
        } else {
            format!("-PT{minutes}M")
        };
        let alert = json!({
            "@type": "Alert",
            "trigger": {"@type": "OffsetTrigger", "offset": offset, "relativeTo": "start"},
            "action": "display",
        });
        event.insert(
            "alerts".to_owned(),
            Value::Object(map_singleton("1", alert)),
        );
    }
    if let Some(url) = raw
        .join_online_meeting_url
        .as_deref()
        .or(raw.net_show_url.as_deref())
        .or(raw.meeting_workspace_url.as_deref())
        .filter(|s| !s.is_empty())
    {
        let mut vl = Map::new();
        vl.insert(
            "@type".to_owned(),
            Value::String("VirtualLocation".to_owned()),
        );
        vl.insert("uri".to_owned(), Value::String(url.to_owned()));
        if raw.is_online_meeting == Some(true) {
            vl.insert("features".to_owned(), json!({"video": true}));
        }
        event.insert(
            "virtualLocations".to_owned(),
            Value::Object(map_singleton("1", Value::Object(vl))),
        );
    }
    event
}

fn is_smtp_routing(routing_type: Option<&str>) -> bool {
    routing_type.is_none_or(|rt| rt.eq_ignore_ascii_case("SMTP"))
}

fn resolve_calendar_address(
    address: Option<&str>,
    routing_type: Option<&str>,
    name: Option<&str>,
) -> Option<(String, Option<String>)> {
    if let Some(addr) = address.filter(|a| !a.trim().is_empty()) {
        if is_smtp_routing(routing_type) {
            return Some((format!("mailto:{addr}"), Some(addr.to_owned())));
        }
        return Some((synthetic_attendee_address(addr), None));
    }
    name.filter(|n| !n.trim().is_empty())
        .map(|n| (synthetic_attendee_address(n), None))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttendeeRole {
    Required,
    Optional,
    Resource,
}

fn add_attendees(
    out: &mut Map<String, Value>,
    next_id: &mut u32,
    attendees: &[RawAttendee],
    role: AttendeeRole,
) {
    for att in attendees {
        let key = next_id.to_string();
        *next_id += 1;
        let mut p = Map::new();
        p.insert("@type".to_owned(), Value::String("Participant".to_owned()));
        if let Some(name) = att.name.as_ref() {
            p.insert("name".to_owned(), Value::String(name.clone()));
        }
        let calendar_address = resolve_calendar_address(
            att.email.as_deref(),
            att.routing_type.as_deref(),
            att.name.as_deref(),
        );
        if let Some((addr, smtp)) = calendar_address {
            p.insert("calendarAddress".to_owned(), Value::String(addr));
            if let Some(email) = smtp {
                p.insert("email".to_owned(), Value::String(email));
            }
            let mut roles = Map::new();
            match role {
                AttendeeRole::Required | AttendeeRole::Resource => {
                    roles.insert("required".to_owned(), Value::Bool(true));
                }
                AttendeeRole::Optional => {
                    roles.insert("optional".to_owned(), Value::Bool(true));
                }
            }
            p.insert("roles".to_owned(), Value::Object(roles));
            if role == AttendeeRole::Resource {
                p.insert("kind".to_owned(), Value::String("resource".to_owned()));
            } else {
                p.insert("expectReply".to_owned(), Value::Bool(true));
            }
            if let Some(rt) = att.response_type.as_ref() {
                let mapped = match rt.as_str() {
                    "Accept" => "accepted",
                    "Tentative" => "tentative",
                    "Decline" => "declined",
                    "Organizer" => "accepted",
                    "NoResponseReceived" => "needs-action",
                    _ => "needs-action",
                };
                p.insert(
                    "participationStatus".to_owned(),
                    Value::String(mapped.to_owned()),
                );
            }
        }
        out.insert(key, Value::Object(p));
    }
}

const NOT_PATCHED_PER_OCCURRENCE: &[&str] = &["created", "updated"];

fn is_override_excluded(key: &str) -> bool {
    is_override_ignored(key) || NOT_PATCHED_PER_OCCURRENCE.contains(&key)
}

fn build_recurrence_overrides(
    base_event: &Map<String, Value>,
    raw: &CalendarItemRaw,
    modified_full: &[CalendarItemRaw],
    iana: Option<&str>,
) -> Option<Value> {
    if raw.modified_occurrences.is_empty() && raw.deleted_occurrences.is_empty() {
        return None;
    }
    let mut map = Map::new();
    for occ in &raw.modified_occurrences {
        let Some(key) = occ
            .original_start
            .as_deref()
            .or(occ.start.as_deref())
            .map(|s| to_local_datetime_in(s, iana))
        else {
            continue;
        };
        let full = modified_full
            .iter()
            .find(|f| !occ.item_id.id.is_empty() && f.id.id == occ.item_id.id);
        let patch = match full {
            Some(full) => {
                let occ_event = build_event_map(full, iana);
                override_patch(&occ_event, base_event, &key)
            }
            None => time_only_patch(occ, iana),
        };
        map.insert(key, Value::Object(patch));
    }
    for occ in &raw.deleted_occurrences {
        let Some(key) = occ.start.as_deref().map(|s| to_local_datetime_in(s, iana)) else {
            continue;
        };
        map.insert(key, json!({"excluded": true}));
    }
    if map.is_empty() {
        None
    } else {
        Some(Value::Object(map))
    }
}

fn override_patch(
    occ_event: &Map<String, Value>,
    base_event: &Map<String, Value>,
    recurrence_id: &str,
) -> Map<String, Value> {
    let mut patch = Map::new();
    let inherited_start = Value::String(recurrence_id.to_owned());
    for (k, v) in occ_event {
        if is_override_excluded(k) {
            continue;
        }
        let baseline = if k == "start" {
            Some(&inherited_start)
        } else {
            base_event.get(k)
        };
        if baseline != Some(v) {
            patch.insert(k.clone(), v.clone());
        }
    }
    for k in base_event.keys() {
        if k == "start" || is_override_excluded(k) {
            continue;
        }
        if !occ_event.contains_key(k) {
            patch.insert(k.clone(), Value::Null);
        }
    }
    patch
}

fn time_only_patch(occ: &RawOccurrence, iana: Option<&str>) -> Map<String, Value> {
    let mut o = Map::new();
    if let Some(s) = occ.start.as_ref() {
        o.insert(
            "start".to_owned(),
            Value::String(to_local_datetime_in(s, iana)),
        );
    }
    if let (Some(s), Some(e)) = (occ.start.as_ref(), occ.end.as_ref())
        && let Some(dur) = duration_iso8601(s, e)
    {
        o.insert("duration".to_owned(), Value::String(dur));
    }
    o
}

fn normalise_utc_datetime(s: &str) -> String {
    let trimmed = s.trim();
    let stripped = trimmed
        .strip_suffix('Z')
        .or_else(|| {
            trimmed
                .rfind(['+', '-'])
                .filter(|i| *i >= 10)
                .map(|i| &trimmed[..i])
        })
        .unwrap_or(trimmed);
    let (date, time) = match stripped.split_once('T') {
        Some((d, t)) => (d, t),
        None => return format!("{stripped}T00:00:00Z"),
    };
    let time_clean = if let Some(dot) = time.find('.') {
        &time[..dot]
    } else {
        time
    };
    format!("{date}T{time_clean}Z")
}

fn to_local_datetime_in(s: &str, iana: Option<&str>) -> String {
    let trimmed = s.trim();
    let utc_anchored =
        trimmed.ends_with('Z') || trimmed.rfind(['+', '-']).filter(|i| *i >= 10).is_some();
    if utc_anchored
        && let Some(tz) = iana
        && let Some(out) = convert_utc_to_local(trimmed, tz)
    {
        return out;
    }
    let stripped = trimmed
        .strip_suffix('Z')
        .or_else(|| {
            trimmed
                .rfind(['+', '-'])
                .filter(|i| *i >= 10)
                .map(|i| &trimmed[..i])
        })
        .unwrap_or(trimmed);
    let (date, time) = match stripped.split_once('T') {
        Some((d, t)) => (d, t),
        None => return format!("{stripped}T00:00:00"),
    };
    let time_clean = if let Some(dot) = time.find('.') {
        &time[..dot]
    } else {
        time
    };
    format!("{date}T{time_clean}")
}

fn convert_utc_to_local(s: &str, iana: &str) -> Option<String> {
    use chrono::DateTime;
    let utc: DateTime<chrono::Utc> = DateTime::parse_from_rfc3339(s)
        .ok()?
        .with_timezone(&chrono::Utc);
    let tz: chrono_tz::Tz = iana.parse().ok()?;
    let local = utc.with_timezone(&tz);
    Some(local.format("%Y-%m-%dT%H:%M:%S").to_string())
}

fn all_day_span_days(start: &str, end: &str) -> u32 {
    fn date_only(s: &str) -> &str {
        s.split('T').next().unwrap_or(s)
    }
    fn ymd(s: &str) -> Option<(i64, u32, u32)> {
        let mut p = s.split('-');
        let y: i64 = p.next()?.parse().ok()?;
        let m: u32 = p.next()?.parse().ok()?;
        let d: u32 = p.next()?.parse().ok()?;
        Some((y, m, d))
    }
    fn to_days(y: i64, m: u32, d: u32) -> i64 {
        let (mut y, m) = if m <= 2 {
            (y - 1, m as i64 + 12)
        } else {
            (y, m as i64)
        };
        y += 4800;
        let mm = m + 1;
        365 * y + y / 4 - y / 100 + y / 400 + 30 * mm + 3 * (mm + 1) / 5 + d as i64 - 32045
    }
    let (Some(s), Some(e)) = (ymd(date_only(start)), ymd(date_only(end))) else {
        return 1;
    };
    let diff = to_days(e.0, e.1, e.2) - to_days(s.0, s.1, s.2);
    if diff <= 0 { 1 } else { diff as u32 }
}

fn duration_iso8601(start: &str, end: &str) -> Option<String> {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;
    let s = OffsetDateTime::parse(start, &Rfc3339).ok()?;
    let e = OffsetDateTime::parse(end, &Rfc3339).ok()?;
    let dur = e - s;
    if dur.is_zero() {
        return Some("PT0S".to_owned());
    }
    let total_seconds = dur.whole_seconds();
    if total_seconds < 0 {
        return None;
    }
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    let mut out = String::from("PT");
    if hours > 0 {
        out.push_str(&format!("{hours}H"));
    }
    if minutes > 0 {
        out.push_str(&format!("{minutes}M"));
    }
    if seconds > 0 || (hours == 0 && minutes == 0) {
        out.push_str(&format!("{seconds}S"));
    }
    Some(out)
}

fn map_singleton(key: &str, value: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(key.to_owned(), value);
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange_ews::parse::{RecurrencePattern, RecurrenceRange};

    #[test]
    fn simple_event_round_trip_converts_utc_to_local_wall_clock() {
        let raw = CalendarItemRaw {
            uid: Some("uid-1".to_owned()),
            subject: Some("Meeting".to_owned()),
            start: Some("2025-06-15T14:00:00Z".to_owned()),
            end: Some("2025-06-15T15:00:00Z".to_owned()),
            location: Some("HQ".to_owned()),
            start_tz: Some("Pacific Standard Time".to_owned()),
            ..CalendarItemRaw::default()
        };
        let v = to_jscalendar(&raw).data;
        assert_eq!(v["@type"], "Event");
        assert_eq!(v["uid"], "uid-1");
        assert_eq!(v["title"], "Meeting");
        assert_eq!(v["start"], "2025-06-15T07:00:00");
        assert_eq!(v["duration"], "PT1H");
        assert_eq!(v["timeZone"], "America/Los_Angeles");
        assert_eq!(v["locations"]["1"]["name"], "HQ");
    }

    #[test]
    fn naive_start_without_timezone_stays_floating() {
        let raw = CalendarItemRaw {
            uid: Some("uid-naive".to_owned()),
            start: Some("2025-06-15T14:00:00".to_owned()),
            end: Some("2025-06-15T15:00:00".to_owned()),
            ..CalendarItemRaw::default()
        };
        let v = to_jscalendar(&raw).data;
        assert_eq!(v["start"], "2025-06-15T14:00:00");
        assert!(v.get("timeZone").is_none());
    }

    #[test]
    fn unknown_windows_timezone_falls_back_to_utc() {
        let raw = CalendarItemRaw {
            uid: Some("uid-x".to_owned()),
            start: Some("2025-06-15T14:00:00Z".to_owned()),
            end: Some("2025-06-15T15:00:00Z".to_owned()),
            start_tz: Some("Made Up Time".to_owned()),
            ..CalendarItemRaw::default()
        };
        let v = to_jscalendar(&raw).data;
        assert_eq!(v["start"], "2025-06-15T14:00:00");
        assert_eq!(v["timeZone"], "Etc/UTC");
    }

    #[test]
    fn recurrence_override_key_uses_local_time_in_event_timezone() {
        let raw = CalendarItemRaw {
            uid: Some("uid-tz".to_owned()),
            start: Some("2025-06-15T21:00:00Z".to_owned()),
            end: Some("2025-06-15T22:00:00Z".to_owned()),
            start_tz: Some("Pacific Standard Time".to_owned()),
            recurrence: Some(crate::exchange_ews::parse::RawRecurrence {
                pattern: Some(RecurrencePattern::Daily { interval: 1 }),
                range: Some(RecurrenceRange::Numbered {
                    start_date: "2025-06-15".to_owned(),
                    number_of_occurrences: 3,
                }),
            }),
            deleted_occurrences: vec![RawOccurrence {
                item_id: Default::default(),
                start: Some("2025-06-16T21:00:00Z".to_owned()),
                end: None,
                original_start: None,
            }],
            ..CalendarItemRaw::default()
        };
        let v = to_jscalendar(&raw).data;
        let over = v["recurrenceOverrides"].as_object().unwrap();
        assert!(over.contains_key("2025-06-16T14:00:00"));
    }

    #[test]
    fn all_day_event_uses_date_only_start() {
        let raw = CalendarItemRaw {
            uid: Some("uid-2".to_owned()),
            subject: Some("Holiday".to_owned()),
            is_all_day_event: Some(true),
            start: Some("2025-12-25T00:00:00".to_owned()),
            end: Some("2025-12-26T00:00:00".to_owned()),
            ..CalendarItemRaw::default()
        };
        let v = to_jscalendar(&raw).data;
        assert_eq!(v["start"], "2025-12-25T00:00:00");
        assert_eq!(v["showWithoutTime"], true);
        assert_eq!(v["duration"], "P1D");
        assert!(v.get("timeZone").is_none());
    }

    #[test]
    fn recurring_master_with_overrides() {
        let raw = CalendarItemRaw {
            uid: Some("uid-3".to_owned()),
            start: Some("2025-06-15T14:00:00Z".to_owned()),
            end: Some("2025-06-15T15:00:00Z".to_owned()),
            recurrence: Some(crate::exchange_ews::parse::RawRecurrence {
                pattern: Some(RecurrencePattern::Daily { interval: 1 }),
                range: Some(RecurrenceRange::Numbered {
                    start_date: "2025-06-15".to_owned(),
                    number_of_occurrences: 3,
                }),
            }),
            modified_occurrences: vec![RawOccurrence {
                item_id: Default::default(),
                start: Some("2025-06-16T15:00:00Z".to_owned()),
                end: Some("2025-06-16T16:30:00Z".to_owned()),
                original_start: Some("2025-06-16T14:00:00Z".to_owned()),
            }],
            deleted_occurrences: vec![RawOccurrence {
                item_id: Default::default(),
                start: Some("2025-06-17T14:00:00Z".to_owned()),
                end: None,
                original_start: None,
            }],
            ..CalendarItemRaw::default()
        };
        let v = to_jscalendar(&raw).data;
        assert!(v.get("recurrenceRules").is_none());
        assert_eq!(v["recurrenceRule"]["frequency"], "daily");
        assert_eq!(v["recurrenceRule"]["count"], 3);
        let over = v["recurrenceOverrides"].as_object().unwrap();
        assert!(over.contains_key("2025-06-16T14:00:00"));
        assert_eq!(over["2025-06-17T14:00:00"]["excluded"], true);
    }

    #[test]
    fn updated_backfills_from_created_when_last_modified_absent() {
        let raw = CalendarItemRaw {
            uid: Some("u".to_owned()),
            start: Some("2025-06-15T14:00:00Z".to_owned()),
            end: Some("2025-06-15T15:00:00Z".to_owned()),
            created: Some("2025-06-01T09:00:00Z".to_owned()),
            last_modified: None,
            ..CalendarItemRaw::default()
        };
        let v = to_jscalendar(&raw).data;
        assert_eq!(
            v["updated"], "2025-06-01T09:00:00Z",
            "updated is mandatory (jscalendarbis 3.1.6); backfill from created"
        );
    }

    #[test]
    fn address_less_attendee_keeps_rsvp_via_synthetic_non_mailto_calendar_address() {
        let raw = CalendarItemRaw {
            uid: Some("u".to_owned()),
            start: Some("2025-06-15T14:00:00Z".to_owned()),
            end: Some("2025-06-15T15:00:00Z".to_owned()),
            organizer_smtp: Some("alice@example.com".to_owned()),
            required_attendees: vec![crate::exchange_ews::parse::RawAttendee {
                email: None,
                routing_type: None,
                name: Some("No Address Person".to_owned()),
                response_type: Some("Accept".to_owned()),
            }],
            ..CalendarItemRaw::default()
        };
        let v = to_jscalendar(&raw).data;
        let p = v["participants"]
            .as_object()
            .unwrap()
            .values()
            .find(|p| p["name"] == "No Address Person")
            .unwrap();
        let addr = p["calendarAddress"].as_str().unwrap();
        assert!(
            addr.starts_with("urn:x-vandelay:attendee:"),
            "a name-only attendee gets a stable synthetic calendarAddress, got {addr}"
        );
        assert!(
            !addr.starts_with("mailto:"),
            "synthetic address MUST NOT be a mailto: (export must not invite a fabricated address)"
        );
        assert_eq!(
            p["participationStatus"], "accepted",
            "the RSVP is preserved now that a calendarAddress is present (jscalendarbis 3.4.6)"
        );
        assert!(p.get("email").is_none(), "no real email is invented");
        assert_eq!(p["roles"]["required"], true);
    }

    #[test]
    fn synthetic_attendee_address_is_stable_per_name() {
        assert_eq!(
            synthetic_attendee_address("Jane Doe"),
            synthetic_attendee_address("Jane Doe")
        );
        assert_ne!(
            synthetic_attendee_address("Jane Doe"),
            synthetic_attendee_address("John Doe")
        );
    }

    #[test]
    fn legacy_ex_routing_type_does_not_produce_a_mailto() {
        let dn = "/o=ExchangeLabs/ou=Exchange Administrative Group/cn=Recipients/cn=abc123";
        let raw = CalendarItemRaw {
            uid: Some("u".to_owned()),
            start: Some("2025-06-15T14:00:00Z".to_owned()),
            end: Some("2025-06-15T15:00:00Z".to_owned()),
            organizer_smtp: Some(dn.to_owned()),
            organizer_routing_type: Some("EX".to_owned()),
            organizer_name: Some("Legacy Organizer".to_owned()),
            required_attendees: vec![RawAttendee {
                email: Some(dn.to_owned()),
                routing_type: Some("EX".to_owned()),
                name: Some("Legacy Attendee".to_owned()),
                response_type: Some("Accept".to_owned()),
            }],
            ..CalendarItemRaw::default()
        };
        let v = to_jscalendar(&raw).data;
        let org_addr = v["organizerCalendarAddress"].as_str().unwrap();
        assert!(
            org_addr.starts_with("urn:x-vandelay:attendee:") && !org_addr.contains("mailto:"),
            "an EX organizer must not become mailto:/o=.../cn=...; got {org_addr}"
        );
        let att = v["participants"]
            .as_object()
            .unwrap()
            .values()
            .find(|p| p["name"] == "Legacy Attendee")
            .unwrap();
        let att_addr = att["calendarAddress"].as_str().unwrap();
        assert!(!att_addr.starts_with("mailto:"), "got {att_addr}");
        assert!(att.get("email").is_none(), "an X500 DN is not an email");
        assert_eq!(att["participationStatus"], "accepted");
    }

    #[test]
    fn smtp_routing_type_still_yields_mailto() {
        let raw = CalendarItemRaw {
            uid: Some("u".to_owned()),
            start: Some("2025-06-15T14:00:00Z".to_owned()),
            end: Some("2025-06-15T15:00:00Z".to_owned()),
            organizer_smtp: Some("alice@example.com".to_owned()),
            required_attendees: vec![RawAttendee {
                email: Some("bob@example.com".to_owned()),
                routing_type: Some("SMTP".to_owned()),
                name: None,
                response_type: Some("Accept".to_owned()),
            }],
            ..CalendarItemRaw::default()
        };
        let v = to_jscalendar(&raw).data;
        let att = v["participants"]
            .as_object()
            .unwrap()
            .values()
            .find(|p| p["email"] == "bob@example.com")
            .unwrap();
        assert_eq!(att["calendarAddress"], "mailto:bob@example.com");
        assert_eq!(att["email"], "bob@example.com");
    }

    #[test]
    fn modified_occurrence_override_captures_title_and_location_not_just_time() {
        let master = CalendarItemRaw {
            uid: Some("uid-series".to_owned()),
            subject: Some("Standup".to_owned()),
            start: Some("2025-06-16T14:00:00Z".to_owned()),
            end: Some("2025-06-16T14:30:00Z".to_owned()),
            recurrence: Some(crate::exchange_ews::parse::RawRecurrence {
                pattern: Some(RecurrencePattern::Daily { interval: 1 }),
                range: Some(RecurrenceRange::Numbered {
                    start_date: "2025-06-16".to_owned(),
                    number_of_occurrences: 5,
                }),
            }),
            modified_occurrences: vec![RawOccurrence {
                item_id: crate::exchange_ews::types::ItemId::new("EXC1", ""),
                start: Some("2025-06-18T14:00:00Z".to_owned()),
                end: Some("2025-06-18T14:30:00Z".to_owned()),
                original_start: Some("2025-06-18T14:00:00Z".to_owned()),
            }],
            ..CalendarItemRaw::default()
        };
        let full_occ = CalendarItemRaw {
            id: crate::exchange_ews::types::ItemId::new("EXC1", ""),
            uid: Some("uid-series".to_owned()),
            subject: Some("Sprint Retro".to_owned()),
            location: Some("Big Room".to_owned()),
            start: Some("2025-06-18T14:00:00Z".to_owned()),
            end: Some("2025-06-18T14:30:00Z".to_owned()),
            ..CalendarItemRaw::default()
        };
        let v = to_jscalendar_with_exceptions(&master, std::slice::from_ref(&full_occ)).data;
        let ov = &v["recurrenceOverrides"]["2025-06-18T14:00:00"];
        assert_eq!(
            ov["title"], "Sprint Retro",
            "changed subject must be in the override"
        );
        assert_eq!(ov["locations"]["1"]["name"], "Big Room");
        assert!(
            ov.get("start").is_none(),
            "an unchanged occurrence time must not emit a redundant start patch"
        );
        assert!(
            ov.get("uid").is_none() && ov.get("recurrenceRule").is_none(),
            "ignored pointers must never appear in a PatchObject"
        );
    }

    #[test]
    fn multi_day_all_day_spans_correct_number_of_days() {
        let raw = CalendarItemRaw {
            uid: Some("uid-multi".to_owned()),
            is_all_day_event: Some(true),
            start: Some("2025-07-04T00:00:00".to_owned()),
            end: Some("2025-07-07T00:00:00".to_owned()),
            ..CalendarItemRaw::default()
        };
        let v = to_jscalendar(&raw).data;
        assert_eq!(v["duration"], "P3D");
        assert_eq!(v["showWithoutTime"], true);
    }

    #[test]
    fn attendees_get_required_or_optional_role() {
        let raw = CalendarItemRaw {
            uid: Some("uid-att".to_owned()),
            start: Some("2025-06-15T14:00:00Z".to_owned()),
            end: Some("2025-06-15T15:00:00Z".to_owned()),
            organizer_smtp: Some("alice@x".to_owned()),
            required_attendees: vec![crate::exchange_ews::parse::RawAttendee {
                email: Some("bob@x".to_owned()),
                routing_type: None,
                name: None,
                response_type: Some("Accept".to_owned()),
            }],
            optional_attendees: vec![crate::exchange_ews::parse::RawAttendee {
                email: Some("eve@x".to_owned()),
                routing_type: None,
                name: None,
                response_type: None,
            }],
            ..CalendarItemRaw::default()
        };
        let v = to_jscalendar(&raw).data;
        assert_eq!(v["organizerCalendarAddress"], "mailto:alice@x");
        let participants = v["participants"].as_object().unwrap();
        let required = participants
            .values()
            .find(|p| p["email"] == "bob@x")
            .unwrap();
        assert_eq!(required["roles"]["required"], true);
        assert!(required["roles"].get("optional").is_none());
        let optional = participants
            .values()
            .find(|p| p["email"] == "eve@x")
            .unwrap();
        assert_eq!(optional["roles"]["optional"], true);
    }

    #[test]
    fn reminder_becomes_offset_trigger_alert() {
        let raw = CalendarItemRaw {
            uid: Some("uid-alarm".to_owned()),
            start: Some("2025-06-15T14:00:00Z".to_owned()),
            end: Some("2025-06-15T15:00:00Z".to_owned()),
            reminder_is_set: Some(true),
            reminder_minutes_before_start: Some(15),
            ..CalendarItemRaw::default()
        };
        let v = to_jscalendar(&raw).data;
        let alert = &v["alerts"]["1"];
        assert_eq!(alert["@type"], "Alert");
        assert_eq!(alert["action"], "display");
        assert_eq!(alert["trigger"]["@type"], "OffsetTrigger");
        assert_eq!(alert["trigger"]["offset"], "-PT15M");
        assert_eq!(alert["trigger"]["relativeTo"], "start");
    }

    #[test]
    fn reminder_not_set_emits_no_alerts() {
        let raw = CalendarItemRaw {
            uid: Some("uid-noalarm".to_owned()),
            start: Some("2025-06-15T14:00:00Z".to_owned()),
            end: Some("2025-06-15T15:00:00Z".to_owned()),
            reminder_is_set: Some(false),
            reminder_minutes_before_start: Some(15),
            ..CalendarItemRaw::default()
        };
        let v = to_jscalendar(&raw).data;
        assert!(v.get("alerts").is_none());
    }

    #[test]
    fn online_meeting_becomes_virtual_location() {
        let raw = CalendarItemRaw {
            uid: Some("uid-online".to_owned()),
            start: Some("2025-06-15T14:00:00Z".to_owned()),
            end: Some("2025-06-15T15:00:00Z".to_owned()),
            is_online_meeting: Some(true),
            net_show_url: Some("https://teams.example/join/abc".to_owned()),
            ..CalendarItemRaw::default()
        };
        let v = to_jscalendar(&raw).data;
        let vl = &v["virtualLocations"]["1"];
        assert_eq!(vl["@type"], "VirtualLocation");
        assert_eq!(vl["uri"], "https://teams.example/join/abc");
        assert_eq!(vl["features"]["video"], true);
    }

    #[test]
    fn join_url_preferred_over_workspace_url() {
        let raw = CalendarItemRaw {
            uid: Some("uid-join".to_owned()),
            start: Some("2025-06-15T14:00:00Z".to_owned()),
            end: Some("2025-06-15T15:00:00Z".to_owned()),
            join_online_meeting_url: Some("https://join/primary".to_owned()),
            meeting_workspace_url: Some("https://workspace/secondary".to_owned()),
            ..CalendarItemRaw::default()
        };
        let v = to_jscalendar(&raw).data;
        assert_eq!(v["virtualLocations"]["1"]["uri"], "https://join/primary");
    }

    #[test]
    fn resources_get_resource_kind_and_no_expect_reply() {
        let raw = CalendarItemRaw {
            uid: Some("uid-res".to_owned()),
            start: Some("2025-06-15T14:00:00Z".to_owned()),
            end: Some("2025-06-15T15:00:00Z".to_owned()),
            organizer_smtp: Some("alice@example.com".to_owned()),
            resources: vec![crate::exchange_ews::parse::RawAttendee {
                email: Some("room-7@x".to_owned()),
                routing_type: None,
                name: Some("Room 7".to_owned()),
                response_type: Some("Accept".to_owned()),
            }],
            ..CalendarItemRaw::default()
        };
        let v = to_jscalendar(&raw).data;
        let res = v["participants"]
            .as_object()
            .unwrap()
            .values()
            .find(|p| p["email"] == "room-7@x")
            .unwrap();
        assert_eq!(res["kind"], "resource");
        assert_eq!(res["roles"]["required"], true);
        assert_eq!(res["participationStatus"], "accepted");
        assert!(res.get("expectReply").is_none());
    }

    #[test]
    fn attendees_carry_expect_reply_and_participation_status() {
        let raw = CalendarItemRaw {
            uid: Some("uid-rsvp".to_owned()),
            start: Some("2025-06-15T14:00:00Z".to_owned()),
            end: Some("2025-06-15T15:00:00Z".to_owned()),
            organizer_smtp: Some("alice@example.com".to_owned()),
            required_attendees: vec![crate::exchange_ews::parse::RawAttendee {
                email: Some("bob@x".to_owned()),
                routing_type: None,
                name: None,
                response_type: Some("Tentative".to_owned()),
            }],
            ..CalendarItemRaw::default()
        };
        let v = to_jscalendar(&raw).data;
        let bob = v["participants"]
            .as_object()
            .unwrap()
            .values()
            .find(|p| p["email"] == "bob@x")
            .unwrap();
        assert_eq!(bob["expectReply"], true);
        assert_eq!(bob["participationStatus"], "tentative");
        assert_eq!(bob["roles"]["required"], true);
    }

    #[test]
    fn attendees_lose_calendar_addresses_when_no_organizer_is_known() {
        let raw = CalendarItemRaw {
            uid: Some("uid-no-organizer".to_owned()),
            start: Some("2025-06-15T14:00:00Z".to_owned()),
            end: Some("2025-06-15T15:00:00Z".to_owned()),
            required_attendees: vec![crate::exchange_ews::parse::RawAttendee {
                email: Some("bob@x".to_owned()),
                routing_type: None,
                name: Some("Bob".to_owned()),
                response_type: Some("Accept".to_owned()),
            }],
            ..CalendarItemRaw::default()
        };
        let v = to_jscalendar(&raw).data;
        assert!(v.get("organizerCalendarAddress").is_none());
        let bob = v["participants"]
            .as_object()
            .unwrap()
            .values()
            .find(|p| p["name"] == "Bob")
            .unwrap();
        for key in ["calendarAddress", "email", "roles", "participationStatus"] {
            assert!(
                bob.get(key).is_none(),
                "{key} requires calendarAddress, which requires organizerCalendarAddress"
            );
        }
    }

    #[test]
    fn all_day_span_helper_is_inclusive_of_start() {
        assert_eq!(all_day_span_days("2025-07-04", "2025-07-04"), 1);
        assert_eq!(all_day_span_days("2025-07-04", "2025-07-05"), 1);
        assert_eq!(all_day_span_days("2025-07-04", "2025-07-07"), 3);
        assert_eq!(all_day_span_days("bad", "input"), 1);
    }
}
