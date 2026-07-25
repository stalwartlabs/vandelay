/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, params};

use crate::dav::client::{DavClient, MultiStatus};
use crate::dav::href::{Href, join_absolute, normalise};
use crate::dav::parse::DavResponse;
use crate::dav::xml;
use crate::db::dav_ids;
use crate::error::Error;
use crate::jmap::error::JmapError;
use crate::logging::{LEVEL_PROGRESS, Logger};
use crate::sync::TypeCounts;
use crate::sync::pool::Pool;

use super::calcard;

pub struct ItemRunCtx<'a> {
    pub client: &'a DavClient,
    pub source_id: i64,
    pub base_url: &'a str,
    pub multiget_batch: usize,
    pub dav_connections: usize,
    pub logger: Logger,
}

struct PerCollectionCtx<'a> {
    run: &'a ItemRunCtx<'a>,
    kind: ItemKind,
    collection_href: &'a str,
    container_local_id: i64,
    absolute_url: &'a str,
}

pub fn reconcile_calendar_events(
    conn: &mut Connection,
    ctx: &ItemRunCtx<'_>,
    collection_href: &str,
    calendar_local_id: i64,
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let absolute = absolute_collection_url(ctx.base_url, collection_href)?;
    let server_items = enumerate_items(ctx.client, &absolute)?;
    let pcc = PerCollectionCtx {
        run: ctx,
        kind: ItemKind::CalendarEvent,
        collection_href,
        container_local_id: calendar_local_id,
        absolute_url: &absolute,
    };
    reconcile_items_generic(conn, &pcc, &server_items, counts)
}

pub fn reconcile_contact_cards(
    conn: &mut Connection,
    ctx: &ItemRunCtx<'_>,
    collection_href: &str,
    address_book_local_id: i64,
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let absolute = absolute_collection_url(ctx.base_url, collection_href)?;
    let server_items = enumerate_items(ctx.client, &absolute)?;
    let pcc = PerCollectionCtx {
        run: ctx,
        kind: ItemKind::ContactCard,
        collection_href,
        container_local_id: address_book_local_id,
        absolute_url: &absolute,
    };
    reconcile_items_generic(conn, &pcc, &server_items, counts)
}

#[derive(Debug, Clone, Copy)]
enum ItemKind {
    CalendarEvent,
    ContactCard,
}

impl ItemKind {
    fn type_name(self) -> &'static str {
        match self {
            ItemKind::CalendarEvent => dav_ids::CALENDAR_EVENT,
            ItemKind::ContactCard => dav_ids::CONTACT_CARD,
        }
    }
    fn data_query(self) -> fn(&[Href]) -> String {
        match self {
            ItemKind::CalendarEvent => xml::calendar_multiget,
            ItemKind::ContactCard => xml::addressbook_multiget,
        }
    }
    fn data_field(self) -> &'static str {
        match self {
            ItemKind::CalendarEvent => "calendar_data",
            ItemKind::ContactCard => "address_data",
        }
    }
}

#[derive(Debug, Clone)]
struct ServerItem {
    href: String,
    etag: String,
}

fn enumerate_items(client: &DavClient, url: &str) -> Result<Vec<ServerItem>, Error> {
    let body = xml::propfind_dav_items();
    let ms = client
        .propfind_responses(url, 1, &body, url)
        .map_err(Error::from)?;
    if ms.status >= 400 {
        return Err(Error::Partial(format!(
            "enumerate {url}: http {}",
            ms.status
        )));
    }
    let self_href = normalise(url, "")
        .map(|h| h.into_string())
        .unwrap_or_default();
    let mut out = Vec::new();
    for r in ms.responses {
        if r.href.as_str() == self_href {
            continue;
        }
        if r.props.is_collection {
            continue;
        }
        out.push(ServerItem {
            href: r.href.into_string(),
            etag: r.props.etag.unwrap_or_default(),
        });
    }
    Ok(out)
}

struct MultigetJob {
    body: String,
    hrefs: Vec<String>,
}

struct MultigetReply {
    hrefs: Vec<String>,
    result: Result<MultiStatus, JmapError>,
}

struct GetJob {
    url: String,
    item: ServerItem,
}

struct GetReply {
    item: ServerItem,
    result: Result<GetBody, JmapError>,
}

struct GetBody {
    status: u16,
    bytes: Vec<u8>,
    etag: Option<String>,
}

fn reconcile_items_generic(
    conn: &mut Connection,
    pcc: &PerCollectionCtx<'_>,
    server_items: &[ServerItem],
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let type_name = pcc.kind.type_name();
    let local_rows =
        dav_ids::items_in_collection(conn, pcc.run.source_id, type_name, pcc.collection_href)
            .map_err(|e| Error::Partial(e.to_string()))?;
    let local_map: HashMap<String, (String, i64)> = local_rows
        .iter()
        .map(|r| (r.item_href.clone(), (r.etag.clone(), r.local_id)))
        .collect();
    let server_set: HashSet<&str> = server_items.iter().map(|s| s.href.as_str()).collect();

    let mut to_fetch: Vec<ServerItem> = Vec::new();
    for s in server_items {
        match local_map.get(&s.href) {
            None => to_fetch.push(s.clone()),
            Some((local_etag, _)) => {
                if s.etag.is_empty() || s.etag != *local_etag {
                    to_fetch.push(s.clone());
                }
            }
        }
    }

    let vanished: Vec<String> = local_map
        .keys()
        .filter(|h| !server_set.contains(h.as_str()))
        .cloned()
        .collect();

    if !to_fetch.is_empty() {
        let fallback = run_multiget_pool(conn, pcc, &to_fetch, &local_map, counts)?;
        if !fallback.is_empty() {
            run_get_pool(conn, pcc, &fallback, &local_map, counts)?;
        }
    }

    for href in &vanished {
        let local_id = match local_map.get(href) {
            Some((_, id)) => *id,
            None => continue,
        };
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| Error::Partial(e.to_string()))?;
        let table = match pcc.kind {
            ItemKind::CalendarEvent => "calendar_events",
            ItemKind::ContactCard => "contact_cards",
        };
        tx.execute(
            &format!("DELETE FROM {table} WHERE id = ?1"),
            params![local_id],
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
        dav_ids::delete_item(&tx, pcc.run.source_id, type_name, href)
            .map_err(|e| Error::Partial(e.to_string()))?;
        tx.commit().map_err(|e| Error::Partial(e.to_string()))?;
        counts.deleted += 1;
    }

    if pcc.run.logger.enabled(LEVEL_PROGRESS) {
        eprintln!(
            "items in {}: fetched={} deleted={} failed={}",
            pcc.collection_href, counts.fetched, counts.deleted, counts.failed
        );
    }
    Ok(())
}

fn run_multiget_pool(
    conn: &mut Connection,
    pcc: &PerCollectionCtx<'_>,
    to_fetch: &[ServerItem],
    local_map: &HashMap<String, (String, i64)>,
    counts: &mut TypeCounts,
) -> Result<Vec<ServerItem>, Error> {
    let logger = pcc.run.logger;
    let absolute_url = pcc.absolute_url;
    let batch_size = pcc.run.multiget_batch.max(1);
    let workers = pcc.run.dav_connections.clamp(1, 8);
    let by_href: HashMap<String, ServerItem> = to_fetch
        .iter()
        .map(|s| (s.href.clone(), s.clone()))
        .collect();
    let client_for_pool = pcc.run.client.clone();
    let url_for_pool = absolute_url.to_owned();
    let pool: Pool<MultigetJob, MultigetReply> = Pool::new(workers, move |job: MultigetJob| {
        let result = client_for_pool.report_responses(&url_for_pool, 1, &job.body, &url_for_pool);
        MultigetReply {
            hrefs: job.hrefs,
            result,
        }
    });
    let mut submitted: usize = 0;
    for chunk in to_fetch.chunks(batch_size) {
        let hrefs: Vec<Href> = chunk
            .iter()
            .map(|s| Href::from_normalised(s.href.clone()))
            .collect();
        let chunk_hrefs: Vec<String> = chunk.iter().map(|s| s.href.clone()).collect();
        pool.submit(MultigetJob {
            body: (pcc.kind.data_query())(&hrefs),
            hrefs: chunk_hrefs,
        });
        submitted += 1;
    }

    let mut fallback: Vec<ServerItem> = Vec::new();
    for _ in 0..submitted {
        let reply = match pool.results().recv() {
            Ok(r) => r,
            Err(_) => break,
        };
        match reply.result {
            Err(JmapError::HttpStatus { status, .. }) if status == 405 || status == 501 => {
                logger.warn(&format!(
                    "multiget {absolute_url} returned {status}; server may not implement multiget, falling back to per-item GET"
                ));
                fallback.extend(reply.hrefs.iter().filter_map(|h| by_href.get(h).cloned()));
            }
            Err(e) => {
                logger.warn(&format!(
                    "multiget {absolute_url}: {e}; falling back to per-item GET"
                ));
                fallback.extend(reply.hrefs.iter().filter_map(|h| by_href.get(h).cloned()));
            }
            Ok(ms) if ms.status >= 400 => {
                logger.warn(&format!(
                    "multiget {absolute_url} returned {}; chunk failed",
                    ms.status
                ));
                counts.failed += reply.hrefs.len() as u64;
            }
            Ok(ms) => {
                let requested: HashSet<&str> = reply.hrefs.iter().map(String::as_str).collect();
                let mut tx = conn
                    .unchecked_transaction()
                    .map_err(|e| Error::Partial(e.to_string()))?;
                let mut pending: usize = 0;
                for r in ms.responses {
                    if !requested.contains(r.href.as_str()) {
                        logger.warn(&format!(
                            "multiget {absolute_url}: server returned unsolicited href {}; skipping",
                            r.href.as_str()
                        ));
                        continue;
                    }
                    handle_multiget_response(&tx, pcc, &r, local_map, counts);
                    pending += 1;
                    if pending >= COMMIT_BATCH {
                        tx.commit().map_err(|e| Error::Partial(e.to_string()))?;
                        tx = conn
                            .unchecked_transaction()
                            .map_err(|e| Error::Partial(e.to_string()))?;
                        pending = 0;
                    }
                }
                tx.commit().map_err(|e| Error::Partial(e.to_string()))?;
            }
        }
    }
    drop(pool);
    Ok(fallback)
}

fn run_get_pool(
    conn: &mut Connection,
    pcc: &PerCollectionCtx<'_>,
    items: &[ServerItem],
    local_map: &HashMap<String, (String, i64)>,
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let logger = pcc.run.logger;
    let absolute_url = pcc.absolute_url;
    let workers = pcc.run.dav_connections.clamp(1, 8);
    let client_for_pool = pcc.run.client.clone();
    let pool: Pool<GetJob, GetReply> = Pool::new(workers, move |job: GetJob| {
        let r = client_for_pool.get(&job.url).map(|resp| GetBody {
            status: resp.status,
            bytes: resp.body,
            etag: resp.etag,
        });
        GetReply {
            item: job.item,
            result: r,
        }
    });

    let mut submitted: usize = 0;
    for item in items {
        let url = match absolute_item_url(absolute_url, &item.href) {
            Ok(u) => u,
            Err(e) => {
                logger.warn(&format!("item url {}: {e}", item.href));
                counts.failed += 1;
                continue;
            }
        };
        pool.submit(GetJob {
            url,
            item: item.clone(),
        });
        submitted += 1;
    }

    let mut tx = conn
        .unchecked_transaction()
        .map_err(|e| Error::Partial(e.to_string()))?;
    let mut pending: usize = 0;
    for _ in 0..submitted {
        let reply = match pool.results().recv() {
            Ok(r) => r,
            Err(_) => break,
        };
        match reply.result {
            Err(e) => {
                logger.warn(&format!("GET {}: {e}", reply.item.href));
                counts.failed += 1;
            }
            Ok(body) if body.status == 404 || body.status == 410 => {}
            Ok(body) if body.status >= 400 => {
                logger.warn(&format!("GET {}: http {}", reply.item.href, body.status));
                counts.failed += 1;
            }
            Ok(body) => {
                let etag = body.etag.unwrap_or_else(|| reply.item.etag.clone());
                let raw = match std::str::from_utf8(&body.bytes) {
                    Ok(s) => s.to_owned(),
                    Err(e) => {
                        logger.warn(&format!("GET {}: non-utf8 body: {e}", reply.item.href));
                        counts.failed += 1;
                        continue;
                    }
                };
                if let Err(e) =
                    insert_or_update_tx(&tx, pcc, &reply.item.href, &etag, &raw, local_map)
                {
                    logger.warn(&format!("item {}: {e}", reply.item.href));
                    counts.failed += 1;
                } else {
                    counts.fetched += 1;
                    pending += 1;
                    if pending >= COMMIT_BATCH {
                        tx.commit().map_err(|e| Error::Partial(e.to_string()))?;
                        tx = conn
                            .unchecked_transaction()
                            .map_err(|e| Error::Partial(e.to_string()))?;
                        pending = 0;
                    }
                }
            }
        }
    }
    tx.commit().map_err(|e| Error::Partial(e.to_string()))?;
    drop(pool);
    Ok(())
}

const COMMIT_BATCH: usize = 64;

fn handle_multiget_response(
    tx: &rusqlite::Transaction<'_>,
    pcc: &PerCollectionCtx<'_>,
    response: &DavResponse,
    local_map: &HashMap<String, (String, i64)>,
    counts: &mut TypeCounts,
) {
    let logger = pcc.run.logger;
    if response_indicates_vanished(response) {
        return;
    }
    let item_href = response.href.as_str().to_owned();
    let etag = response.props.etag.clone().unwrap_or_default();
    let data = match pcc.kind {
        ItemKind::CalendarEvent => response.props.calendar_data.as_deref(),
        ItemKind::ContactCard => response.props.address_data.as_deref(),
    };
    let Some(raw) = data else {
        logger.warn(&format!(
            "multiget {item_href}: missing {} payload",
            pcc.kind.data_field()
        ));
        counts.failed += 1;
        return;
    };
    if let Err(e) = insert_or_update_tx(tx, pcc, &item_href, &etag, raw, local_map) {
        logger.warn(&format!("item {item_href}: {e}"));
        counts.failed += 1;
    } else {
        counts.fetched += 1;
    }
}

fn insert_or_update_tx(
    tx: &rusqlite::Transaction<'_>,
    pcc: &PerCollectionCtx<'_>,
    item_href: &str,
    etag: &str,
    raw: &str,
    local_map: &HashMap<String, (String, i64)>,
) -> Result<(), Error> {
    let existing = local_map.get(item_href).map(|(_, id)| *id);
    match pcc.kind {
        ItemKind::CalendarEvent => insert_or_update_event(
            tx,
            pcc.container_local_id,
            existing,
            raw,
            item_href,
            pcc.run.logger,
        )?,
        ItemKind::ContactCard => {
            insert_or_update_card(tx, pcc.container_local_id, existing, raw, item_href)?
        }
    };
    if existing.is_none() {
        let new_local = tx.last_insert_rowid();
        dav_ids::insert(
            tx,
            pcc.run.source_id,
            pcc.kind.type_name(),
            pcc.collection_href,
            item_href,
            etag,
            new_local,
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
    } else {
        dav_ids::update_etag(tx, pcc.run.source_id, pcc.kind.type_name(), item_href, etag)
            .map_err(|e| Error::Partial(e.to_string()))?;
    }
    Ok(())
}

fn insert_or_update_event(
    tx: &rusqlite::Transaction<'_>,
    calendar_local: i64,
    existing: Option<i64>,
    raw: &str,
    item_href: &str,
    logger: Logger,
) -> Result<(), Error> {
    let entries = calcard::ical_to_jscalendar_entries(raw)
        .map_err(|e| Error::Partial(format!("iCalendar parse: {e}")))?;
    if entries.len() > 1 && logger.enabled(crate::logging::LEVEL_DEFAULT) {
        logger.warn(&format!(
            "{item_href}: iCalendar resource has {} entries; storing the first ({}), \
             others dropped (recurrence overrides on the master are preserved)",
            entries.len(),
            entries[0].data_type.as_column(),
        ));
    }
    let mut first = entries
        .into_iter()
        .next()
        .ok_or_else(|| Error::Partial("iCalendar contained no parseable entries".to_owned()))?;
    let (is_draft, use_default_alerts, _uid) =
        calcard::strip_extracted_fields_from_event(&mut first.data);
    let data_type = first.data_type.as_column();
    let calendar_ids = format!("[{calendar_local}]");
    if let Some(local) = existing {
        tx.execute(
            "UPDATE calendar_events SET calendar_ids = ?1, is_draft = ?2,
                                          use_default_alerts = ?3, data = ?4, data_type = ?5
             WHERE id = ?6",
            params![
                calendar_ids,
                is_draft as i64,
                use_default_alerts as i64,
                first.data.to_string(),
                data_type,
                local,
            ],
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
    } else {
        tx.execute(
            "INSERT INTO calendar_events (calendar_ids, is_draft, use_default_alerts, data, data_type)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                calendar_ids,
                is_draft as i64,
                use_default_alerts as i64,
                first.data.to_string(),
                data_type,
            ],
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
    }
    Ok(())
}

fn insert_or_update_card(
    tx: &rusqlite::Transaction<'_>,
    address_book_local: i64,
    existing: Option<i64>,
    raw: &str,
    item_href: &str,
) -> Result<(), Error> {
    let card = calcard::vcard_to_jscontact(raw, item_href)
        .map_err(|e| Error::Partial(format!("vCard parse: {e}")))?;
    let address_book_ids = format!("[{address_book_local}]");
    if let Some(local) = existing {
        tx.execute(
            "UPDATE contact_cards SET uid = ?1, address_book_ids = ?2, data = ?3
             WHERE id = ?4",
            params![card.uid, address_book_ids, card.data.to_string(), local],
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
    } else {
        tx.execute(
            "INSERT INTO contact_cards (uid, address_book_ids, data) VALUES (?1, ?2, ?3)",
            params![card.uid, address_book_ids, card.data.to_string()],
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
    }
    Ok(())
}

fn response_indicates_vanished(response: &DavResponse) -> bool {
    if let Some(status) = response.status
        && (status == 404 || status == 410)
    {
        return true;
    }
    response
        .propstat_errors
        .iter()
        .any(|s| *s == 404 || *s == 410)
}

fn absolute_collection_url(base_url: &str, href: &str) -> Result<String, Error> {
    join_absolute(base_url, href).map_err(|e| Error::Partial(e.to_string()))
}

fn absolute_item_url(collection_url: &str, item_href: &str) -> Result<String, Error> {
    join_absolute(collection_url, item_href).map_err(|e| Error::Partial(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_collection_url_joins_path() {
        let u = absolute_collection_url("https://x/dav/", "/dav/cal/u/d/").unwrap();
        assert_eq!(u, "https://x/dav/cal/u/d/");
    }

    #[test]
    fn absolute_item_url_joins_relative() {
        let u = absolute_item_url("https://x/dav/cal/u/d/", "/dav/cal/u/d/event.ics").unwrap();
        assert_eq!(u, "https://x/dav/cal/u/d/event.ics");
    }

    fn response(status: Option<u16>, propstat_errors: Vec<u16>) -> DavResponse {
        DavResponse {
            href: Href::from_normalised("/dav/cal/u/d/e.ics".to_owned()),
            status,
            props: crate::dav::parse::ResourceProps::default(),
            propstat_errors,
        }
    }

    #[test]
    fn response_level_404_is_vanished() {
        assert!(response_indicates_vanished(&response(Some(404), vec![])));
        assert!(response_indicates_vanished(&response(Some(410), vec![])));
    }

    #[test]
    fn per_propstat_404_or_410_is_vanished() {
        assert!(response_indicates_vanished(&response(None, vec![404])));
        assert!(response_indicates_vanished(&response(None, vec![200, 410])));
    }

    #[test]
    fn other_propstat_errors_are_not_vanished() {
        assert!(!response_indicates_vanished(&response(None, vec![403])));
        assert!(!response_indicates_vanished(&response(None, vec![])));
    }
}
