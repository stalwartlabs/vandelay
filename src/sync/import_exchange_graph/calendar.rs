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
use crate::exchange_graph::api::{self, PREFER_BODY_HTML, PREFER_BODY_TEXT, PREFER_TIMEZONE_UTC};
use crate::exchange_graph::calendar_map::{
    ConvertedEvent, EventType, classify_event_type, convert_event,
};
use crate::exchange_graph::error::GraphError;
use crate::exchange_graph::types::EventBodyFormat;
use crate::logging::LEVEL_PROGRESS;
use crate::sync::TypeCounts;
use crate::sync::pool::Pool;

use super::coordinator::{CHUNK_SIZE, GraphCoordinator};
use super::folders::CalendarFolder;

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
        for stub in &stubs {
            match classify_event_type(stub) {
                EventType::Occurrence => occurrence_count += 1,
                _ => {
                    if let Some(id) = stub.get("id").and_then(Value::as_str) {
                        server_total.insert(id.to_owned());
                        if !local.contains_key(id) && planned.insert(id.to_owned()) {
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
        if want_ids.is_empty() {
            continue;
        }
        let fetched = fetch_events(ctx, &want_ids);
        let mut masters: Vec<(String, ConvertedEvent)> = Vec::new();
        let mut exceptions: Vec<(String, ConvertedEvent)> = Vec::new();
        for (graph_id, result) in fetched {
            match result {
                Ok(raw) => match convert_event(&raw, None) {
                    Ok(c) => match c.event_type {
                        EventType::Exception => exceptions.push((graph_id, c)),
                        _ => masters.push((graph_id, c)),
                    },
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

        let mut master_by_graph_id: HashMap<String, ConvertedEvent> = masters.into_iter().collect();
        for (ex_graph_id, ex) in exceptions {
            let Some(master_graph_id) = ex.series_master_id.clone() else {
                ctx.logger.warn(&format!(
                    "graph exception event {ex_graph_id} has no seriesMasterId; dropping"
                ));
                counts.skipped += 1;
                continue;
            };
            if let Some(master) = master_by_graph_id.get_mut(&master_graph_id) {
                merge_exception_into(master, &ex);
            } else if local.contains_key(&master_graph_id) {
                merge_exception_into_existing(conn, ctx, &master_graph_id, &ex, counts);
            } else {
                counts.skipped += 1;
                ctx.logger.warn(&format!(
                    "graph exception {ex_graph_id} references missing master {master_graph_id}; orphaned"
                ));
            }
        }

        let pairs: Vec<(String, ConvertedEvent)> = master_by_graph_id.into_iter().collect();
        insert_events_chunked(conn, ctx, cal.local_id, &pairs, counts)?;
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
) -> Vec<(String, Result<Value, GraphError>)> {
    type R = (String, Result<Value, GraphError>);
    let client = ctx.client.clone();
    let endpoints: crate::exchange_graph::api::Endpoints = (*ctx.endpoints).clone();
    let body_prefer = match ctx.event_body_format {
        EventBodyFormat::Text => PREFER_BODY_TEXT,
        EventBodyFormat::Html => PREFER_BODY_HTML,
    };
    let prefer: Vec<String> = vec![PREFER_TIMEZONE_UTC.to_owned(), body_prefer.to_owned()];
    let pool: Pool<String, R> = Pool::new(ctx.workers, move |id: String| {
        let url = endpoints.event(&id);
        let prefer_refs: Vec<&str> = prefer.iter().map(String::as_str).collect();
        let result = client.get_json_with_prefer(&url, &prefer_refs);
        (id, result)
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

fn merge_exception_into(master: &mut ConvertedEvent, ex: &ConvertedEvent) {
    let Some(raw_key) = ex.original_start.as_deref() else {
        return;
    };
    let master_tz = master
        .data
        .get("timeZone")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let key = normalise_override_key(raw_key, master_tz.as_deref());
    let Value::Object(map) = &mut master.data else {
        return;
    };
    let overrides = map
        .entry("recurrenceOverrides".to_owned())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let Value::Object(overrides) = overrides else {
        return;
    };
    overrides.insert(key, ex.data.clone());
}

fn merge_exception_into_existing(
    conn: &mut Connection,
    ctx: &GraphCoordinator<'_>,
    master_graph_id: &str,
    ex: &ConvertedEvent,
    counts: &mut TypeCounts,
) {
    let local_id = match exchange_graph_ids::local_for_graph_id(
        conn,
        ctx.source_id,
        exchange_graph_ids::CALENDAR_EVENT,
        master_graph_id,
    ) {
        Ok(Some(id)) => id,
        Ok(None) => {
            counts.skipped += 1;
            return;
        }
        Err(e) => {
            counts.failed += 1;
            ctx.logger.warn(&format!(
                "graph exception merge: lookup of master {master_graph_id} failed: {e}"
            ));
            return;
        }
    };
    let Some(raw_key) = ex.original_start.as_deref() else {
        counts.skipped += 1;
        return;
    };
    if let Err(e) = merge_persisted_master(conn, local_id, raw_key, &ex.data) {
        counts.failed += 1;
        ctx.logger.warn(&format!(
            "graph exception merge into stored master {master_graph_id} failed: {e}"
        ));
    } else {
        counts.fetched += 1;
    }
}

fn merge_persisted_master(
    conn: &Connection,
    local_id: i64,
    raw_key: &str,
    ex_data: &Value,
) -> Result<(), String> {
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    let row: String = tx
        .query_row(
            "SELECT data FROM calendar_events WHERE id = ?1",
            params![local_id],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    let mut data: Value = serde_json::from_str(&row).map_err(|e| e.to_string())?;
    let master_tz = data
        .get("timeZone")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let key = normalise_override_key(raw_key, master_tz.as_deref());
    if let Value::Object(map) = &mut data {
        let entry = map
            .entry("recurrenceOverrides".to_owned())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Value::Object(overrides) = entry {
            overrides.insert(key, ex_data.clone());
        }
    }
    tx.execute(
        "UPDATE calendar_events SET data = ?1 WHERE id = ?2",
        params![data.to_string(), local_id],
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
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
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    for chunk in pairs.chunks(CHUNK_SIZE) {
        let tx = conn.unchecked_transaction()?;
        for (graph_id, event) in chunk {
            insert_event_in_tx(&tx, ctx, calendar_local_id, graph_id, event, counts)?;
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
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let calendar_ids = json!([calendar_local_id]).to_string();
    let data = event.data.to_string();
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
