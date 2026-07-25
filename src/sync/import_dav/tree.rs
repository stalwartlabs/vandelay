/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashSet, VecDeque};
use std::io::Read;

use rusqlite::{Connection, OptionalExtension, params};
use time::format_description::well_known::Rfc3339;

use crate::dav::client::DavClient;
use crate::dav::discover::DiscoveredCollection;
use crate::dav::href::{Href, join_absolute, last_path_component};
use crate::dav::parse::DavResponse;
use crate::dav::xml;
use crate::db;
use crate::db::dav_ids;
use crate::error::Error;
use crate::jmap::error::JmapError;
use crate::logging::{LEVEL_PROGRESS, Logger};
use crate::sync::TypeCounts;
use crate::sync::pool::Pool;

struct FilePlan {
    item_href: String,
    parent_href: String,
    parent_local: Option<i64>,
    existing_local: Option<i64>,
    propfind_etag: String,
    propfind_content_type: Option<String>,
    propfind_last_modified: Option<String>,
    propfind_creation_date: Option<String>,
    propfind_displayname: Option<String>,
    url: String,
}

struct FileFetch {
    plan: FilePlan,
}

struct FileFetched {
    plan: FilePlan,
    result: Result<FetchedBody, JmapError>,
}

struct FetchedBody {
    status: u16,
    bytes: Vec<u8>,
    etag: Option<String>,
    content_type: Option<String>,
    last_modified: Option<String>,
}

pub struct WebDavCtx<'a> {
    pub client: &'a DavClient,
    pub source_id: i64,
    pub base_url: &'a str,
    pub dav_connections: usize,
    pub logger: Logger,
}

pub fn reconcile_filenodes(
    conn: &mut Connection,
    ctx: &WebDavCtx<'_>,
    root: &DiscoveredCollection,
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let client = ctx.client;
    let source_id = ctx.source_id;
    let base_url = ctx.base_url;
    let dav_connections = ctx.dav_connections;
    let logger = ctx.logger;
    let absolute_root = absolute(base_url, root.href.as_str())?;

    let known: Vec<(String, i64)> =
        dav_ids::collections_of_type(conn, source_id, dav_ids::FILE_NODE)
            .map_err(|e| Error::Partial(e.to_string()))?
            .into_iter()
            .collect();
    let mut seen: HashSet<String> = HashSet::new();
    seen.insert(root.href.as_str().to_owned());

    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(root.href.as_str().to_owned());
    let mut queue: VecDeque<(String, String, Option<i64>)> = VecDeque::new();
    queue.push_back((absolute_root, root.href.as_str().to_owned(), None));

    let mut file_plans: Vec<FilePlan> = Vec::new();

    while let Some((url, parent_href, parent_local)) = queue.pop_front() {
        match walk_one(
            conn,
            client,
            source_id,
            WalkPos {
                url: &url,
                parent_href: &parent_href,
                parent_local,
            },
            WalkState {
                counts,
                seen: &mut seen,
                file_plans: &mut file_plans,
            },
            logger,
        ) {
            Ok(children) => {
                for (child_url, child_href, child_local) in children {
                    if visited.insert(child_href.clone()) {
                        queue.push_back((child_url, child_href, child_local));
                    } else {
                        logger.warn(&format!("cycle detected at {child_href}; not recursing"));
                    }
                }
            }
            Err(e) => {
                logger.warn(&format!("PROPFIND {url}: {e}"));
                counts.failed += 1;
            }
        }
    }

    fetch_files_parallel(
        conn,
        client,
        source_id,
        dav_connections,
        file_plans,
        counts,
        logger,
    )?;

    delete_vanished(conn, source_id, &known, &seen, counts, logger)?;

    Ok(())
}

fn fetch_files_parallel(
    conn: &mut Connection,
    client: &DavClient,
    source_id: i64,
    dav_connections: usize,
    plans: Vec<FilePlan>,
    counts: &mut TypeCounts,
    logger: Logger,
) -> Result<(), Error> {
    if plans.is_empty() {
        return Ok(());
    }
    let workers = dav_connections.clamp(1, 8);
    let client_for_pool = client.clone();
    let pool: Pool<FileFetch, FileFetched> = Pool::new(workers, move |job: FileFetch| {
        let result = match client_for_pool.get_stream(&job.plan.url) {
            Ok(mut s) => {
                let mut bytes = Vec::new();
                match s.read_to_end(&mut bytes) {
                    Ok(_) => Ok(FetchedBody {
                        status: s.status,
                        bytes,
                        etag: s.etag,
                        content_type: s.content_type,
                        last_modified: s.last_modified,
                    }),
                    Err(e) => Err(JmapError::Transport(format!("read body: {e}"))),
                }
            }
            Err(e) => Err(e),
        };
        FileFetched {
            plan: job.plan,
            result,
        }
    });

    let mut submitted: usize = 0;
    for plan in plans {
        pool.submit(FileFetch { plan });
        submitted += 1;
    }

    const FILE_COMMIT_BATCH: usize = 16;
    let mut tx = conn
        .unchecked_transaction()
        .map_err(|e| Error::Partial(e.to_string()))?;
    let mut pending: usize = 0;
    for _ in 0..submitted {
        let fetched = match pool.results().recv() {
            Ok(r) => r,
            Err(_) => break,
        };
        match fetched.result {
            Err(e) => {
                logger.warn(&format!("file {}: {e}", fetched.plan.item_href));
                counts.failed += 1;
            }
            Ok(body) if body.status == 404 || body.status == 410 => {}
            Ok(body) if body.status >= 400 => {
                logger.warn(&format!(
                    "file {}: http {}",
                    fetched.plan.item_href, body.status
                ));
                counts.failed += 1;
            }
            Ok(body) => match commit_file(&tx, source_id, &fetched.plan, body, logger) {
                Ok(()) => {
                    counts.fetched += 1;
                    pending += 1;
                    if pending >= FILE_COMMIT_BATCH {
                        tx.commit().map_err(|e| Error::Partial(e.to_string()))?;
                        tx = conn
                            .unchecked_transaction()
                            .map_err(|e| Error::Partial(e.to_string()))?;
                        pending = 0;
                    }
                }
                Err(e) => {
                    logger.warn(&format!("file {}: {e}", fetched.plan.item_href));
                    counts.failed += 1;
                }
            },
        }
    }
    tx.commit().map_err(|e| Error::Partial(e.to_string()))?;
    drop(pool);
    Ok(())
}

fn commit_file(
    tx: &rusqlite::Transaction<'_>,
    source_id: i64,
    plan: &FilePlan,
    body: FetchedBody,
    logger: Logger,
) -> Result<(), Error> {
    let etag = body
        .etag
        .clone()
        .unwrap_or_else(|| plan.propfind_etag.clone());
    let content_type = body
        .content_type
        .or_else(|| plan.propfind_content_type.clone());
    let last_modified = normalise_dav_date(
        &body
            .last_modified
            .or_else(|| plan.propfind_last_modified.clone()),
    );
    let created = format_or_now(&plan.propfind_creation_date)?;
    let name = display_or_basename(plan, logger);

    let blob_local =
        db::blobs::intern_blob(tx, &body.bytes).map_err(|e| Error::Partial(e.to_string()))?;
    if let Some(local) = plan.existing_local {
        tx.execute(
            "UPDATE file_nodes SET parent_id = ?1, node_type = 'file', blob_id = ?2,
                                    target = NULL, name = ?3, media_type = ?4,
                                    created = ?5, modified = ?6
             WHERE id = ?7",
            params![
                plan.parent_local,
                blob_local,
                name,
                content_type,
                created,
                last_modified,
                local,
            ],
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
        dav_ids::update_etag(tx, source_id, dav_ids::FILE_NODE, &plan.item_href, &etag)
            .map_err(|e| Error::Partial(e.to_string()))?;
    } else {
        tx.execute(
            "INSERT INTO file_nodes (parent_id, node_type, blob_id, target, name, media_type,
                                      created, modified, is_subscribed, role)
             VALUES (?1, 'file', ?2, NULL, ?3, ?4, ?5, ?6, 1, NULL)",
            params![
                plan.parent_local,
                blob_local,
                name,
                content_type,
                created,
                last_modified,
            ],
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
        let local = tx.last_insert_rowid();
        dav_ids::insert(
            tx,
            source_id,
            dav_ids::FILE_NODE,
            &plan.parent_href,
            &plan.item_href,
            &etag,
            local,
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
    }
    Ok(())
}

fn display_or_basename(plan: &FilePlan, logger: Logger) -> String {
    let href = Href::from_normalised(plan.item_href.clone());
    let basename = decoded_basename(&href);
    if let Some(n) = &plan.propfind_displayname
        && !n.trim().is_empty()
        && n.trim() != basename
        && logger.enabled(crate::logging::LEVEL_BODIES)
    {
        eprintln!(
            "webdav: {} displayname={:?} differs from basename={basename:?}; using basename",
            plan.item_href, n
        );
    }
    basename
}

fn delete_vanished(
    conn: &mut Connection,
    source_id: i64,
    known: &[(String, i64)],
    seen: &HashSet<String>,
    counts: &mut TypeCounts,
    logger: Logger,
) -> Result<(), Error> {
    let mut vanished: Vec<(String, i64)> = known
        .iter()
        .filter(|(href, _)| !seen.contains(href))
        .cloned()
        .collect();
    if vanished.is_empty() {
        return Ok(());
    }
    vanished.sort_by(|a, b| {
        let depth_a = a.0.matches('/').count();
        let depth_b = b.0.matches('/').count();
        depth_b.cmp(&depth_a).then_with(|| a.0.cmp(&b.0))
    });

    let tx = conn
        .unchecked_transaction()
        .map_err(|e| Error::Partial(e.to_string()))?;
    for (href, local_id) in &vanished {
        if let Err(e) = tx.execute("DELETE FROM file_nodes WHERE id = ?1", params![local_id]) {
            logger.warn(&format!(
                "vanished file_node {href:?} delete failed: {e}; skipping"
            ));
            counts.failed += 1;
            continue;
        }
        if let Err(e) = dav_ids::delete_item(&tx, source_id, dav_ids::FILE_NODE, href) {
            logger.warn(&format!(
                "vanished file_node {href:?} sync_id delete failed: {e}"
            ));
            counts.failed += 1;
            continue;
        }
        counts.deleted += 1;
    }
    tx.commit().map_err(|e| Error::Partial(e.to_string()))?;
    if logger.enabled(LEVEL_PROGRESS) {
        eprintln!("webdav: deleted {} vanished file_nodes", vanished.len());
    }
    Ok(())
}

struct WalkPos<'a> {
    url: &'a str,
    parent_href: &'a str,
    parent_local: Option<i64>,
}

struct WalkState<'a> {
    counts: &'a mut TypeCounts,
    seen: &'a mut HashSet<String>,
    file_plans: &'a mut Vec<FilePlan>,
}

fn walk_one(
    conn: &mut Connection,
    client: &DavClient,
    source_id: i64,
    pos: WalkPos<'_>,
    state: WalkState<'_>,
    logger: Logger,
) -> Result<Vec<(String, String, Option<i64>)>, Error> {
    let url = pos.url;
    let parent_href = pos.parent_href;
    let parent_local = pos.parent_local;
    let counts = state.counts;
    let seen = state.seen;
    let file_plans = state.file_plans;
    let body = xml::propfind_webdav_listing();
    let ms = client
        .propfind_responses(url, 1, &body, url)
        .map_err(Error::from)?;
    if ms.status >= 400 {
        return Err(Error::Partial(format!("http {}", ms.status)));
    }
    let parent_norm = crate::dav::href::normalise(url, "")
        .map(|h| h.into_string())
        .unwrap_or_default();
    let mut children = Vec::new();
    for r in ms.responses {
        if r.href.as_str() == parent_norm || r.href.as_str() == parent_href {
            continue;
        }
        seen.insert(r.href.as_str().to_owned());
        if r.props.is_collection {
            let local = upsert_directory(conn, source_id, &r, parent_local, counts)?;
            let abs = absolute(url, r.href.as_str())?;
            children.push((abs, r.href.as_str().to_owned(), Some(local)));
        } else {
            match plan_file(conn, source_id, &r, url, parent_local, parent_href) {
                Ok(Some(plan)) => file_plans.push(plan),
                Ok(None) => {}
                Err(e) => {
                    logger.warn(&format!("file {}: {e}", r.href.as_str()));
                    counts.failed += 1;
                }
            }
        }
    }
    if logger.enabled(LEVEL_PROGRESS) {
        eprintln!(
            "dir {url}: queued {} files (fetched-so-far={} failed-so-far={})",
            file_plans.len(),
            counts.fetched,
            counts.failed
        );
    }
    Ok(children)
}

fn plan_file(
    conn: &Connection,
    source_id: i64,
    response: &DavResponse,
    collection_url: &str,
    parent_local: Option<i64>,
    parent_href: &str,
) -> Result<Option<FilePlan>, Error> {
    let item_href = response.href.as_str().to_owned();
    let propfind_etag = response.props.etag.clone().unwrap_or_default();
    let existing = dav_ids::local_for_item(conn, source_id, dav_ids::FILE_NODE, &item_href)
        .map_err(|e| Error::Partial(e.to_string()))?;
    if let Some(local) = existing {
        let stored_etag: Option<String> = conn
            .query_row(
                "SELECT etag FROM sync_id_dav
                 WHERE source_id = ?1 AND type_name = ?2 AND item_href = ?3",
                params![source_id, dav_ids::FILE_NODE, item_href],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| Error::Partial(e.to_string()))?;
        if let Some(stored) = stored_etag.as_deref()
            && !propfind_etag.is_empty()
            && stored == propfind_etag
        {
            return Ok(None);
        }
        let url = absolute(collection_url, &item_href)?;
        Ok(Some(FilePlan {
            item_href,
            parent_href: parent_href.to_owned(),
            parent_local,
            existing_local: Some(local),
            propfind_etag,
            propfind_content_type: response.props.content_type.clone(),
            propfind_last_modified: response.props.last_modified.clone(),
            propfind_creation_date: response.props.creation_date.clone(),
            propfind_displayname: response.props.displayname.clone(),
            url,
        }))
    } else {
        let url = absolute(collection_url, &item_href)?;
        Ok(Some(FilePlan {
            item_href,
            parent_href: parent_href.to_owned(),
            parent_local,
            existing_local: None,
            propfind_etag,
            propfind_content_type: response.props.content_type.clone(),
            propfind_last_modified: response.props.last_modified.clone(),
            propfind_creation_date: response.props.creation_date.clone(),
            propfind_displayname: response.props.displayname.clone(),
            url,
        }))
    }
}

fn upsert_directory(
    conn: &mut Connection,
    source_id: i64,
    response: &DavResponse,
    parent_local: Option<i64>,
    counts: &mut TypeCounts,
) -> Result<i64, Error> {
    let item_href = response.href.as_str().to_owned();
    let name = display_or_path(&response.props.displayname, &response.href);
    let modified = normalise_dav_date(&response.props.last_modified);
    let existing = dav_ids::local_for_item(conn, source_id, dav_ids::FILE_NODE, &item_href)
        .map_err(|e| Error::Partial(e.to_string()))?;
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| Error::Partial(e.to_string()))?;
    if let Some(local) = existing {
        tx.execute(
            "UPDATE file_nodes SET parent_id = ?1, name = ?2, modified = ?3
             WHERE id = ?4",
            params![parent_local, name, modified, local],
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
        tx.commit().map_err(|e| Error::Partial(e.to_string()))?;
        counts.fetched += 1;
        Ok(local)
    } else {
        let created = format_or_now(&response.props.creation_date)?;
        tx.execute(
            "INSERT INTO file_nodes (parent_id, node_type, blob_id, target, name, media_type,
                                      created, modified, is_subscribed, role)
             VALUES (?1, 'directory', NULL, NULL, ?2, NULL, ?3, ?4, 1, NULL)",
            params![parent_local, name, created, modified],
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
        let local = tx.last_insert_rowid();
        let parent_href = parent_href_string(&response.href);
        dav_ids::insert(
            &tx,
            source_id,
            dav_ids::FILE_NODE,
            &parent_href,
            &item_href,
            "",
            local,
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
        tx.commit().map_err(|e| Error::Partial(e.to_string()))?;
        counts.created += 1;
        Ok(local)
    }
}

pub(super) fn parent_href_string(href: &Href) -> String {
    let s = href.as_str();
    let trimmed = s.trim_end_matches('/');
    if let Some(slash) = trimmed.rfind('/') {
        s[..=slash].to_owned()
    } else {
        "/".to_owned()
    }
}

fn absolute(base: &str, href: &str) -> Result<String, Error> {
    join_absolute(base, href).map_err(|e| Error::Partial(e.to_string()))
}

fn display_or_path(name: &Option<String>, href: &Href) -> String {
    if let Some(n) = name
        && !n.trim().is_empty()
    {
        return n.clone();
    }
    decoded_basename(href)
}

fn decoded_basename(href: &Href) -> String {
    let c = last_path_component(href);
    if c.is_empty() { "_".to_owned() } else { c }
}

fn normalise_dav_date(input: &Option<String>) -> Option<String> {
    let raw = input.as_deref()?.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(t) = time::OffsetDateTime::parse(raw, &Rfc3339) {
        return t.format(&Rfc3339).ok();
    }
    let imf_fixdate = time::macros::format_description!(
        "[weekday repr:short], [day] [month repr:short] [year] \
         [hour]:[minute]:[second] GMT"
    );
    if let Ok(p) = time::PrimitiveDateTime::parse(raw, imf_fixdate) {
        return p.assume_utc().format(&Rfc3339).ok();
    }
    None
}

fn format_or_now(input: &Option<String>) -> Result<String, Error> {
    if let Some(s) = normalise_dav_date(input) {
        return Ok(s);
    }
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|e| Error::Partial(format!("clock: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_href_string_of_root_is_root() {
        let h = Href::from_normalised("/".to_owned());
        assert_eq!(parent_href_string(&h), "/");
    }

    #[test]
    fn parent_href_string_of_dir_strips_trailing_segment() {
        let h = Href::from_normalised("/dav/file/u/work/".to_owned());
        assert_eq!(parent_href_string(&h), "/dav/file/u/");
    }

    #[test]
    fn decoded_basename_handles_percent_encoding() {
        let h = Href::from_normalised("/dav/file/u/work/note%201.txt".to_owned());
        assert_eq!(decoded_basename(&h), "note 1.txt");
    }

    fn plan_with(displayname: Option<&str>, href: &str) -> FilePlan {
        FilePlan {
            item_href: href.to_owned(),
            parent_href: "/dav/file/u/".to_owned(),
            parent_local: Some(1),
            existing_local: None,
            propfind_etag: String::new(),
            propfind_content_type: None,
            propfind_last_modified: None,
            propfind_creation_date: None,
            propfind_displayname: displayname.map(str::to_owned),
            url: format!("https://x{href}"),
        }
    }

    #[test]
    fn display_or_basename_prefers_url_basename_over_displayname() {
        let plan = plan_with(Some("Pretty Name.txt"), "/dav/file/u/photo.jpg");
        let logger = Logger::from_flags(false, 0);
        assert_eq!(display_or_basename(&plan, logger), "photo.jpg");
    }

    #[test]
    fn display_or_basename_returns_basename_when_no_displayname() {
        let plan = plan_with(None, "/dav/file/u/photo.jpg");
        let logger = Logger::from_flags(false, 0);
        assert_eq!(display_or_basename(&plan, logger), "photo.jpg");
    }

    #[test]
    fn normalise_dav_date_parses_imf_fixdate() {
        let n = normalise_dav_date(&Some("Wed, 27 May 2026 07:59:07 GMT".to_owned()))
            .expect("imf parse");
        assert!(n.starts_with("2026-05-27T07:59:07"));
    }

    #[test]
    fn normalise_dav_date_returns_none_on_garbage() {
        assert!(normalise_dav_date(&Some("not a date".to_owned())).is_none());
        assert!(normalise_dav_date(&None).is_none());
    }

    #[test]
    fn visited_hashset_breaks_recursion_on_cyclic_listing() {
        let mut visited: HashSet<String> = HashSet::new();
        visited.insert("/files/u/".to_owned());
        assert!(visited.insert("/files/u/a/".to_owned()));
        assert!(visited.insert("/files/u/a/b/".to_owned()));
        assert!(
            !visited.insert("/files/u/".to_owned()),
            "cycle back to root not re-recursed"
        );
        assert!(
            !visited.insert("/files/u/a/".to_owned()),
            "cycle back to a/ not re-recursed"
        );
    }
}
