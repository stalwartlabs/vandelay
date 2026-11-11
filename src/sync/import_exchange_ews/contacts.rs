/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::{Connection, params};

use crate::db::exchange_ews_ids;
use crate::error::Error;
use crate::exchange_ews::contact_map::{synthetic_uid, to_jscontact};
use crate::exchange_ews::parse::{ContactItemRaw, parse_contact_item};
use crate::exchange_ews::types::ItemId;
use crate::exchange_ews::xml::ItemShape;
use crate::logging::LEVEL_PROGRESS;
use crate::sync::TypeCounts;

use super::attachments::fetch_contact_photo;
use super::folders::FolderPlan;
use super::items::{
    EnumerationMode, ItemRunCtx, delete_vanished, enumerate_folder, for_each_fetched_item, plan_for,
};

pub fn reconcile_all(
    conn: &mut Connection,
    ctx: &ItemRunCtx<'_>,
    plan: &FolderPlan,
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    for folder in &plan.contacts {
        let folder_id = &folder.folder.folder_id;
        let local_folder_id = match exchange_ews_ids::local_for_item(
            conn,
            ctx.source_id,
            exchange_ews_ids::ADDRESS_BOOK,
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
                "contact folder {:?} failed: {}",
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
        exchange_ews_ids::ADDRESS_BOOK,
        &folder.id,
    )
    .map_err(|e| Error::Partial(e.to_string()))?;
    let outcome = enumerate_folder(ctx, folder, prior.as_deref()).map_err(Error::from)?;
    let local = exchange_ews_ids::items_in_folder(
        conn,
        ctx.source_id,
        exchange_ews_ids::CONTACT_CARD,
        &folder.id,
    )
    .map_err(|e| Error::Partial(e.to_string()))?;
    let plan = plan_for(&outcome, &local);
    if ctx.logger.enabled(LEVEL_PROGRESS) {
        eprintln!(
            "EWS contacts folder {:?}: new={} changed={} vanished={}",
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
        let failed_items = for_each_fetched_item(ctx, ItemShape::Contact, &to_fetch, |msg| {
            if !msg.success {
                if matches!(
                    msg.response_code,
                    crate::exchange_ews::types::ResponseCode::ItemNotFound
                ) {
                    counts.skipped += 1;
                } else {
                    counts.failed += 1;
                    ctx.logger
                        .warn(&format!("GetItem (contact) error: {}", msg.response_code));
                }
                return Ok(());
            }
            let parsed = parse_contact_item(&msg.inner_xml)
                .map_err(|e| Error::Partial(format!("unparsable item: {e}")))?;
            if parsed.id.id.is_empty() {
                counts.failed += 1;
                return Ok(());
            }
            let existing = plan
                .present_changed
                .iter()
                .find(|(id, _)| id.id == parsed.id.id)
                .map(|(_, local)| *local);
            apply_contact(
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
        exchange_ews_ids::CONTACT_CARD,
        "contact_cards",
        &plan.vanished,
        counts,
    )?;
    if let EnumerationMode::Delta { new_sync_state, .. } = &outcome.mode {
        exchange_ews_ids::set_sync_state(
            conn,
            ctx.source_id,
            exchange_ews_ids::ADDRESS_BOOK,
            &folder.id,
            new_sync_state,
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
    }
    Ok(())
}

fn apply_contact(
    conn: &mut Connection,
    ctx: &ItemRunCtx<'_>,
    item: &ContactItemRaw,
    local_folder_id: i64,
    folder_id: &str,
    existing_local_id: Option<i64>,
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let mut card = to_jscontact(item);
    if let Some(photo_att) = item.attachments.iter().find(|a| a.is_contact_photo) {
        match fetch_contact_photo(conn, ctx, &photo_att.attachment_id) {
            Ok(Some((blob_id, media_type))) => {
                let mut media = serde_json::Map::new();
                media.insert(
                    "photo".to_owned(),
                    serde_json::json!({
                        "@type": "Media",
                        "kind": "photo",
                        "@blob": blob_id,
                        "mediaType": media_type,
                    }),
                );
                if let Some(m) = card.as_object_mut() {
                    m.insert("media".to_owned(), serde_json::Value::Object(media));
                }
            }
            Ok(None) => {}
            Err(e) => {
                ctx.logger
                    .warn(&format!("contact photo fetch failed: {e}; continuing"));
            }
        }
    }
    let uid = synthetic_uid(&item.id.id);
    let address_book_ids = serde_json::json!([local_folder_id]).to_string();
    let data = card.to_string();

    let tx = conn
        .unchecked_transaction()
        .map_err(|e| Error::Partial(e.to_string()))?;
    match existing_local_id {
        Some(id) => {
            tx.execute(
                "UPDATE contact_cards SET uid = ?1, address_book_ids = ?2, data = ?3 WHERE id = ?4",
                params![uid, address_book_ids, data, id],
            )
            .map_err(|e| Error::Partial(e.to_string()))?;
            exchange_ews_ids::update_change_key(
                &tx,
                ctx.source_id,
                exchange_ews_ids::CONTACT_CARD,
                &item.id.id,
                &item.id.change_key,
            )
            .map_err(|e| Error::Partial(e.to_string()))?;
            counts.fetched += 1;
        }
        None => {
            tx.execute(
                "INSERT INTO contact_cards (uid, address_book_ids, data) VALUES (?1, ?2, ?3)",
                params![uid, address_book_ids, data],
            )
            .map_err(|e| Error::Partial(e.to_string()))?;
            let new_id = tx.last_insert_rowid();
            exchange_ews_ids::insert(
                &tx,
                ctx.source_id,
                exchange_ews_ids::CONTACT_CARD,
                folder_id,
                &item.id.id,
                &item.id.change_key,
                new_id,
            )
            .map_err(|e| Error::Partial(e.to_string()))?;
            counts.created += 1;
        }
    }
    tx.commit().map_err(|e| Error::Partial(e.to_string()))?;
    Ok(())
}
