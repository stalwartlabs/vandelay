/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use chrono::{NaiveDate, NaiveDateTime, TimeZone};
use chrono_tz::Tz;
use serde_json::{Map, Value, json};

use crate::exchange::date::WireDate;
use crate::exchange::graph::error::GraphError;
use crate::exchange::graph::recurrence::GraphRecurrence;
use crate::exchange::jscalendar::{ParticipantMap, ResolvedAddress, Routing};
use crate::exchange::tz::ZoneId;
use crate::exchange::vocab::{DurationStyle, IsoDuration, Participation, Privacy};

const UTC_ZONE: &str = "Etc/UTC";
const LOCAL_FORMAT: &str = "%Y-%m-%dT%H:%M:%S";
const MIDNIGHT: &str = "T00:00:00";
const DEFAULT_START_TIME: &str = "00:00:00";
const SECONDS_PER_DAY: i64 = 86_400;
const SECONDS_PER_HOUR: i64 = 3_600;
const SECONDS_PER_MINUTE: i64 = 60;
const DEFAULT_PRIORITY: i64 = 5;
const ADDRESS_FIELDS: [&str; 5] = ["street", "city", "state", "postalCode", "countryOrRegion"];

type Hex = &'static str;
type Role = &'static str;

#[derive(Debug, Clone, Copy)]
pub struct CalendarColor<'a>(pub &'a str);

impl CalendarColor<'_> {
    pub fn hex(self) -> Option<&'static str> {
        hashify::map!(
            self.0.as_bytes(),
            Hex,
            "lightBlue" => "#A6CEE3",
            "lightGreen" => "#B2DF8A",
            "lightOrange" => "#FDBF6F",
            "lightGray" => "#CCCCCC",
            "lightYellow" => "#FFFF99",
            "lightTeal" => "#A0E7E5",
            "lightPink" => "#FB9A99",
            "lightBrown" => "#B15928",
            "lightRed" => "#FB9A99",
        )
        .copied()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct GraphEvent<'a>(pub &'a Value);

impl GraphEvent<'_> {
    pub fn event_type(self) -> EventType {
        self.0
            .get("type")
            .and_then(Value::as_str)
            .and_then(|t| {
                hashify::map!(
                    t.as_bytes(),
                    EventType,
                    "seriesMaster" => EventType::SeriesMaster,
                    "occurrence" => EventType::Occurrence,
                    "exception" => EventType::Exception,
                )
            })
            .copied()
            .unwrap_or(EventType::SingleInstance)
    }

    pub fn starts_on_outlook_no_date(self) -> bool {
        self.0
            .get("start")
            .and_then(|s| s.get("dateTime"))
            .and_then(Value::as_str)
            .is_some_and(|s| WireDate(s).is_outlook_no_date())
    }

    pub fn convert(self, fallback_calendar_tz: Option<&str>) -> Result<ConvertedEvent, GraphError> {
        let graph_event = self.0;
        let uid = graph_event
            .get("iCalUId")
            .and_then(Value::as_str)
            .or_else(|| graph_event.get("id").and_then(Value::as_str))
            .ok_or_else(|| GraphError::Malformed("event has neither iCalUId nor id".to_owned()))?
            .to_owned();

        let event_type = self.event_type();
        let series_master_id = graph_event
            .get("seriesMasterId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let original_start = graph_event
            .get("originalStart")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let is_draft = graph_event
            .get("isDraft")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let mut card = Map::new();
        card.insert("@type".to_owned(), Value::from("Event"));
        card.insert("uid".to_owned(), Value::from(uid.clone()));

        if let Some(subject) = graph_event.get("subject").and_then(Value::as_str)
            && !subject.is_empty()
        {
            card.insert("title".to_owned(), Value::from(subject.to_owned()));
        }

        if let Some(body) = graph_event.get("body").and_then(Value::as_object)
            && let Some(content) = body.get("content").and_then(Value::as_str)
            && !content.is_empty()
        {
            card.insert("description".to_owned(), Value::from(content.to_owned()));
        }

        let start_dt = GraphEvent::local_datetime(graph_event.get("start"));
        let end_dt = GraphEvent::local_datetime(graph_event.get("end"));

        let is_all_day = graph_event
            .get("isAllDay")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let display_tz = graph_event
            .get("originalStartTimeZone")
            .and_then(Value::as_str)
            .and_then(|z| ZoneId(z).iana())
            .or_else(|| {
                graph_event
                    .get("start")
                    .and_then(|s| s.get("timeZone"))
                    .and_then(Value::as_str)
                    .and_then(|z| ZoneId(z).iana())
            })
            .or_else(|| fallback_calendar_tz.and_then(|z| ZoneId(z).iana()))
            .filter(|z| z != UTC_ZONE);

        if is_all_day {
            let zone = display_tz.as_deref();
            let start = start_dt.as_deref().and_then(|s| WireDate(s).all_day(zone));
            if let Some(start) = start {
                card.insert(
                    "start".to_owned(),
                    Value::from(GraphEvent::all_day_key(start)),
                );
                let days = end_dt
                    .as_deref()
                    .and_then(|e| WireDate(e).all_day(zone))
                    .map_or(1, |end| (end - start).num_days().max(1));
                card.insert("duration".to_owned(), Value::from(format!("P{days}D")));
            }
            card.insert("showWithoutTime".to_owned(), Value::Bool(true));
        } else {
            if let Some(start) = start_dt.as_deref() {
                let value = display_tz
                    .as_deref()
                    .and_then(|tz| GraphEvent::utc_naive_to_local(start, tz))
                    .unwrap_or_else(|| start.to_owned());
                card.insert("start".to_owned(), Value::from(value));
            }
            if let (Some(start), Some(end)) = (start_dt.as_deref(), end_dt.as_deref())
                && let Some(dur) = GraphEvent::duration_iso8601(start, end)
            {
                card.insert("duration".to_owned(), Value::from(dur));
            }
            let label = display_tz.as_deref().unwrap_or(UTC_ZONE);
            card.insert("timeZone".to_owned(), Value::from(label));
        }

        if let Some(true) = graph_event.get("isCancelled").and_then(Value::as_bool) {
            card.insert("status".to_owned(), Value::from("cancelled"));
        } else {
            card.insert("status".to_owned(), Value::from("confirmed"));
        }

        if let Some(sens) = graph_event.get("sensitivity").and_then(Value::as_str) {
            card.insert(
                "privacy".to_owned(),
                Value::from(Privacy::from_sensitivity(sens).as_str()),
            );
        }

        if let Some(imp) = graph_event.get("importance").and_then(Value::as_str) {
            card.insert(
                "priority".to_owned(),
                Value::from(GraphEvent::priority(imp)),
            );
        }

        if let Some(show) = graph_event.get("showAs").and_then(Value::as_str) {
            card.insert(
                "freeBusyStatus".to_owned(),
                Value::from(GraphEvent::free_busy(show)),
            );
        }

        if let Some(cats) = graph_event.get("categories").and_then(Value::as_array)
            && !cats.is_empty()
        {
            let mut map = Map::new();
            for cat in cats.iter().filter_map(Value::as_str) {
                map.insert(cat.to_owned(), Value::Bool(true));
            }
            if !map.is_empty() {
                card.insert("keywords".to_owned(), Value::Object(map));
            }
        }

        if let Some(locs) = graph_event.get("locations").and_then(Value::as_array)
            && !locs.is_empty()
        {
            let mut locations = Map::new();
            let width = GraphEvent::pad_width(locs.len());
            for (i, loc) in locs.iter().enumerate() {
                let key = format!("loc-{:0width$}", i + 1, width = width);
                let mut object = Map::new();
                object.insert("@type".to_owned(), Value::from("Location"));
                let display_name = loc
                    .get("displayName")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty());
                let address_str = loc
                    .get("address")
                    .and_then(Value::as_object)
                    .map(GraphEvent::joined_address)
                    .filter(|s| !s.is_empty());
                let name = match (display_name, address_str.as_deref()) {
                    (Some(dn), Some(addr)) if dn != addr => Some(format!("{dn}, {addr}")),
                    (Some(dn), _) => Some(dn.to_owned()),
                    (None, Some(addr)) => Some(addr.to_owned()),
                    (None, None) => None,
                };
                if let Some(n) = name {
                    object.insert("name".to_owned(), Value::from(n));
                }
                locations.insert(key, Value::Object(object));
            }
            if !locations.is_empty() {
                card.insert("locations".to_owned(), Value::Object(locations));
            }
        }

        if let Some(recurrence) = graph_event.get("recurrence")
            && !recurrence.is_null()
        {
            let rule = GraphRecurrence(recurrence).to_rule_map()?;
            card.insert("recurrenceRule".to_owned(), Value::Object(rule));
        }

        let mut participants: Vec<(String, Value)> = Vec::new();
        if let Some(org) = graph_event
            .get("organizer")
            .and_then(|o| o.get("emailAddress"))
        {
            if let Some(resolved) = GraphEvent::resolve_calendar_address(org) {
                card.insert(
                    "organizerCalendarAddress".to_owned(),
                    Value::from(resolved.uri),
                );
            }
            let p = GraphEvent::build_participant(org, &["owner", "chair"], None, None);
            participants.push(("organizer".to_owned(), p));
        }
        if let Some(attendees) = graph_event.get("attendees").and_then(Value::as_array) {
            let width = GraphEvent::pad_width(attendees.len());
            for (i, att) in attendees.iter().enumerate() {
                let key = format!("att-{:0width$}", i + 1, width = width);
                let Some(email) = att.get("emailAddress") else {
                    continue;
                };
                let role = att
                    .get("type")
                    .and_then(Value::as_str)
                    .map(GraphEvent::attendee_role)
                    .unwrap_or("required");
                let status = att
                    .get("status")
                    .and_then(|s| s.get("response"))
                    .and_then(Value::as_str)
                    .and_then(Participation::from_graph);
                let p = GraphEvent::build_participant(email, &[role], status, Some(true));
                participants.push((key, p));
            }
        }
        if !participants.is_empty() {
            let mut map: Map<String, Value> = participants.into_iter().collect();
            if !card.contains_key("organizerCalendarAddress") {
                ParticipantMap(&mut map).drop_calendar_address_dependents();
            }
            card.insert("participants".to_owned(), Value::Object(map));
        }

        if let Some(created) = graph_event.get("createdDateTime").and_then(Value::as_str) {
            card.insert(
                "created".to_owned(),
                Value::from(WireDate(created).without_fraction()),
            );
        }
        if let Some(updated) = graph_event
            .get("lastModifiedDateTime")
            .and_then(Value::as_str)
        {
            card.insert(
                "updated".to_owned(),
                Value::from(WireDate(updated).without_fraction()),
            );
        }

        let use_default_alerts = false;
        if let Some(true) = graph_event.get("isReminderOn").and_then(Value::as_bool)
            && let Some(mins) = graph_event
                .get("reminderMinutesBeforeStart")
                .and_then(Value::as_i64)
        {
            let trigger = format!("-PT{}M", mins.unsigned_abs());
            let alert = json!({
                "@type": "Alert",
                "trigger": {
                    "@type": "OffsetTrigger",
                    "relativeTo": "start",
                    "offset": trigger
                }
            });
            let mut alerts = Map::new();
            alerts.insert("alert-1".to_owned(), alert);
            card.insert("alerts".to_owned(), Value::Object(alerts));
        }

        if let Some(cancels) = graph_event
            .get("cancelledOccurrences")
            .and_then(Value::as_array)
            && !cancels.is_empty()
        {
            let start_time = card
                .get("start")
                .and_then(Value::as_str)
                .and_then(|s| s.split_once('T').map(|(_, time)| time.to_owned()));
            let mut overrides = Map::new();
            for entry in cancels.iter().filter_map(Value::as_str) {
                if let Some(key) =
                    GraphEvent::cancelled_occurrence_key(entry, start_time.as_deref())
                {
                    overrides.insert(key, json!({"excluded": true}));
                }
            }
            if !overrides.is_empty() {
                card.insert("recurrenceOverrides".to_owned(), Value::Object(overrides));
            }
        }

        Ok(ConvertedEvent {
            uid,
            data: Value::Object(card),
            is_draft,
            use_default_alerts,
            series_master_id,
            event_type,
            original_start,
            zone: display_tz,
        })
    }

    pub fn all_day_key(date: NaiveDate) -> String {
        format!("{date}{MIDNIGHT}")
    }

    fn utc_naive_to_local(utc_naive: &str, iana: &str) -> Option<String> {
        let tz: Tz = iana.parse().ok()?;
        let naive = NaiveDateTime::parse_from_str(utc_naive, LOCAL_FORMAT).ok()?;
        Some(
            tz.from_utc_datetime(&naive)
                .naive_local()
                .format(LOCAL_FORMAT)
                .to_string(),
        )
    }

    fn cancelled_occurrence_key(entry: &str, start_time: Option<&str>) -> Option<String> {
        let date = entry.rsplit('.').next()?;
        let [y0, y1, y2, y3, b'-', m0, m1, b'-', d0, d1] = date.as_bytes() else {
            return None;
        };
        if ![y0, y1, y2, y3, m0, m1, d0, d1]
            .into_iter()
            .all(u8::is_ascii_digit)
        {
            return None;
        }
        Some(format!(
            "{date}T{}",
            start_time.unwrap_or(DEFAULT_START_TIME)
        ))
    }

    fn local_datetime(slot: Option<&Value>) -> Option<String> {
        let dt = slot?.get("dateTime").and_then(Value::as_str)?;
        Some(dt.split_once('.').map_or(dt, |(base, _)| base).to_owned())
    }

    fn non_empty<'v>(email: &'v Value, key: &str) -> Option<&'v str> {
        email
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    fn resolve_calendar_address(email: &Value) -> Option<ResolvedAddress> {
        ResolvedAddress::resolve(
            GraphEvent::non_empty(email, "address"),
            Routing::Smtp,
            GraphEvent::non_empty(email, "name"),
        )
    }

    fn joined_address(addr: &Map<String, Value>) -> String {
        let mut out = String::new();
        for part in ADDRESS_FIELDS
            .iter()
            .filter_map(|f| addr.get(*f).and_then(Value::as_str))
            .filter(|s| !s.is_empty())
        {
            if !out.is_empty() {
                out.push_str(", ");
            }
            out.push_str(part);
        }
        out
    }

    fn build_participant(
        email: &Value,
        roles: &[&str],
        participation: Option<Participation>,
        expect_reply: Option<bool>,
    ) -> Value {
        let mut map = Map::new();
        map.insert("@type".to_owned(), Value::from("Participant"));
        if let Some(name) = GraphEvent::non_empty(email, "name") {
            map.insert("name".to_owned(), Value::from(name));
        }
        if let Some(resolved) = GraphEvent::resolve_calendar_address(email) {
            map.insert("calendarAddress".to_owned(), Value::from(resolved.uri));
            if let Some(addr) = resolved.email {
                map.insert("email".to_owned(), Value::from(addr));
            }
            let role_map: Map<String, Value> = roles
                .iter()
                .map(|r| ((*r).to_owned(), Value::Bool(true)))
                .collect();
            map.insert("roles".to_owned(), Value::Object(role_map));
            if let Some(status) = participation {
                map.insert(
                    "participationStatus".to_owned(),
                    Value::from(status.as_str()),
                );
            }
            if let Some(b) = expect_reply {
                map.insert("expectReply".to_owned(), Value::Bool(b));
            }
        }
        Value::Object(map)
    }

    fn attendee_role(attendee_type: &str) -> &'static str {
        hashify::map!(
            attendee_type.as_bytes(),
            Role,
            "optional" => "optional",
            "resource" => "informational",
        )
        .copied()
        .unwrap_or("required")
    }

    fn pad_width(n: usize) -> usize {
        n.checked_ilog10().map_or(1, |digits| digits as usize + 1)
    }

    fn priority(importance: &str) -> i64 {
        hashify::map!(
            importance.as_bytes(),
            i64,
            "high" => 1,
            "low" => 9,
        )
        .copied()
        .unwrap_or(DEFAULT_PRIORITY)
    }

    fn free_busy(show_as: &str) -> &'static str {
        if show_as == "free" { "free" } else { "busy" }
    }

    fn duration_iso8601(start: &str, end: &str) -> Option<String> {
        let s = GraphEvent::naive_seconds(start)?;
        let e = GraphEvent::naive_seconds(end)?;
        IsoDuration(e - s).format(DurationStyle::Graph)
    }

    fn naive_seconds(s: &str) -> Option<i64> {
        let (date, time) = s.split_once('T')?;
        let mut date_parts = date.split('-');
        let year: i32 = date_parts.next()?.parse().ok()?;
        let month: u32 = date_parts.next()?.parse().ok()?;
        let day: u32 = date_parts.next()?.parse().ok()?;
        let time = time.split('.').next()?.trim_end_matches('Z');
        let mut time_parts = time.split(':');
        let hh: i64 = time_parts.next()?.parse().ok()?;
        let mm: i64 = time_parts.next()?.parse().ok()?;
        let ss: i64 = time_parts.next().unwrap_or("0").parse().ok()?;
        let days = NaiveDate::from_ymd_opt(year, month, day)?
            .signed_duration_since(NaiveDate::default())
            .num_days();
        Some(days * SECONDS_PER_DAY + hh * SECONDS_PER_HOUR + mm * SECONDS_PER_MINUTE + ss)
    }
}

#[derive(Debug, Clone)]
pub struct ConvertedEvent {
    pub uid: String,
    pub data: Value,
    pub is_draft: bool,
    pub use_default_alerts: bool,
    pub series_master_id: Option<String>,
    pub event_type: EventType,
    pub original_start: Option<String>,
    pub zone: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    SingleInstance,
    SeriesMaster,
    Occurrence,
    Exception,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;
    use serde_json::json;

    fn sample() -> Value {
        json!({
            "id": "AAA",
            "iCalUId": "uid-1",
            "type": "singleInstance",
            "subject": "Sync up",
            "body": {"contentType": "text", "content": "Agenda"},
            "start": {"dateTime": "2026-05-27T10:00:00.0000000", "timeZone": "UTC"},
            "end": {"dateTime": "2026-05-27T11:00:00.0000000", "timeZone": "UTC"},
            "isAllDay": false,
            "isCancelled": false,
            "sensitivity": "private",
            "importance": "high",
            "showAs": "busy",
            "categories": ["Red", "Internal"],
            "createdDateTime": "2026-05-01T08:00:00Z",
            "lastModifiedDateTime": "2026-05-02T08:00:00Z",
            "isReminderOn": true,
            "reminderMinutesBeforeStart": 15,
            "isDraft": false,
            "organizer": {"emailAddress": {"name": "Alice", "address": "alice@x.com"}},
            "attendees": [
                {"emailAddress": {"name": "Bob", "address": "bob@x.com"}, "type": "required"}
            ]
        })
    }

    #[test]
    fn classify_event_type_dispatch() {
        assert_eq!(
            GraphEvent(&json!({"type": "seriesMaster"})).event_type(),
            EventType::SeriesMaster
        );
        assert_eq!(
            GraphEvent(&json!({"type": "occurrence"})).event_type(),
            EventType::Occurrence
        );
        assert_eq!(
            GraphEvent(&json!({"type": "exception"})).event_type(),
            EventType::Exception
        );
        assert_eq!(
            GraphEvent(&json!({})).event_type(),
            EventType::SingleInstance
        );
    }

    #[test]
    fn single_instance_converts_to_event_value() {
        let conv = GraphEvent(&sample()).convert(Some("UTC")).unwrap();
        assert_eq!(conv.uid, "uid-1");
        assert_eq!(conv.data["@type"], "Event");
        assert_eq!(conv.data["title"], "Sync up");
        assert_eq!(conv.data["description"], "Agenda");
        assert_eq!(conv.data["start"], "2026-05-27T10:00:00");
        assert_eq!(conv.data["duration"], "PT1H");
        assert_eq!(conv.data["status"], "confirmed");
        assert_eq!(conv.data["privacy"], "private");
        assert_eq!(conv.data["priority"], 1);
        assert_eq!(conv.data["freeBusyStatus"], "busy");
        let kws = conv.data["keywords"].as_object().unwrap();
        assert!(kws.contains_key("Red"));
        assert!(kws.contains_key("Internal"));
        assert!(conv.data.get("categories").is_none());
        assert!(conv.data.get("version").is_none());
        assert_eq!(conv.data["timeZone"], "Etc/UTC");
        let alerts = conv.data["alerts"].as_object().unwrap();
        assert_eq!(alerts["alert-1"]["trigger"]["offset"], "-PT15M");
    }

    #[test]
    fn cancelled_event_status_is_cancelled() {
        let mut v = sample();
        v["isCancelled"] = Value::Bool(true);
        let conv = GraphEvent(&v).convert(None).unwrap();
        assert_eq!(conv.data["status"], "cancelled");
    }

    #[test]
    fn all_day_sets_show_without_time() {
        let mut v = sample();
        v["isAllDay"] = Value::Bool(true);
        let conv = GraphEvent(&v).convert(None).unwrap();
        assert_eq!(conv.data["showWithoutTime"], true);
    }

    #[test]
    fn priority_mapping_covers_three_levels() {
        let conv = GraphEvent(&sample()).convert(None).unwrap();
        assert_eq!(conv.data["priority"], 1);
        let mut v = sample();
        v["importance"] = Value::from("normal");
        assert_eq!(GraphEvent(&v).convert(None).unwrap().data["priority"], 5);
        v["importance"] = Value::from("low");
        assert_eq!(GraphEvent(&v).convert(None).unwrap().data["priority"], 9);
    }

    #[test]
    fn duration_handles_subhour() {
        assert_eq!(
            GraphEvent::duration_iso8601("2026-05-27T10:00:00", "2026-05-27T10:30:00").as_deref(),
            Some("PT30M")
        );
    }

    #[test]
    fn duration_handles_multi_day() {
        assert_eq!(
            GraphEvent::duration_iso8601("2026-05-27T10:00:00", "2026-05-28T11:30:45").as_deref(),
            Some("PT25H30M45S")
        );
    }

    #[test]
    fn windows_tz_fallback_returns_iana() {
        assert_eq!(
            ZoneId("Pacific Standard Time").iana().as_deref(),
            Some("America/Los_Angeles")
        );
        assert_eq!(
            ZoneId("America/New_York").iana().as_deref(),
            Some("America/New_York")
        );
        assert!(ZoneId("Made Up Zone").iana().is_none());
    }

    #[test]
    fn organizer_and_attendees_become_participants() {
        let conv = GraphEvent(&sample()).convert(None).unwrap();
        let participants = conv.data["participants"].as_object().unwrap();
        assert!(participants.contains_key("organizer"));
        assert_eq!(participants["organizer"]["email"], "alice@x.com");
        assert_eq!(
            participants["organizer"]["calendarAddress"],
            "mailto:alice@x.com"
        );
        assert_eq!(participants["organizer"]["roles"]["owner"], true);
        assert_eq!(participants["organizer"]["roles"]["chair"], true);
        assert!(participants.contains_key("att-1"));
        assert_eq!(participants["att-1"]["email"], "bob@x.com");
        assert_eq!(participants["att-1"]["roles"]["required"], true);
    }

    #[test]
    fn a_hybrid_x500_address_is_not_emitted_as_a_mailto() {
        let mut v = sample();
        v["attendees"] = json!([
            {
                "emailAddress": {
                    "name": "Kristina Morgental",
                    "address": "/o=ExchangeLabs/ou=Exchange Administrative Group \
                        (FYDIBOHF23SPDLT)/cn=Recipients/cn=bdc77b18152647a29d28ce1188376dc9-kristina"
                },
                "type": "required",
                "status": {"response": "none"}
            }
        ]);
        let conv = GraphEvent(&v).convert(None).unwrap();
        let p = &conv.data["participants"]["att-1"];
        let addr = p["calendarAddress"].as_str().unwrap();
        assert!(
            addr.starts_with("urn:x-vandelay:attendee:"),
            "a hybrid X500 reference must not become a malformed mailto, got {addr}"
        );
        assert!(
            p.get("email").is_none(),
            "an X500 reference is not an email address"
        );
        assert_eq!(p["name"], "Kristina Morgental", "the display name is kept");
    }

    #[test]
    fn a_prefixed_or_bracketed_address_is_normalised() {
        let mut v = sample();
        v["attendees"] = json!([
            {
                "emailAddress": {"name": "Bob", "address": "SMTP:bob@example.com"},
                "type": "required",
                "status": {"response": "none"}
            },
            {
                "emailAddress": {"name": "Eve", "address": "Eve <eve@example.com>"},
                "type": "required",
                "status": {"response": "none"}
            }
        ]);
        let conv = GraphEvent(&v).convert(None).unwrap();
        assert_eq!(
            conv.data["participants"]["att-1"]["calendarAddress"],
            "mailto:bob@example.com"
        );
        assert_eq!(
            conv.data["participants"]["att-1"]["email"],
            "bob@example.com"
        );
        assert_eq!(
            conv.data["participants"]["att-2"]["calendarAddress"],
            "mailto:eve@example.com"
        );
    }

    #[test]
    fn a_name_only_attendee_gets_a_stable_synthetic_calendar_address() {
        let mut v = sample();
        v["attendees"] = json!([
            {
                "emailAddress": {"name": "Room 4"},
                "type": "resource",
                "status": {"response": "accepted"}
            }
        ]);
        let conv = GraphEvent(&v).convert(None).unwrap();
        let p = &conv.data["participants"]["att-1"];
        let addr = p["calendarAddress"].as_str().unwrap();
        assert!(
            addr.starts_with("urn:x-vandelay:attendee:"),
            "a name-only attendee needs a synthetic calendarAddress, got {addr}"
        );
        assert!(
            p.get("email").is_none(),
            "there is no SMTP address to report as email"
        );
        assert_eq!(p["participationStatus"], "accepted");
        assert_eq!(p["roles"]["informational"], true);
        assert_eq!(
            addr,
            GraphEvent(&v).convert(None).unwrap().data["participants"]["att-1"]["calendarAddress"]
        );
    }

    #[test]
    fn an_unidentifiable_attendee_keeps_no_calendar_address_dependent_property() {
        let mut v = sample();
        v["attendees"] = json!([
            {"emailAddress": {}, "type": "required", "status": {"response": "accepted"}}
        ]);
        let conv = GraphEvent(&v).convert(None).unwrap();
        let p = &conv.data["participants"]["att-1"];
        for key in [
            "calendarAddress",
            "email",
            "roles",
            "participationStatus",
            "expectReply",
        ] {
            assert!(
                p.get(key).is_none(),
                "{key} requires calendarAddress (jscalendarbis 3.4.6)"
            );
        }
        assert_eq!(p["@type"], "Participant");
    }

    #[test]
    fn an_organizer_without_an_address_still_yields_a_calendar_address() {
        let mut v = sample();
        v["organizer"] = json!({"emailAddress": {"name": "Alice Example"}});
        let conv = GraphEvent(&v).convert(None).unwrap();
        let organizer_address = conv.data["organizerCalendarAddress"].as_str().unwrap();
        assert!(organizer_address.starts_with("urn:x-vandelay:attendee:"));
        assert_eq!(
            conv.data["participants"]["organizer"]["calendarAddress"],
            organizer_address
        );
    }

    #[test]
    fn participants_drop_calendar_addresses_when_no_organizer_is_known() {
        let mut v = sample();
        v.as_object_mut().unwrap().remove("organizer");
        let conv = GraphEvent(&v).convert(None).unwrap();
        assert!(
            conv.data.get("organizerCalendarAddress").is_none(),
            "no organizer is derivable from the source event"
        );
        let participants = conv.data["participants"].as_object().unwrap();
        assert!(!participants.is_empty());
        for (key, participant) in participants {
            assert!(
                participant.get("calendarAddress").is_none(),
                "{key} may not carry a calendarAddress without organizerCalendarAddress"
            );
            assert!(participant.get("roles").is_none(), "{key} kept roles");
        }
    }

    #[test]
    fn optional_attendee_keeps_role_and_uses_status_when_present() {
        let mut v = sample();
        v["attendees"] = json!([
            {
                "emailAddress": {"name": "Carol", "address": "carol@x.com"},
                "type": "optional",
                "status": {"response": "tentativelyAccepted"}
            }
        ]);
        let conv = GraphEvent(&v).convert(None).unwrap();
        let p = &conv.data["participants"]["att-1"];
        assert_eq!(p["roles"]["optional"], true);
        assert!(p["roles"].get("required").is_none());
        assert_eq!(p["participationStatus"], "tentative");
    }

    #[test]
    fn many_attendees_zero_pad_keys_for_stable_sort() {
        let mut v = sample();
        let mut atts = Vec::new();
        for i in 1..=11 {
            atts.push(json!({
                "emailAddress": {"address": format!("u{i}@x.com")},
                "type": "required"
            }));
        }
        v["attendees"] = Value::Array(atts);
        let conv = GraphEvent(&v).convert(None).unwrap();
        let map = conv.data["participants"].as_object().unwrap();
        assert!(map.contains_key("att-01"));
        assert!(map.contains_key("att-11"));
    }

    #[test]
    fn original_start_time_zone_is_preserved() {
        let mut v = sample();
        v["originalStartTimeZone"] = Value::from("Pacific Standard Time");
        v["start"]["timeZone"] = Value::from("UTC");
        let conv = GraphEvent(&v).convert(None).unwrap();
        assert_eq!(conv.data["timeZone"], "America/Los_Angeles");
        assert_eq!(conv.data["start"], "2026-05-27T03:00:00");
    }

    #[test]
    fn utc_times_are_converted_into_the_original_zone() {
        let mut v = sample();
        v["originalStartTimeZone"] = Value::from("Europe/Paris");
        v["start"]["dateTime"] = Value::from("2025-03-12T07:30:00.0000000");
        v["start"]["timeZone"] = Value::from("UTC");
        v["end"]["dateTime"] = Value::from("2025-03-12T08:00:00.0000000");
        v["end"]["timeZone"] = Value::from("UTC");
        let conv = GraphEvent(&v).convert(None).unwrap();
        assert_eq!(conv.data["start"], "2025-03-12T08:30:00");
        assert_eq!(conv.data["timeZone"], "Europe/Paris");
        assert_eq!(conv.data["duration"], "PT30M");
    }

    #[test]
    fn summer_event_shifts_by_two_hours_in_paris() {
        let mut v = sample();
        v["originalStartTimeZone"] = Value::from("Romance Standard Time");
        v["start"]["dateTime"] = Value::from("2025-07-15T06:30:00.0000000");
        v["start"]["timeZone"] = Value::from("UTC");
        v["end"]["dateTime"] = Value::from("2025-07-15T07:30:00.0000000");
        v["end"]["timeZone"] = Value::from("UTC");
        let conv = GraphEvent(&v).convert(None).unwrap();
        assert_eq!(conv.data["start"], "2025-07-15T08:30:00");
        assert_eq!(conv.data["timeZone"], "Europe/Paris");
    }

    #[test]
    fn unresolvable_microsoft_zone_falls_back_to_utc() {
        let mut v = sample();
        v["originalStartTimeZone"] = Value::from("tzone://Microsoft/Custom");
        v["start"]["dateTime"] = Value::from("2025-03-12T07:30:00.0000000");
        v["start"]["timeZone"] = Value::from("UTC");
        let conv = GraphEvent(&v).convert(None).unwrap();
        assert_eq!(conv.data["start"], "2025-03-12T07:30:00");
        assert_eq!(conv.data["timeZone"], "Etc/UTC");
    }

    #[test]
    fn all_day_keeps_date_and_omits_timezone() {
        let mut v = sample();
        v["isAllDay"] = Value::Bool(true);
        v["originalStartTimeZone"] = Value::from("Europe/Paris");
        v["start"]["dateTime"] = Value::from("2025-03-12T00:00:00.0000000");
        v["start"]["timeZone"] = Value::from("UTC");
        let conv = GraphEvent(&v).convert(None).unwrap();
        assert_eq!(conv.data["start"], "2025-03-12T00:00:00");
        assert_eq!(conv.data["showWithoutTime"], true);
        assert!(conv.data.get("timeZone").is_none());
    }

    #[test]
    fn all_day_utc_instant_east_of_utc_keeps_its_local_date() {
        let mut v = sample();
        v["isAllDay"] = Value::Bool(true);
        v["originalStartTimeZone"] = Value::from("Romance Standard Time");
        v["start"]["dateTime"] = Value::from("2026-10-23T22:00:00.0000000");
        v["end"]["dateTime"] = Value::from("2026-10-25T23:00:00.0000000");
        let conv = GraphEvent(&v).convert(None).unwrap();
        assert_eq!(conv.data["start"], "2026-10-24T00:00:00");
        assert_eq!(conv.data["duration"], "P2D");
        assert_eq!(conv.zone.as_deref(), Some("Europe/Paris"));
    }

    #[test]
    fn all_day_midnight_is_kept_whatever_the_zone() {
        let mut v = sample();
        v["isAllDay"] = Value::Bool(true);
        v["originalStartTimeZone"] = Value::from("New Zealand Standard Time");
        v["start"]["dateTime"] = Value::from("2026-11-12T00:00:00.0000000");
        v["end"]["dateTime"] = Value::from("2026-11-13T00:00:00.0000000");
        let conv = GraphEvent(&v).convert(None).unwrap();
        assert_eq!(conv.data["start"], "2026-11-12T00:00:00");
        assert_eq!(conv.data["duration"], "P1D");
    }

    #[test]
    fn duration_h_and_s_no_minutes_includes_zero_m() {
        assert_eq!(
            GraphEvent::duration_iso8601("2026-05-27T10:00:00", "2026-05-27T11:00:05").as_deref(),
            Some("PT1H0M5S")
        );
    }

    #[test]
    fn fractional_seconds_are_stripped_from_timestamps() {
        let mut v = sample();
        v["createdDateTime"] = Value::from("2026-05-01T08:00:00.0000000Z");
        v["lastModifiedDateTime"] = Value::from("2026-05-02T08:00:00.123Z");
        let conv = GraphEvent(&v).convert(None).unwrap();
        assert_eq!(conv.data["created"], "2026-05-01T08:00:00Z");
        assert_eq!(conv.data["updated"], "2026-05-02T08:00:00Z");
    }

    #[test]
    fn organizer_calendar_address_is_set_alongside_participants() {
        let conv = GraphEvent(&sample()).convert(None).unwrap();
        assert_eq!(conv.data["organizerCalendarAddress"], "mailto:alice@x.com");
    }

    #[test]
    fn recurrence_emits_singular_recurrence_rule_object() {
        let mut v = sample();
        v["recurrence"] = json!({
            "pattern": {"type": "daily", "interval": 1},
            "range": {"type": "noEnd"}
        });
        let conv = GraphEvent(&v).convert(None).unwrap();
        assert!(conv.data.get("recurrenceRules").is_none());
        let rule = &conv.data["recurrenceRule"];
        assert!(rule.is_object());
        assert_eq!(rule["@type"], "RecurrenceRule");
        assert_eq!(rule["frequency"], "daily");
    }

    #[test]
    fn location_with_address_only_uses_name_property() {
        let mut v = sample();
        v["locations"] = json!([{
            "address": {"street": "1 Infinite Loop", "city": "Cupertino"}
        }]);
        let conv = GraphEvent(&v).convert(None).unwrap();
        let locs = conv.data["locations"].as_object().unwrap();
        let only = locs.values().next().unwrap();
        assert_eq!(only["@type"], "Location");
        assert_eq!(only["name"], "1 Infinite Loop, Cupertino");
        assert!(only.get("description").is_none());
    }

    fn reference_days_from_civil(y: i64, m: i64, d: i64) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = (if y >= 0 { y } else { y - 399 }) / 400;
        let yoe = (y - era * 400) as u64;
        let m_adj: i64 = if m > 2 { m - 3 } else { m + 9 };
        let doy = ((153 * m_adj + 2) / 5 + d - 1) as u64;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146097 + (doe as i64) - 719468
    }

    #[test]
    fn naive_seconds_match_the_hand_rolled_civil_day_count() {
        // R20: chrono replaces days_from_civil; every valid day from 1600 to 2400 agrees.
        let mut day = NaiveDate::from_ymd_opt(1600, 1, 1).unwrap();
        let last = NaiveDate::from_ymd_opt(2400, 12, 31).unwrap();
        while day <= last {
            let text = format!("{}T12:34:56.789", day.format("%Y-%m-%d"));
            let (y, m, d) = (
                i64::from(day.year()),
                i64::from(day.month()),
                i64::from(day.day()),
            );
            let expected = reference_days_from_civil(y, m, d) * 86_400 + 12 * 3_600 + 34 * 60 + 56;
            assert_eq!(GraphEvent::naive_seconds(&text), Some(expected), "{text}");
            day = day.succ_opt().unwrap();
        }
    }
}
