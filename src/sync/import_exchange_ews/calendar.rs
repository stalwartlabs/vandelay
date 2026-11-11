/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::{Connection, params};
use serde_json::Value;

use crate::db::exchange_ews_ids;
use crate::error::Error;
use crate::exchange::date::is_outlook_no_date;
use crate::exchange_ews::calendar_map::to_jscalendar_with_exceptions;
use crate::exchange_ews::parse::{CalendarItemRaw, parse_calendar_item};
use crate::exchange_ews::types::{CalendarItemType, ItemId};
use crate::exchange_ews::xml::ItemShape;
use crate::logging::LEVEL_PROGRESS;
use crate::sync::TypeCounts;

use super::attachments::{fetch_attachments, intern_attachment};
use super::folders::FolderPlan;
use super::items::{
    EnumerationMode, ItemRunCtx, delete_vanished, enumerate_folder, for_each_fetched_item,
    get_items, plan_for,
};

pub fn reconcile_all(
    conn: &mut Connection,
    ctx: &ItemRunCtx<'_>,
    plan: &FolderPlan,
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    for folder in &plan.calendar {
        let folder_id = &folder.folder.folder_id;
        let local_folder_id = match exchange_ews_ids::local_for_item(
            conn,
            ctx.source_id,
            exchange_ews_ids::CALENDAR,
            &folder_id.id,
        )
        .map_err(|e| Error::Partial(e.to_string()))?
        {
            Some(id) => id,
            None => continue,
        };
        if let Err(e) = reconcile_one(
            conn,
            ctx,
            folder_id,
            &folder.folder.display_name,
            local_folder_id,
            counts,
        ) {
            ctx.logger.warn(&format!(
                "calendar folder {:?} failed: {}",
                folder.folder.display_name, e
            ));
            counts.failed += 1;
        }
    }
    Ok(())
}

fn reconcile_one(
    conn: &mut Connection,
    ctx: &ItemRunCtx<'_>,
    folder: &crate::exchange_ews::types::FolderId,
    folder_name: &str,
    local_folder_id: i64,
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let prior = exchange_ews_ids::get_sync_state(
        conn,
        ctx.source_id,
        exchange_ews_ids::CALENDAR,
        &folder.id,
    )
    .map_err(|e| Error::Partial(e.to_string()))?;
    let mut outcome = enumerate_folder(ctx, folder, prior.as_deref()).map_err(Error::from)?;
    outcome.items.retain(|s| {
        matches!(
            s.element.to_ascii_lowercase().as_str(),
            "calendaritem" | "item"
        )
    });
    let local = exchange_ews_ids::items_in_folder(
        conn,
        ctx.source_id,
        exchange_ews_ids::CALENDAR_EVENT,
        &folder.id,
    )
    .map_err(|e| Error::Partial(e.to_string()))?;
    let plan = plan_for(&outcome, &local);
    if ctx.logger.enabled(LEVEL_PROGRESS) {
        eprintln!(
            "EWS calendar folder {:?}: new={} changed={} vanished={}",
            folder_name,
            plan.new.len(),
            plan.present_changed.len(),
            plan.vanished.len()
        );
    }
    let mut to_fetch: Vec<ItemId> = plan.new.clone();
    for (id, _) in &plan.present_changed {
        to_fetch.push(id.clone());
    }
    if !to_fetch.is_empty() {
        let failed_items = for_each_fetched_item(ctx, ItemShape::CalendarItem, &to_fetch, |msg| {
            if !msg.success {
                if matches!(
                    msg.response_code,
                    crate::exchange_ews::types::ResponseCode::ItemNotFound
                ) {
                    counts.skipped += 1;
                } else {
                    counts.failed += 1;
                    ctx.logger
                        .warn(&format!("GetItem (calendar) error: {}", msg.response_code));
                }
                return Ok(());
            }
            let parsed = parse_calendar_item(&msg.inner_xml)
                .map_err(|e| Error::Partial(format!("unparsable item: {e}")))?;
            if parsed.id.id.is_empty() {
                counts.failed += 1;
                return Ok(());
            }
            if matches!(
                parsed.calendar_item_type,
                Some(CalendarItemType::Occurrence) | Some(CalendarItemType::Exception)
            ) || parsed.start.as_deref().is_some_and(is_outlook_no_date)
            {
                counts.skipped += 1;
                return Ok(());
            }
            let existing = plan
                .present_changed
                .iter()
                .find(|(id, _)| id.id == parsed.id.id)
                .map(|(_, local)| *local);
            apply_event(
                conn,
                ctx,
                &parsed,
                local_folder_id,
                &folder.id,
                existing,
                counts,
            )
        })?;
        counts.failed += failed_items;
    }
    delete_vanished(
        conn,
        ctx.source_id,
        exchange_ews_ids::CALENDAR_EVENT,
        "calendar_events",
        &plan.vanished,
        counts,
    )?;
    if let EnumerationMode::Delta { new_sync_state, .. } = &outcome.mode {
        exchange_ews_ids::set_sync_state(
            conn,
            ctx.source_id,
            exchange_ews_ids::CALENDAR,
            &folder.id,
            new_sync_state,
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
    }
    Ok(())
}

fn apply_event(
    conn: &mut Connection,
    ctx: &ItemRunCtx<'_>,
    raw: &CalendarItemRaw,
    local_folder_id: i64,
    folder_id: &str,
    existing_local_id: Option<i64>,
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let modified_full = fetch_modified_occurrences(ctx, raw);
    let mut built = to_jscalendar_with_exceptions(raw, &modified_full, ctx.owner_smtp);
    attach_calendar_links(conn, ctx, raw, &mut built.data)?;
    let calendar_ids = serde_json::json!([local_folder_id]).to_string();
    let data = built.data.to_string();
    let is_draft = if built.is_draft { 1 } else { 0 };
    let use_default_alerts = if built.use_default_alerts { 1 } else { 0 };

    let tx = conn
        .unchecked_transaction()
        .map_err(|e| Error::Partial(e.to_string()))?;
    match existing_local_id {
        Some(id) => {
            tx.execute(
                "UPDATE calendar_events SET calendar_ids = ?1, is_draft = ?2, \
                 use_default_alerts = ?3, data = ?4, data_type = 'Event' WHERE id = ?5",
                params![calendar_ids, is_draft, use_default_alerts, data, id],
            )
            .map_err(|e| Error::Partial(e.to_string()))?;
            exchange_ews_ids::update_change_key(
                &tx,
                ctx.source_id,
                exchange_ews_ids::CALENDAR_EVENT,
                &raw.id.id,
                &raw.id.change_key,
            )
            .map_err(|e| Error::Partial(e.to_string()))?;
            counts.fetched += 1;
        }
        None => {
            tx.execute(
                "INSERT INTO calendar_events (calendar_ids, is_draft, use_default_alerts, data, data_type) \
                 VALUES (?1, ?2, ?3, ?4, 'Event')",
                params![calendar_ids, is_draft, use_default_alerts, data],
            )
            .map_err(|e| Error::Partial(e.to_string()))?;
            let new_id = tx.last_insert_rowid();
            exchange_ews_ids::insert(
                &tx,
                ctx.source_id,
                exchange_ews_ids::CALENDAR_EVENT,
                folder_id,
                &raw.id.id,
                &raw.id.change_key,
                new_id,
            )
            .map_err(|e| Error::Partial(e.to_string()))?;
            counts.created += 1;
        }
    }
    tx.commit().map_err(|e| Error::Partial(e.to_string()))?;
    Ok(())
}

fn fetch_modified_occurrences(ctx: &ItemRunCtx<'_>, raw: &CalendarItemRaw) -> Vec<CalendarItemRaw> {
    let ids: Vec<ItemId> = raw
        .modified_occurrences
        .iter()
        .filter(|o| !o.item_id.id.is_empty())
        .map(|o| o.item_id.clone())
        .collect();
    if ids.is_empty() {
        return Vec::new();
    }
    match get_items(ctx, ItemShape::CalendarItem, &ids) {
        Ok(outcome) => outcome
            .messages
            .iter()
            .filter(|m| m.success)
            .filter_map(|m| parse_calendar_item(&m.inner_xml).ok())
            .filter(|occ| !occ.id.id.is_empty())
            .collect(),
        Err(e) => {
            ctx.logger.warn(&format!(
                "GetItem (calendar exceptions) failed: {e}; overrides keep time-only changes"
            ));
            Vec::new()
        }
    }
}

fn attach_calendar_links(
    conn: &mut Connection,
    ctx: &ItemRunCtx<'_>,
    raw: &CalendarItemRaw,
    data: &mut Value,
) -> Result<(), Error> {
    let refs: Vec<_> = raw
        .attachments
        .iter()
        .filter(|a| !a.is_item_attachment && !a.attachment_id.is_empty())
        .collect();
    if refs.is_empty() {
        return Ok(());
    }
    let ids: Vec<&str> = refs.iter().map(|a| a.attachment_id.as_str()).collect();
    let fetched = match fetch_attachments(ctx, &ids) {
        Ok(f) => f,
        Err(e) => {
            ctx.logger.warn(&format!(
                "calendar attachments fetch failed: {e}; continuing without enclosures"
            ));
            return Ok(());
        }
    };
    let mut links = serde_json::Map::new();
    for (idx, att) in (1u32..).zip(fetched) {
        let blob_id = intern_attachment(conn, &att.bytes)?;
        let declared = refs
            .iter()
            .find(|r| r.attachment_id == att.attachment_id)
            .and_then(|r| r.content_type.as_deref());
        let name = refs
            .iter()
            .find(|r| r.attachment_id == att.attachment_id)
            .and_then(|r| r.name.clone());
        let key = idx.to_string();
        let mut entry = serde_json::Map::new();
        entry.insert("@type".to_owned(), Value::String("Link".to_owned()));
        entry.insert("@blob".to_owned(), Value::from(blob_id));
        entry.insert(
            "contentType".to_owned(),
            Value::String(content_type_or_from(declared, &att.media_type)),
        );
        if let Some(n) = name {
            entry.insert("title".to_owned(), Value::String(n));
        }
        entry.insert("rel".to_owned(), Value::String("enclosure".to_owned()));
        links.insert(key, Value::Object(entry));
    }
    if links.is_empty() {
        return Ok(());
    }
    if let Some(obj) = data.as_object_mut() {
        if let Some(existing) = obj.get_mut("links").and_then(Value::as_object_mut) {
            for (k, v) in links {
                existing.insert(k, v);
            }
        } else {
            obj.insert("links".to_owned(), Value::Object(links));
        }
    }
    Ok(())
}

fn content_type_or_from(declared: Option<&str>, fallback: &str) -> String {
    declared
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| fallback.to_owned())
}
