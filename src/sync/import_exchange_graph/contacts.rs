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
use crate::exchange_graph::api;
use crate::exchange_graph::contact_map::{ConvertedContact, convert_contact};
use crate::exchange_graph::error::GraphError;
use crate::logging::LEVEL_PROGRESS;
use crate::sync::TypeCounts;
use crate::sync::import_jmap::pool::Pool;

use super::coordinator::{CHUNK_SIZE, GraphCoordinator};
use super::folders::ContactFolder;

fn fetch_photo(
    client: &crate::exchange_graph::client::GraphClient,
    endpoints: &crate::exchange_graph::api::Endpoints,
    id: &str,
) -> Option<ContactPhoto> {
    let url = endpoints.contact_photo(id);
    let resp = client
        .get_with_prefer(&url, crate::exchange_graph::client::Accept::Binary, &[])
        .ok()?;
    if resp.body.is_empty() {
        return None;
    }
    let media_type = sniff_image_type(&resp.body)
        .map(str::to_owned)
        .or_else(|| resp.content_type.clone())
        .unwrap_or_else(|| "application/octet-stream".to_owned());
    Some((resp.body, media_type))
}

fn sniff_image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.starts_with(b"BM") {
        return Some("image/bmp");
    }
    None
}

pub fn reconcile_all(
    conn: &mut Connection,
    ctx: &GraphCoordinator<'_>,
    books: &[ContactFolder],
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let local: HashMap<String, i64> =
        exchange_graph_ids::ids_of_type(conn, ctx.source_id, exchange_graph_ids::CONTACT_CARD)?;
    let mut server_total: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut planned: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut any_failure = false;

    for book in books {
        let url = ctx
            .endpoints
            .contact_folder_contacts_ids(&book.graph_id, ctx.top);
        let ids = match api::collect_all_ids(ctx.client, &url, &[]) {
            Ok(v) => v,
            Err(e) => {
                ctx.logger.warn(&format!(
                    "contact folder {} enumeration failed: {e}",
                    book.graph_id
                ));
                counts.failed += 1;
                any_failure = true;
                continue;
            }
        };
        if ctx.logger.enabled(LEVEL_PROGRESS) {
            eprintln!(
                "graph contact folder {} contacts: {}",
                book.graph_id,
                ids.len()
            );
        }
        for id in &ids {
            server_total.insert(id.clone());
        }
        let mut new_ids: Vec<String> = Vec::new();
        for id in ids {
            if local.contains_key(&id) {
                counts.fetched += 1;
            } else if planned.insert(id.clone()) {
                new_ids.push(id);
            }
        }
        if new_ids.is_empty() {
            continue;
        }
        let fetched = fetch_contacts(ctx, &new_ids);
        let mut converted: Vec<(String, ConvertedContact, Option<ContactPhoto>)> = Vec::new();
        for (graph_id, result, photo) in fetched {
            match result {
                Ok(raw) => match convert_contact(&raw) {
                    Ok(c) => converted.push((graph_id, c, photo)),
                    Err(e) => {
                        counts.failed += 1;
                        ctx.logger
                            .warn(&format!("graph contact {graph_id} convert failed: {e}"));
                    }
                },
                Err(GraphError::Vanished) => counts.skipped += 1,
                Err(e) => {
                    counts.failed += 1;
                    ctx.logger
                        .warn(&format!("graph contact {graph_id} fetch failed: {e}"));
                }
            }
        }
        insert_contacts_chunked(conn, ctx, book.local_id, &converted, counts)?;
    }

    if any_failure {
        ctx.logger.warn(
            "graph contact vanished-cleanup skipped: one or more contact folders failed to enumerate; \
             a clean re-run will reconcile deletions",
        );
    } else {
        delete_vanished(conn, ctx.source_id, &local, &server_total, counts)?;
    }
    Ok(())
}

pub type ContactPhoto = (Vec<u8>, String);

fn fetch_contacts(
    ctx: &GraphCoordinator<'_>,
    ids: &[String],
) -> Vec<(String, Result<Value, GraphError>, Option<ContactPhoto>)> {
    type R = (String, Result<Value, GraphError>, Option<ContactPhoto>);
    let client = ctx.client.clone();
    let endpoints: crate::exchange_graph::api::Endpoints = (*ctx.endpoints).clone();
    let want_photos = ctx.contact_photos;
    let pool: Pool<String, R> = Pool::new(ctx.workers, move |id: String| {
        let url = endpoints.contact(&id);
        let contact = client.get_json_with_prefer(&url, &[]);
        let photo = if want_photos && contact.is_ok() {
            fetch_photo(&client, &endpoints, &id)
        } else {
            None
        };
        (id, contact, photo)
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

fn insert_contacts_chunked(
    conn: &mut Connection,
    ctx: &GraphCoordinator<'_>,
    book_local_id: i64,
    pairs: &[(String, ConvertedContact, Option<ContactPhoto>)],
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    for chunk in pairs.chunks(CHUNK_SIZE) {
        let tx = conn.unchecked_transaction()?;
        for (graph_id, contact, photo) in chunk {
            insert_contact_in_tx(&tx, ctx, book_local_id, graph_id, contact, photo, counts)?;
        }
        tx.commit()?;
    }
    Ok(())
}

fn insert_contact_in_tx(
    tx: &Transaction<'_>,
    ctx: &GraphCoordinator<'_>,
    book_local_id: i64,
    graph_id: &str,
    converted: &ConvertedContact,
    photo: &Option<ContactPhoto>,
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let book_ids = json!([book_local_id]).to_string();
    let mut card = converted.data.clone();
    if let Some((bytes, media_type)) = photo {
        let blob_id = crate::db::blobs::intern_blob(tx, bytes)?;
        if let Some(map) = card.as_object_mut() {
            map.insert(
                "media".to_owned(),
                json!({
                    "photo": {
                        "@type": "Media",
                        "kind": "photo",
                        "@blob": blob_id,
                        "mediaType": media_type,
                    }
                }),
            );
        }
    }
    let data = card.to_string();
    tx.execute(
        "INSERT INTO contact_cards (uid, address_book_ids, data) VALUES (?1, ?2, ?3)",
        params![converted.uid, book_ids, data],
    )?;
    let new_id = tx.last_insert_rowid();
    exchange_graph_ids::insert(
        tx,
        ctx.source_id,
        exchange_graph_ids::CONTACT_CARD,
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
            let result = tx.execute("DELETE FROM contact_cards WHERE id = ?1", params![local_id]);
            match result {
                Ok(_) => {
                    exchange_graph_ids::delete(
                        &tx,
                        source_id,
                        exchange_graph_ids::CONTACT_CARD,
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
    use super::sniff_image_type;

    #[test]
    fn photo_type_comes_from_the_bytes_not_the_header() {
        let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
        assert_eq!(sniff_image_type(png), Some("image/png"));
        assert_eq!(
            sniff_image_type(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some("image/jpeg")
        );
        assert_eq!(sniff_image_type(b"GIF89a..."), Some("image/gif"));
        assert_eq!(sniff_image_type(b"RIFF____WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff_image_type(b"not an image"), None);
    }
}
