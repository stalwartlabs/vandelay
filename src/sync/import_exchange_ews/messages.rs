/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use encodify::base64::LENIENT;
use rusqlite::{Connection, params};
use serde_json::{Value, json};

use crate::db::{blobs, exchange_ews_ids};
use crate::error::Error;
use crate::exchange_ews::parse::{MessageItem, parse_message_item};
use crate::exchange_ews::types::ItemId;
use crate::exchange_ews::xml::ItemShape;
use crate::logging::LEVEL_PROGRESS;
use crate::sync::TypeCounts;
use crate::sync::emailmeta::email_meta_from_blob;
use crate::sync::keys::index_to_json;

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
    for folder in &plan.mail {
        let folder_id = &folder.folder.folder_id;
        let local_folder_id = match exchange_ews_ids::local_for_item(
            conn,
            ctx.source_id,
            exchange_ews_ids::MAILBOX,
            &folder_id.id,
        )
        .map_err(|e| Error::Partial(e.to_string()))?
        {
            Some(id) => id,
            None => continue,
        };
        if let Err(e) = reconcile_one_folder(
            conn,
            ctx,
            folder_id,
            &folder.folder.display_name,
            local_folder_id,
            counts,
        ) {
            ctx.logger.warn(&format!(
                "email folder {:?} failed: {}",
                folder.folder.display_name, e
            ));
            counts.failed += 1;
        }
    }
    Ok(())
}

fn reconcile_one_folder(
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
        exchange_ews_ids::MAILBOX,
        &folder.id,
    )
    .map_err(|e| Error::Partial(e.to_string()))?;
    let outcome = enumerate_folder(ctx, folder, prior.as_deref()).map_err(Error::from)?;
    let local =
        exchange_ews_ids::items_in_folder(conn, ctx.source_id, exchange_ews_ids::EMAIL, &folder.id)
            .map_err(|e| Error::Partial(e.to_string()))?;
    let plan = plan_for(&outcome, &local);
    if ctx.logger.enabled(LEVEL_PROGRESS) {
        eprintln!(
            "EWS folder {:?}: new={} changed={} vanished={} unchanged={}",
            folder_name,
            plan.new.len(),
            plan.present_changed.len(),
            plan.vanished.len(),
            plan.present_unchanged.len()
        );
    }
    let mut to_fetch: Vec<ItemId> = plan.new.clone();
    for (id, _local_id) in &plan.present_changed {
        to_fetch.push(id.clone());
    }
    if !to_fetch.is_empty() {
        let failed_items = for_each_fetched_item(ctx, ItemShape::Message, &to_fetch, |msg| {
            if !msg.success {
                if matches!(
                    msg.response_code,
                    crate::exchange_ews::types::ResponseCode::ItemNotFound
                ) {
                    counts.skipped += 1;
                } else {
                    counts.failed += 1;
                    ctx.logger.warn(&format!(
                        "GetItem (message) error: {} {}",
                        msg.response_code, msg.message_text
                    ));
                }
                return Ok(());
            }
            let parsed = parse_message_item(&msg.inner_xml).map_err(Error::from)?;
            if parsed.id.id.is_empty() {
                counts.failed += 1;
                return Ok(());
            }
            let existing = plan
                .present_changed
                .iter()
                .find(|(id, _)| id.id == parsed.id.id)
                .map(|(_, local)| *local);
            apply_message(
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
        exchange_ews_ids::EMAIL,
        "emails",
        &plan.vanished,
        counts,
    )?;
    if let EnumerationMode::Delta { new_sync_state, .. } = &outcome.mode {
        exchange_ews_ids::set_sync_state(
            conn,
            ctx.source_id,
            exchange_ews_ids::MAILBOX,
            &folder.id,
            new_sync_state,
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
    }
    Ok(())
}

fn apply_message(
    conn: &mut Connection,
    ctx: &ItemRunCtx<'_>,
    item: &MessageItem,
    local_folder_id: i64,
    folder_id: &str,
    existing_local_id: Option<i64>,
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let Some(mime_b64) = item.mime_content.as_ref() else {
        counts.skipped += 1;
        ctx.logger.warn(&format!(
            "message {} has no MimeContent; skipping",
            item.id.id
        ));
        return Ok(());
    };
    let bytes = match LENIENT.decode(mime_b64) {
        Ok(b) => b,
        Err(e) => {
            counts.failed += 1;
            ctx.logger.warn(&format!(
                "message {} MimeContent base64 decode failed: {e}",
                item.id.id
            ));
            return Ok(());
        }
    };
    let (idx, _date_header) = email_meta_from_blob(&bytes);
    let message_match = index_to_json(&idx);
    let received_at = item
        .date_time_received
        .clone()
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_owned());
    let keywords = keyword_array(item);
    let mailbox_ids = json!([local_folder_id]).to_string();

    let tx = conn
        .unchecked_transaction()
        .map_err(|e| Error::Partial(e.to_string()))?;
    let blob_id = blobs::intern_blob(&tx, &bytes).map_err(|e| Error::Partial(e.to_string()))?;
    match existing_local_id {
        Some(id) => {
            tx.execute(
                "UPDATE emails SET blob_id = ?1, received_at = ?2, mailbox_ids = ?3, \
                 keywords = ?4, message_match = ?5 WHERE id = ?6",
                params![
                    blob_id,
                    received_at,
                    mailbox_ids,
                    keywords,
                    message_match,
                    id
                ],
            )
            .map_err(|e| Error::Partial(e.to_string()))?;
            exchange_ews_ids::update_change_key(
                &tx,
                ctx.source_id,
                exchange_ews_ids::EMAIL,
                &item.id.id,
                &item.id.change_key,
            )
            .map_err(|e| Error::Partial(e.to_string()))?;
            counts.fetched += 1;
        }
        None => {
            tx.execute(
                "INSERT INTO emails (blob_id, received_at, mailbox_ids, keywords, message_match) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![blob_id, received_at, mailbox_ids, keywords, message_match],
            )
            .map_err(|e| Error::Partial(e.to_string()))?;
            let new_id = tx.last_insert_rowid();
            exchange_ews_ids::insert(
                &tx,
                ctx.source_id,
                exchange_ews_ids::EMAIL,
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

fn keyword_array(item: &MessageItem) -> String {
    let mut kws: Vec<String> = Vec::new();
    if matches!(item.is_read, Some(true)) {
        kws.push("$seen".to_owned());
    }
    if matches!(item.is_draft, Some(true)) {
        kws.push("$draft".to_owned());
    }
    if matches!(item.is_read_receipt_requested, Some(true)) {
        kws.push("$notified".to_owned());
    }
    if matches!(item.flag_status.as_deref(), Some("Flagged")) {
        kws.push("$flagged".to_owned());
    }
    for cat in &item.categories {
        kws.push(cat.to_ascii_lowercase());
    }
    let value: Value = Value::Array(kws.into_iter().map(Value::String).collect());
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::keyword_array;
    use crate::exchange_ews::parse::MessageItem;

    #[test]
    fn read_draft_flagged_and_categories_map_to_jmap_keywords() {
        let item = MessageItem {
            is_read: Some(true),
            is_draft: Some(true),
            flag_status: Some("Flagged".to_owned()),
            categories: vec!["Red Category".to_owned(), "VIP".to_owned()],
            ..MessageItem::default()
        };
        let kws = keyword_array(&item);
        for expected in ["$seen", "$draft", "$flagged", "red category", "vip"] {
            assert!(kws.contains(expected), "missing {expected} in {kws}");
        }
    }

    #[test]
    fn unread_unflagged_message_has_no_seen_or_flagged() {
        let item = MessageItem {
            is_read: Some(false),
            flag_status: Some("NotFlagged".to_owned()),
            ..MessageItem::default()
        };
        let kws = keyword_array(&item);
        assert!(!kws.contains("$seen"), "unread must not be $seen: {kws}");
        assert!(
            !kws.contains("$flagged"),
            "only FlagStatus=Flagged maps to $flagged: {kws}"
        );
    }
}
