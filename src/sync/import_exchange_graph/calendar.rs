/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;

use rusqlite::{Connection, Transaction, params};
use serde_json::{Value, json};

use crate::db::exchange_graph_ids;
use crate::error::Error;
use crate::exchange::jscalendar::override_patch_from_event;
use crate::exchange_graph::api::{self, PREFER_BODY_HTML, PREFER_BODY_TEXT, PREFER_TIMEZONE_UTC};
use crate::exchange_graph::calendar_map::{
    ConvertedEvent, EventType, classify_event_type, convert_event,
};
use crate::exchange_graph::error::GraphError;
use crate::exchange_graph::types::EventBodyFormat;
use crate::logging::LEVEL_PROGRESS;
use crate::sync::TypeCounts;
use crate::sync::import_jmap::pool::Pool;

use super::coordinator::{CHUNK_SIZE, GraphCoordinator};
use super::folders::CalendarFolder;

pub type EventAttachment = (String, String, Vec<u8>);

fn fetch_event_attachments(
    client: &crate::exchange_graph::client::GraphClient,
    endpoints: &crate::exchange_graph::api::Endpoints,
    event_id: &str,
) -> Vec<EventAttachment> {
    use base64::Engine;
    let url = endpoints.event_attachments(event_id);
    let Ok(body) = client.get_json_with_prefer(&url, &[]) else {
        return Vec::new();
    };
    let Some(items) = body.get("value").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        if item.get("@odata.type").and_then(Value::as_str)
            != Some("#microsoft.graph.fileAttachment")
        {
            continue;
        }
        let Some(encoded) = item.get("contentBytes").and_then(Value::as_str) else {
            continue;
        };
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
            continue;
        };
        if bytes.is_empty() {
            continue;
        }
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("attachment")
            .to_owned();
        let content_type = item
            .get("contentType")
            .and_then(Value::as_str)
            .unwrap_or("application/octet-stream")
            .to_owned();
        out.push((name, content_type, bytes));
    }
    out
}

pub fn reconcile_all(
    conn: &mut Connection,
    ctx: &GraphCoordinator<'_>,
    calendars: &[CalendarFolder],
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let local: HashMap<String, i64> =
        exchange_graph_ids::ids_of_type(conn, ctx.source_id, exchange_graph_ids::CALENDAR_EVENT)?;
    let mut server_total: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut planned: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut any_failure = false;

    for cal in calendars {
        let url = ctx.endpoints.calendar_events_ids(&cal.graph_id, ctx.top);
        let stubs = match api::collect_all_values(ctx.client, &url, &[PREFER_TIMEZONE_UTC]) {
            Ok(v) => v,
            Err(e) => {
                ctx.logger
                    .warn(&format!("calendar {} stub fetch failed: {e}", cal.graph_id));
                counts.failed += 1;
                any_failure = true;
                continue;
            }
        };
        let mut want_ids: Vec<String> = Vec::new();
        let mut occurrence_count = 0usize;
        let mut series_masters: Vec<String> = Vec::new();
        for stub in &stubs {
            match classify_event_type(stub) {
                EventType::Occurrence => occurrence_count += 1,
                _ => {
                    if matches!(classify_event_type(stub), EventType::SeriesMaster)
                        && let Some(id) = stub.get("id").and_then(Value::as_str)
                    {
                        series_masters.push(id.to_owned());
                    }
                    if let Some(id) = stub.get("id").and_then(Value::as_str) {
                        server_total.insert(id.to_owned());
                        if local.contains_key(id) {
                            counts.fetched += 1;
                        } else if planned.insert(id.to_owned()) {
                            want_ids.push(id.to_owned());
                        }
                    }
                }
            }
        }
        if ctx.logger.enabled(LEVEL_PROGRESS) {
            eprintln!(
                "graph calendar {} events: stubs={} new={} occurrences_skipped={}",
                cal.graph_id,
                stubs.len(),
                want_ids.len(),
                occurrence_count
            );
        }
        let fetched = fetch_events(ctx, &want_ids);
        let mut masters: Vec<(String, ConvertedEvent)> = Vec::new();
        let mut attachments_by_id: HashMap<String, Vec<EventAttachment>> = HashMap::new();
        for (graph_id, result, attachments) in fetched {
            match result {
                Ok(raw) => match convert_event(&raw, None) {
                    Ok(c) => {
                        attachments_by_id.insert(graph_id.clone(), attachments);
                        masters.push((graph_id, c));
                    }
                    Err(e) => {
                        counts.failed += 1;
                        ctx.logger
                            .warn(&format!("graph event {graph_id} convert failed: {e}"));
                    }
                },
                Err(GraphError::Vanished) => counts.skipped += 1,
                Err(e) => {
                    counts.failed += 1;
                    ctx.logger
                        .warn(&format!("graph event {graph_id} fetch failed: {e}"));
                }
            }
        }

        let master_by_graph_id: HashMap<String, ConvertedEvent> = masters.into_iter().collect();
        let pairs: Vec<(String, ConvertedEvent)> = master_by_graph_id.into_iter().collect();
        insert_events_chunked(conn, ctx, cal.local_id, &pairs, &attachments_by_id, counts)?;

        if !series_masters.is_empty() {
            apply_series_expansion(conn, ctx, &cal.graph_id, &series_masters, counts)?;
        }
    }

    if any_failure {
        ctx.logger.warn(
            "graph event vanished-cleanup skipped: one or more calendars failed to enumerate; \
             a clean re-run will reconcile deletions",
        );
    } else {
        delete_vanished(conn, ctx.source_id, &local, &server_total, counts)?;
    }
    Ok(())
}

fn fetch_events(
    ctx: &GraphCoordinator<'_>,
    ids: &[String],
) -> Vec<(String, Result<Value, GraphError>, Vec<EventAttachment>)> {
    type R = (String, Result<Value, GraphError>, Vec<EventAttachment>);
    let client = ctx.client.clone();
    let endpoints: crate::exchange_graph::api::Endpoints = (*ctx.endpoints).clone();
    let body_prefer = match ctx.event_body_format {
        EventBodyFormat::Text => PREFER_BODY_TEXT,
        EventBodyFormat::Html => PREFER_BODY_HTML,
    };
    let prefer: Vec<String> = vec![PREFER_TIMEZONE_UTC.to_owned(), body_prefer.to_owned()];
    let want_attachments = ctx.event_attachments;
    let pool: Pool<String, R> = Pool::new(ctx.workers, move |id: String| {
        let url = endpoints.event(&id);
        let prefer_refs: Vec<&str> = prefer.iter().map(String::as_str).collect();
        let result = client.get_json_with_prefer(&url, &prefer_refs);
        let attachments = match (&result, want_attachments) {
            (Ok(raw), true) if raw.get("hasAttachments").and_then(Value::as_bool) == Some(true) => {
                fetch_event_attachments(&client, &endpoints, &id)
            }
            _ => Vec::new(),
        };
        (id, result, attachments)
    });
    for id in ids {
        pool.submit(id.clone());
    }
    let mut out = Vec::with_capacity(ids.len());
    for _ in 0..ids.len() {
        if let Ok(r) = pool.results().recv() {
            out.push(r);
        }
    }
    out
}

fn exception_windows(years: i32) -> Vec<(String, String)> {
    let today = chrono::Utc::now().date_naive();
    let span = chrono::Duration::days(
        i64::from(years.max(1))
            .saturating_mul(365)
            .min(api::EXCEPTION_WINDOW_MAX_DAYS),
    );
    let fmt = |d: chrono::NaiveDate| d.format("%Y-%m-%dT00:00:00").to_string();
    vec![
        (fmt(today - span), fmt(today)),
        (fmt(today), fmt(today + span)),
    ]
}

fn fetch_calendar_exceptions(
    ctx: &GraphCoordinator<'_>,
    calendar_id: &str,
    counts: &mut TypeCounts,
) -> Vec<(String, ConvertedEvent)> {
    let windows = exception_windows(ctx.exception_window_years);
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (from, to) in windows {
        let url = ctx
            .endpoints
            .calendar_exceptions(calendar_id, &from, &to, ctx.top);
        match api::collect_all_values(ctx.client, &url, &[PREFER_TIMEZONE_UTC]) {
            Ok(values) => {
                if ctx.logger.enabled(LEVEL_PROGRESS) {
                    eprintln!(
                        "graph calendar {calendar_id} exceptions in {from}..{to}: {}",
                        values.len()
                    );
                }
                for raw in values {
                    let Some(ex_id) = raw.get("id").and_then(Value::as_str).map(str::to_owned)
                    else {
                        continue;
                    };
                    if !seen.insert(ex_id.clone()) {
                        continue;
                    }
                    match convert_event(&raw, None) {
                        Ok(c) => out.push((ex_id, c)),
                        Err(e) => {
                            counts.failed += 1;
                            ctx.logger
                                .warn(&format!("graph exception {ex_id} convert failed: {e}"));
                        }
                    }
                }
            }
            Err(e) => {
                counts.failed += 1;
                ctx.logger.warn(&format!(
                    "graph calendar {calendar_id} exception lookup {from}..{to} failed: {e}"
                ));
            }
        }
    }
    out
}

fn apply_series_expansion(
    conn: &mut Connection,
    ctx: &GraphCoordinator<'_>,
    calendar_id: &str,
    series_masters: &[String],
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let exceptions = fetch_calendar_exceptions(ctx, calendar_id, counts);
    let occurrences = fetch_calendar_occurrences(ctx, calendar_id, counts);
    let windows = exception_windows(ctx.exception_window_years);
    let ids: HashMap<String, i64> =
        exchange_graph_ids::ids_of_type(conn, ctx.source_id, exchange_graph_ids::CALENDAR_EVENT)?;

    let mut by_master: HashMap<&str, Vec<&ConvertedEvent>> = HashMap::new();
    for (_, ex) in &exceptions {
        if let Some(master) = ex.series_master_id.as_deref() {
            by_master.entry(master).or_default().push(ex);
        }
    }

    for master_id in series_masters {
        let Some(local_id) = ids.get(master_id).copied() else {
            continue;
        };
        let stored: String = match conn.query_row(
            "SELECT data FROM calendar_events WHERE id = ?1",
            params![local_id],
            |row| row.get(0),
        ) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Ok(mut card) = serde_json::from_str::<Value>(&stored) else {
            continue;
        };
        let before = card.clone();

        for ex in by_master.get(master_id.as_str()).into_iter().flatten() {
            let Some(raw_key) = ex.original_start.as_deref() else {
                continue;
            };
            let tz = card
                .get("timeZone")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let key = normalise_override_key(raw_key, tz.as_deref());
            if let Some(map) = card.as_object_mut() {
                let entry = map
                    .entry("recurrenceOverrides".to_owned())
                    .or_insert_with(|| Value::Object(serde_json::Map::new()));
                if let Some(overrides) = entry.as_object_mut() {
                    overrides.insert(key, override_patch_from_event(&ex.data));
                }
            }
        }

        let cancelled = cancelled_occurrence_keys(
            &card,
            master_id,
            &occurrences,
            by_master
                .get(master_id.as_str())
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            &windows,
        );
        if !cancelled.is_empty()
            && let Some(map) = card.as_object_mut()
        {
            let entry = map
                .entry("recurrenceOverrides".to_owned())
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if let Some(overrides) = entry.as_object_mut() {
                for key in cancelled {
                    overrides
                        .entry(key)
                        .or_insert_with(|| json!({"excluded": true}));
                }
            }
        }

        if card != before {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "UPDATE calendar_events SET data = ?1 WHERE id = ?2",
                params![card.to_string(), local_id],
            )?;
            tx.commit()?;
            counts.updated += 1;
        }
    }
    Ok(())
}

fn cancelled_occurrence_keys(
    card: &Value,
    master_id: &str,
    occurrences: &HashMap<String, Vec<String>>,
    exceptions: &[&ConvertedEvent],
    windows: &[(String, String)],
) -> Vec<String> {
    let Some(rule) = card.get("recurrenceRule") else {
        return Vec::new();
    };
    let Some(start) = card.get("start").and_then(Value::as_str) else {
        return Vec::new();
    };
    let (start_date, start_time) = match start.split_once('T') {
        Some((d, t)) => (d, t),
        None => (start, "00:00:00"),
    };
    let Ok(series_start) = chrono::NaiveDate::parse_from_str(start_date, "%Y-%m-%d") else {
        return Vec::new();
    };
    let Some(pattern) =
        crate::exchange_graph::expand::graph_pattern_from_jscalendar(rule, series_start)
    else {
        return Vec::new();
    };
    let tz = card.get("timeZone").and_then(Value::as_str);

    let mut actual: std::collections::HashSet<String> = std::collections::HashSet::new();
    for utc in occurrences.get(master_id).into_iter().flatten() {
        actual.insert(local_date_of(utc, tz));
    }
    for ex in exceptions {
        if let Some(raw) = ex.original_start.as_deref() {
            let key = normalise_override_key(raw, tz);
            actual.insert(key.split('T').next().unwrap_or(&key).to_owned());
        }
    }

    let mut out = Vec::new();
    for (from, to) in windows {
        let (Ok(w0), Ok(w1)) = (
            chrono::NaiveDate::parse_from_str(&from[..10], "%Y-%m-%d"),
            chrono::NaiveDate::parse_from_str(&to[..10], "%Y-%m-%d"),
        ) else {
            continue;
        };
        let Some(expected) = crate::exchange_graph::expand::expected_dates(&pattern, w0, w1) else {
            return Vec::new();
        };
        for date in expected {
            let key = date.format("%Y-%m-%d").to_string();
            if !actual.contains(&key) {
                out.push(format!("{key}T{start_time}"));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn local_date_of(utc: &str, tz: Option<&str>) -> String {
    let key = normalise_override_key(utc, tz);
    key.split('T').next().unwrap_or(&key).to_owned()
}

fn fetch_calendar_occurrences(
    ctx: &GraphCoordinator<'_>,
    calendar_id: &str,
    counts: &mut TypeCounts,
) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for (from, to) in exception_windows(ctx.exception_window_years) {
        let url = ctx
            .endpoints
            .calendar_occurrences(calendar_id, &from, &to, ctx.top);
        match api::collect_all_values(ctx.client, &url, &[PREFER_TIMEZONE_UTC]) {
            Ok(values) => {
                for raw in values {
                    let Some(master) = raw.get("seriesMasterId").and_then(Value::as_str) else {
                        continue;
                    };
                    let Some(start) = raw
                        .get("start")
                        .and_then(|s| s.get("dateTime"))
                        .and_then(Value::as_str)
                    else {
                        continue;
                    };
                    out.entry(master.to_owned())
                        .or_default()
                        .push(start.to_owned());
                }
            }
            Err(e) => {
                counts.failed += 1;
                ctx.logger.warn(&format!(
                    "graph calendar {calendar_id} occurrence sweep {from}..{to} failed: {e}; \
                     deleted occurrences cannot be detected for this calendar"
                ));
            }
        }
    }
    out
}

pub fn normalise_override_key(raw: &str, master_tz: Option<&str>) -> String {
    if let Some(tz_name) = master_tz
        && let Some(local) = utc_offset_to_local(raw, tz_name)
    {
        return local;
    }
    strip_offset_and_fractional(raw)
}

fn utc_offset_to_local(raw: &str, tz_name: &str) -> Option<String> {
    use chrono::{DateTime, NaiveDateTime, TimeZone};
    use chrono_tz::Tz;
    let tz: Tz = tz_name.parse().ok()?;
    let parsed: DateTime<chrono::FixedOffset> = if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        dt
    } else if let Ok(naive) = NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(format_local(naive));
    } else if let Ok(naive) = NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S") {
        return Some(format_local(naive));
    } else {
        return None;
    };
    let local = tz.from_utc_datetime(&parsed.naive_utc()).naive_local();
    Some(format_local(local))
}

fn format_local(dt: chrono::NaiveDateTime) -> String {
    dt.format("%Y-%m-%dT%H:%M:%S").to_string()
}

fn strip_offset_and_fractional(raw: &str) -> String {
    let no_offset = if let Some(idx) = raw.find('Z') {
        &raw[..idx]
    } else if let Some(idx) = raw.find('+') {
        &raw[..idx]
    } else if let Some(idx) = raw.rfind('-')
        && idx > 10
    {
        &raw[..idx]
    } else {
        raw
    };
    let trimmed = no_offset.split('.').next().unwrap_or(no_offset);
    if trimmed.contains('T') {
        trimmed.to_owned()
    } else {
        format!("{trimmed}T00:00:00")
    }
}

fn insert_events_chunked(
    conn: &mut Connection,
    ctx: &GraphCoordinator<'_>,
    calendar_local_id: i64,
    pairs: &[(String, ConvertedEvent)],
    attachments: &HashMap<String, Vec<EventAttachment>>,
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    for chunk in pairs.chunks(CHUNK_SIZE) {
        let tx = conn.unchecked_transaction()?;
        for (graph_id, event) in chunk {
            let files = attachments.get(graph_id).map(Vec::as_slice).unwrap_or(&[]);
            insert_event_in_tx(&tx, ctx, calendar_local_id, graph_id, event, files, counts)?;
        }
        tx.commit()?;
    }
    Ok(())
}

fn insert_event_in_tx(
    tx: &Transaction<'_>,
    ctx: &GraphCoordinator<'_>,
    calendar_local_id: i64,
    graph_id: &str,
    event: &ConvertedEvent,
    attachments: &[EventAttachment],
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let calendar_ids = json!([calendar_local_id]).to_string();
    let mut card = event.data.clone();
    attach_links(tx, &mut card, attachments)?;
    let data = card.to_string();
    tx.execute(
        "INSERT INTO calendar_events (calendar_ids, is_draft, use_default_alerts, data, data_type)
         VALUES (?1, ?2, ?3, ?4, 'Event')",
        params![
            calendar_ids,
            event.is_draft as i64,
            event.use_default_alerts as i64,
            data,
        ],
    )?;
    let new_id = tx.last_insert_rowid();
    exchange_graph_ids::insert(
        tx,
        ctx.source_id,
        exchange_graph_ids::CALENDAR_EVENT,
        graph_id,
        new_id,
    )?;
    counts.created += 1;
    Ok(())
}

fn attach_links(
    tx: &Transaction<'_>,
    card: &mut Value,
    attachments: &[EventAttachment],
) -> Result<(), Error> {
    if attachments.is_empty() {
        return Ok(());
    }
    let mut links = serde_json::Map::new();
    for (idx, (name, content_type, bytes)) in (1u32..).zip(attachments) {
        let blob_id = crate::db::blobs::intern_blob(tx, bytes)?;
        links.insert(
            idx.to_string(),
            json!({
                "@type": "Link",
                "@blob": blob_id,
                "contentType": content_type,
                "title": name,
                "rel": "enclosure",
            }),
        );
    }
    if let Some(map) = card.as_object_mut() {
        match map.get_mut("links").and_then(Value::as_object_mut) {
            Some(existing) => existing.extend(links),
            None => {
                map.insert("links".to_owned(), Value::Object(links));
            }
        }
    }
    Ok(())
}

fn delete_vanished(
    conn: &mut Connection,
    source_id: i64,
    local: &HashMap<String, i64>,
    server: &std::collections::HashSet<String>,
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let vanished: Vec<(&String, &i64)> = local
        .iter()
        .filter(|(graph_id, _)| !server.contains(graph_id.as_str()))
        .collect();
    for chunk in vanished.chunks(CHUNK_SIZE) {
        let tx = conn.unchecked_transaction()?;
        for (graph_id, local_id) in chunk {
            let result = tx.execute(
                "DELETE FROM calendar_events WHERE id = ?1",
                params![local_id],
            );
            match result {
                Ok(_) => {
                    exchange_graph_ids::delete(
                        &tx,
                        source_id,
                        exchange_graph_ids::CALENDAR_EVENT,
                        graph_id,
                    )?;
                    counts.deleted += 1;
                }
                Err(_) => {
                    counts.failed += 1;
                }
            }
        }
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exception_windows_never_exceed_the_graph_limit() {
        for requested in [1, 5, 50] {
            let windows = exception_windows(requested);
            assert_eq!(windows.len(), 2, "one window behind today, one ahead");
            for (from, to) in windows {
                let start = chrono::NaiveDate::parse_from_str(&from[..10], "%Y-%m-%d").unwrap();
                let end = chrono::NaiveDate::parse_from_str(&to[..10], "%Y-%m-%d").unwrap();
                let days = (end - start).num_days();
                assert!(days > 0, "window runs forwards: {from}..{to}");
                assert!(
                    days <= api::EXCEPTION_WINDOW_MAX_DAYS,
                    "requested {requested}y produced {days} days, over Graph's cap"
                );
            }
        }
    }

    #[test]
    fn cancelled_occurrence_keys_drop_the_oid_prefix() {
        let raw = serde_json::json!({
            "id": "M",
            "start": {"dateTime": "2026-09-21T11:00:00.0000000", "timeZone": "UTC"},
            "end": {"dateTime": "2026-09-21T11:30:00.0000000", "timeZone": "UTC"},
            "recurrence": {
                "pattern": {"type": "daily", "interval": 1},
                "range": {"type": "numbered", "startDate": "2026-09-21",
                          "numberOfOccurrences": 6}
            },
            "cancelledOccurrences": ["OID.AAMkAGI2TGuLAAA=.2026-09-24"]
        });
        let converted = convert_event(&raw, None).unwrap();
        let overrides = converted.data["recurrenceOverrides"].as_object().unwrap();
        assert!(
            overrides.contains_key("2026-09-24T11:00:00"),
            "beta cancelledOccurrences are OID.<id>.<date>; the key must be a LocalDateTime, \
             got {:?}",
            overrides.keys().collect::<Vec<_>>()
        );
        assert_eq!(overrides["2026-09-24T11:00:00"]["excluded"], true);
    }

    #[test]
    fn override_key_utc_converts_to_master_local_time() {
        let local = normalise_override_key("2026-05-11T15:00:00Z", Some("America/New_York"));
        assert_eq!(local, "2026-05-11T11:00:00");
    }

    #[test]
    fn override_key_no_tz_strips_z_only() {
        let local = normalise_override_key("2026-05-11T15:00:00Z", None);
        assert_eq!(local, "2026-05-11T15:00:00");
    }

    #[test]
    fn override_key_with_explicit_offset_converts() {
        let local = normalise_override_key("2026-05-11T15:00:00+00:00", Some("Europe/London"));
        assert_eq!(local, "2026-05-11T16:00:00");
    }

    #[test]
    fn override_key_naive_localdatetime_passes_through() {
        let local = normalise_override_key("2026-05-11T15:00:00", Some("America/New_York"));
        assert_eq!(local, "2026-05-11T15:00:00");
    }

    #[test]
    fn override_key_unknown_timezone_strips_offset_only() {
        let local = normalise_override_key("2026-05-11T15:00:00Z", Some("Not/A_Zone"));
        assert_eq!(local, "2026-05-11T15:00:00");
    }

    #[test]
    fn override_key_fractional_seconds_stripped() {
        let local = normalise_override_key("2026-05-11T15:00:00.1234567Z", None);
        assert_eq!(local, "2026-05-11T15:00:00");
    }
}
