/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashSet, VecDeque};
use std::io::Read;

use rusqlite::OptionalExtension;
use time::format_description::well_known::Rfc3339 as Rfc3339Format;
use time::macros::format_description;
use time::{OffsetDateTime, PrimitiveDateTime};

use crate::archive::rows::{FileNodeRecord, NodeType, Subscribed};
use crate::archive::{BlobId, LocalId, SourceId, Store};
use crate::console::{Counted, Grouped};
use crate::dav::client::{DavClient, DavStream};
use crate::dav::discover::DiscoveredCollection;
use crate::dav::error::DavError;
use crate::dav::href::Href;
use crate::dav::parse::DavResponse;
use crate::dav::retry::DavOutcome;
use crate::dav::xml::Propfind;
use crate::error::Error;
use crate::logging::{Level, Logger};
use crate::sync::TypeCounts;
use crate::sync::dates::MissingDate;
use crate::sync::pool::Pool;
use crate::sync::tree::FolderPath;
use crate::sync::vanish::{Children, OnRowError, RowFailure, VanishedSweep};
use crate::types::ObjectType;

use super::MAX_DAV_CONNECTIONS;

const FILE_COMMIT_BATCH: usize = 16;
const UNNAMED: &str = "_";
const HREF_DELIMITER: char = '/';

pub struct FileTree<'a> {
    pub client: &'a DavClient,
    pub store: Store<'a>,
    pub source: SourceId,
    pub base_url: &'a str,
    pub dav_connections: usize,
    pub logger: Logger,
}

struct FilePlan {
    item_href: String,
    parent_href: String,
    parent_local: Option<LocalId>,
    existing_local: Option<LocalId>,
    etag: String,
    content_type: Option<String>,
    last_modified: Option<String>,
    creation_date: Option<String>,
    displayname: Option<String>,
    url: String,
}

struct FileFetched {
    plan: FilePlan,
    result: Result<FetchedBody, DavError>,
}

struct FetchedBody {
    status: u16,
    bytes: Vec<u8>,
    etag: Option<String>,
    content_type: Option<String>,
    last_modified: Option<String>,
}

impl FetchedBody {
    fn read(mut stream: DavStream) -> Result<FetchedBody, DavError> {
        let mut bytes = Vec::new();
        stream
            .read_to_end(&mut bytes)
            .map_err(|e| DavError::Transport(format!("read body: {e}")))?;
        Ok(FetchedBody {
            status: stream.status,
            bytes,
            etag: stream.etag,
            content_type: stream.content_type,
            last_modified: stream.last_modified,
        })
    }
}

struct Directory {
    url: String,
    href: String,
    local: Option<LocalId>,
}

struct Walk {
    seen: HashSet<String>,
    plans: Vec<FilePlan>,
}

impl FileTree<'_> {
    pub fn reconcile(
        &self,
        root: &DiscoveredCollection,
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        let logger = self.logger;
        let root_href = root.href.as_str();
        let known = self
            .store
            .dav_ids(self.source)
            .all(ObjectType::FileNode)
            .map_err(Error::partial)?;
        let mut walk = Walk {
            seen: HashSet::from([root_href.to_owned()]),
            plans: Vec::new(),
        };
        let mut visited = HashSet::from([root_href.to_owned()]);
        let mut queue = VecDeque::from([Directory {
            url: Href::join_absolute(self.base_url, root_href).map_err(Error::partial)?,
            href: root_href.to_owned(),
            local: None,
        }]);

        while let Some(directory) = queue.pop_front() {
            match self.list(&directory, &mut walk, counts) {
                Ok(children) => {
                    for child in children {
                        if visited.insert(child.href.clone()) {
                            queue.push_back(child);
                        } else {
                            logger.warn(format_args!(
                                "cycle detected at {}; not recursing",
                                child.href
                            ));
                        }
                    }
                }
                Err(e) if e.aborts_run() => return Err(e),
                Err(e) => {
                    logger.warn(format_args!("PROPFIND {} failed: {e}", directory.url));
                    counts.failed += 1;
                }
            }
        }

        self.fetch(walk.plans, counts)?;

        let mut vanished: Vec<(String, LocalId)> = known
            .into_iter()
            .filter(|(href, _)| !walk.seen.contains(href))
            .collect();
        self.sweep(&mut vanished, counts)
    }

    fn list(
        &self,
        directory: &Directory,
        walk: &mut Walk,
        counts: &mut TypeCounts,
    ) -> Result<Vec<Directory>, Error> {
        let url = directory.url.as_str();
        let ms = self
            .client
            .propfind_responses(url, 1, Propfind::WebdavListing.body(), url)
            .map_err(Error::partial)?;
        if DavOutcome::is_error(ms.status) {
            return Err(Error::Partial(format!("http {}", ms.status)));
        }
        let self_href = Href::parse(url, "")
            .map(Href::into_string)
            .unwrap_or_default();
        let mut children = Vec::new();
        for response in ms.responses {
            let href = response.href.as_str();
            if href == self_href || href == directory.href {
                continue;
            }
            walk.seen.insert(href.to_owned());
            if response.props.is_collection {
                let local = self.upsert_directory(&response, directory.local, counts)?;
                children.push(Directory {
                    url: Href::join_absolute(url, href).map_err(Error::partial)?,
                    href: response.href.into_string(),
                    local: Some(local),
                });
            } else {
                let href = response.href.as_str().to_owned();
                match self.plan_file(response, directory) {
                    Ok(Some(plan)) => walk.plans.push(plan),
                    Ok(None) => {}
                    Err(e) => {
                        self.logger.warn(format_args!("file {href}: {e}"));
                        counts.failed += 1;
                    }
                }
            }
        }
        self.logger.progress(format_args!(
            "Directory {url}: {} queued ({} fetched, {} failed so far)",
            Counted::new(walk.plans.len() as u64, "file", "files"),
            Grouped(counts.fetched),
            Grouped(counts.failed)
        ));
        Ok(children)
    }

    fn plan_file(
        &self,
        response: DavResponse,
        directory: &Directory,
    ) -> Result<Option<FilePlan>, Error> {
        let item_href = response.href.into_string();
        let props = response.props;
        let etag = props.etag.unwrap_or_default();
        let known = self
            .store
            .dav_ids(self.source)
            .item(ObjectType::FileNode, &item_href)
            .map_err(Error::partial)?;
        if let Some(known) = &known
            && !etag.is_empty()
            && known.etag == etag
        {
            return Ok(None);
        }
        let url = Href::join_absolute(&directory.url, &item_href).map_err(Error::partial)?;
        Ok(Some(FilePlan {
            item_href,
            parent_href: directory.href.clone(),
            parent_local: directory.local,
            existing_local: known.map(|known| known.local),
            etag,
            content_type: props.content_type,
            last_modified: props.last_modified,
            creation_date: props.creation_date,
            displayname: props.displayname,
            url,
        }))
    }

    fn upsert_directory(
        &self,
        response: &DavResponse,
        parent: Option<LocalId>,
        counts: &mut TypeCounts,
    ) -> Result<LocalId, Error> {
        let href = response.href.as_str();
        let name = NodeHref(&response.href).directory_name(response.props.displayname.as_deref());
        let modified = DavDate(response.props.last_modified.as_deref()).normalised();
        let existing = self
            .store
            .dav_ids(self.source)
            .local_id(ObjectType::FileNode, href)
            .map_err(Error::partial)?;
        let tx = self.store.begin().map_err(Error::partial)?;
        let store = tx.store();
        let kept = match existing {
            Some(local) => StoredNode(store).kept(local).map_err(Error::partial)?,
            None => None,
        };
        let created;
        let record = match &kept {
            Some(kept) => FileNodeRecord {
                parent,
                node_type: kept.node_type,
                blob: kept.blob,
                target: kept.target.as_deref(),
                name: &name,
                media_type: kept.media_type.as_deref(),
                created: &kept.created,
                modified: modified.as_deref(),
                subscribed: kept.subscribed,
                role: kept.role.as_deref(),
                executable: kept.executable,
            },
            None => {
                created = DavDate(response.props.creation_date.as_deref()).or_now();
                FileNodeRecord {
                    parent,
                    node_type: NodeType::Directory,
                    blob: None,
                    target: None,
                    name: &name,
                    media_type: None,
                    created: &created,
                    modified: modified.as_deref(),
                    subscribed: Subscribed::Yes,
                    role: None,
                    executable: false,
                }
            }
        };
        let nodes = store.file_nodes();
        let local = match existing {
            Some(local) => {
                nodes.update(local, &record).map_err(Error::partial)?;
                counts.fetched += 1;
                local
            }
            None => {
                let local = nodes.insert(&record).map_err(Error::partial)?;
                store
                    .dav_ids(self.source)
                    .insert(
                        ObjectType::FileNode,
                        response.href.parent().as_str(),
                        href,
                        "",
                        local,
                    )
                    .map_err(Error::partial)?;
                counts.created += 1;
                local
            }
        };
        tx.commit().map_err(Error::partial)?;
        Ok(local)
    }

    fn fetch(&self, plans: Vec<FilePlan>, counts: &mut TypeCounts) -> Result<(), Error> {
        if plans.is_empty() {
            return Ok(());
        }
        let logger = self.logger;
        let workers = self.dav_connections.clamp(1, MAX_DAV_CONNECTIONS);
        let client = self.client.clone();
        let pool: Pool<FilePlan, FileFetched> = Pool::new(workers, move |plan: FilePlan| {
            let result = client.get_stream(&plan.url).and_then(FetchedBody::read);
            FileFetched { plan, result }
        });
        let submitted = plans.len();
        for plan in plans {
            pool.submit(plan);
        }

        let mut tx = self.store.begin().map_err(Error::partial)?;
        let mut pending = 0usize;
        for _ in 0..submitted {
            let Ok(fetched) = pool.results().recv() else {
                break;
            };
            let href = fetched.plan.item_href.as_str();
            match fetched.result {
                Err(e) => {
                    logger.warn(format_args!("file {href}: {e}"));
                    counts.failed += 1;
                }
                Ok(body) if DavOutcome::is_gone(body.status) => {}
                Ok(body) if DavOutcome::is_error(body.status) => {
                    logger.warn(format_args!("GET {href} returned HTTP {}", body.status));
                    counts.failed += 1;
                }
                Ok(body) => match self.commit_file(tx.store(), &fetched.plan, body) {
                    Ok(()) => {
                        counts.fetched += 1;
                        pending += 1;
                        if pending >= FILE_COMMIT_BATCH {
                            tx.commit().map_err(Error::partial)?;
                            tx = self.store.begin().map_err(Error::partial)?;
                            pending = 0;
                        }
                    }
                    Err(e) => {
                        logger.warn(format_args!("file {href}: {e}"));
                        counts.failed += 1;
                    }
                },
            }
        }
        tx.commit().map_err(Error::partial)
    }

    fn commit_file(
        &self,
        store: Store<'_>,
        plan: &FilePlan,
        body: FetchedBody,
    ) -> Result<(), Error> {
        let etag = body.etag.as_deref().unwrap_or(&plan.etag);
        let media_type = body
            .content_type
            .as_deref()
            .or(plan.content_type.as_deref());
        let modified = DavDate(
            body.last_modified
                .as_deref()
                .or(plan.last_modified.as_deref()),
        )
        .normalised();
        let created = DavDate(plan.creation_date.as_deref()).or_now();
        let name = plan.name(self.logger);
        let blob = store.blobs().intern(&body.bytes).map_err(Error::partial)?;
        let kept = match plan.existing_local {
            Some(local) => StoredNode(store).kept(local).map_err(Error::partial)?,
            None => None,
        };
        let record = FileNodeRecord {
            parent: plan.parent_local,
            node_type: NodeType::File,
            blob: Some(blob),
            target: None,
            name: &name,
            media_type,
            created: &created,
            modified: modified.as_deref(),
            subscribed: kept.as_ref().map_or(Subscribed::Yes, |k| k.subscribed),
            role: kept.as_ref().and_then(|k| k.role.as_deref()),
            executable: kept.as_ref().is_some_and(|k| k.executable),
        };
        let nodes = store.file_nodes();
        let ids = store.dav_ids(self.source);
        match plan.existing_local {
            Some(local) => {
                nodes.update(local, &record).map_err(Error::partial)?;
                ids.update_etag(ObjectType::FileNode, &plan.item_href, etag)
            }
            None => {
                let local = nodes.insert(&record).map_err(Error::partial)?;
                ids.insert(
                    ObjectType::FileNode,
                    &plan.parent_href,
                    &plan.item_href,
                    etag,
                    local,
                )
            }
        }
        .map_err(Error::partial)
    }

    fn sweep(
        &self,
        vanished: &mut [(String, LocalId)],
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        if vanished.is_empty() {
            return Ok(());
        }
        FolderPath::sort_children_first_by(vanished, HREF_DELIMITER, |(href, _)| href);
        let source = self.source;
        let logger = self.logger;
        VanishedSweep::new(self.store, OnRowError::Count, Children::Restrict)
            .run(
                ObjectType::FileNode,
                vanished,
                counts,
                |store, href, _| store.dav_ids(source).delete(ObjectType::FileNode, href),
                |href, _, failure| {
                    if let RowFailure::Sql(e) = failure {
                        logger.warn(format_args!(
                            "cannot delete vanished file node {href}: {e}; skipped"
                        ));
                    }
                },
            )
            .map_err(Error::partial)?;
        logger.progress(format_args!(
            "Deleted {}",
            Counted::new(
                vanished.len() as u64,
                "vanished file node",
                "vanished file nodes"
            )
        ));
        Ok(())
    }
}

impl FilePlan {
    fn name(&self, logger: Logger) -> String {
        let basename = NodeHref::basename_of(&self.item_href);
        if let Some(displayname) = &self.displayname
            && !displayname.trim().is_empty()
            && displayname.trim() != basename
            && logger.enabled(Level::Bodies)
        {
            logger.progress(format_args!(
                "File {}: display name \"{displayname}\" differs from \"{basename}\"; using the latter",
                self.item_href
            ));
        }
        basename
    }
}

#[derive(Clone, Copy)]
struct NodeHref<'h>(&'h Href);

impl NodeHref<'_> {
    fn basename(self) -> String {
        let component = self.0.last_component();
        if component.is_empty() {
            UNNAMED.to_owned()
        } else {
            component
        }
    }

    fn basename_of(href: &str) -> String {
        NodeHref(&Href::from_normalised(href.to_owned())).basename()
    }

    fn directory_name(self, displayname: Option<&str>) -> String {
        match displayname {
            Some(name) if !name.trim().is_empty() => name.to_owned(),
            _ => self.basename(),
        }
    }
}

struct KeptNode {
    node_type: NodeType,
    blob: Option<BlobId>,
    target: Option<String>,
    media_type: Option<String>,
    created: String,
    subscribed: Subscribed,
    role: Option<String>,
    executable: bool,
}

#[derive(Clone, Copy)]
struct StoredNode<'a>(Store<'a>);

impl StoredNode<'_> {
    fn kept(self, id: LocalId) -> rusqlite::Result<Option<KeptNode>> {
        self.0
            .connection()
            .prepare_cached(
                "SELECT node_type, blob_id, target, media_type, created, is_subscribed, role,
                        executable
                 FROM file_nodes WHERE id = ?1",
            )?
            .query_row([id], |row| {
                Ok(KeptNode {
                    node_type: row.get(0)?,
                    blob: row.get(1)?,
                    target: row.get(2)?,
                    media_type: row.get(3)?,
                    created: row.get(4)?,
                    subscribed: row.get(5)?,
                    role: row.get(6)?,
                    executable: row.get(7)?,
                })
            })
            .optional()
    }
}

#[derive(Clone, Copy)]
struct DavDate<'a>(Option<&'a str>);

impl DavDate<'_> {
    fn normalised(self) -> Option<String> {
        let raw = self.0?.trim();
        if raw.is_empty() {
            return None;
        }
        if let Ok(t) = OffsetDateTime::parse(raw, &Rfc3339Format) {
            return t.format(&Rfc3339Format).ok();
        }
        let imf_fixdate = format_description!(
            "[weekday repr:short], [day] [month repr:short] [year] \
             [hour]:[minute]:[second] GMT"
        );
        PrimitiveDateTime::parse(raw, imf_fixdate)
            .ok()
            .and_then(|p| p.assume_utc().format(&Rfc3339Format).ok())
    }

    fn or_now(self) -> String {
        self.normalised()
            .unwrap_or_else(|| MissingDate::Now.resolve())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_handles_percent_encoding() {
        assert_eq!(
            NodeHref::basename_of("/dav/file/u/work/note%201.txt"),
            "note 1.txt"
        );
    }

    fn plan_with(displayname: Option<&str>, href: &str) -> FilePlan {
        FilePlan {
            item_href: href.to_owned(),
            parent_href: "/dav/file/u/".to_owned(),
            parent_local: Some(LocalId::from_raw(1)),
            existing_local: None,
            etag: String::new(),
            content_type: None,
            last_modified: None,
            creation_date: None,
            displayname: displayname.map(str::to_owned),
            url: format!("https://x{href}"),
        }
    }

    #[test]
    fn file_name_prefers_the_url_basename_over_the_displayname() {
        let plan = plan_with(Some("Pretty Name.txt"), "/dav/file/u/photo.jpg");
        assert_eq!(plan.name(Logger::new(Level::Default)), "photo.jpg");
    }

    #[test]
    fn file_name_is_the_basename_without_a_displayname() {
        let plan = plan_with(None, "/dav/file/u/photo.jpg");
        assert_eq!(plan.name(Logger::new(Level::Default)), "photo.jpg");
    }

    #[test]
    fn directory_name_prefers_a_non_blank_displayname() {
        let href = Href::from_normalised("/dav/file/u/work/".to_owned());
        assert_eq!(NodeHref(&href).directory_name(Some("Work")), "Work");
        assert_eq!(NodeHref(&href).directory_name(Some("  ")), "work");
        assert_eq!(NodeHref(&href).directory_name(None), "work");
    }

    #[test]
    fn dav_date_parses_imf_fixdate() {
        let n = DavDate(Some("Wed, 27 May 2026 07:59:07 GMT"))
            .normalised()
            .expect("imf parse");
        assert!(n.starts_with("2026-05-27T07:59:07"));
    }

    #[test]
    fn dav_date_is_none_on_garbage() {
        assert!(DavDate(Some("not a date")).normalised().is_none());
        assert!(DavDate(None).normalised().is_none());
    }

    #[test]
    fn directory_update_keeps_the_columns_dav_never_rewrites() {
        use crate::archive::Archive;
        use crate::archive::sources::{SourceKey, SourceKind};
        use crate::dav::parse::ResourceProps;
        use crate::net::auth::Auth;
        use crate::net::http::{ClientOptions, TlsPolicy};
        use crate::net::retry::RetryPolicy;

        let archive = Archive::open_in_memory().unwrap();
        let store = archive.store();
        let source = store
            .sources()
            .upsert(
                &SourceKey {
                    kind: SourceKind::WebDav,
                    url: "https://x",
                    account_id: "u",
                },
                Some("u"),
                "u",
            )
            .unwrap();
        let logger = Logger::new(Level::Quiet);
        let client = DavClient::new(
            Auth::Basic {
                user: "u".into(),
                password: "p".into(),
            },
            ClientOptions {
                retry: RetryPolicy::new(0),
                tls: TlsPolicy::Verify,
                logger,
            },
        );
        let tree = FileTree {
            client: &client,
            store,
            source,
            base_url: "https://x/dav/",
            dav_connections: 1,
            logger,
        };
        let response = DavResponse {
            href: Href::from_normalised("/dav/docs/".to_owned()),
            status: None,
            props: ResourceProps::default(),
            propstat_errors: Vec::new(),
        };
        let local = tree
            .upsert_directory(&response, None, &mut TypeCounts::default())
            .unwrap();
        archive
            .connection()
            .execute_batch(
                "UPDATE file_nodes SET is_subscribed = 0, role = 'archive', executable = 1,
                    created = '2001-01-01T00:00:00Z'",
            )
            .unwrap();

        let again = tree
            .upsert_directory(&response, None, &mut TypeCounts::default())
            .unwrap();

        assert_eq!(again, local);
        let kept: (bool, Option<String>, bool, String) = archive
            .connection()
            .query_row(
                "SELECT is_subscribed, role, executable, created FROM file_nodes",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            kept,
            (
                false,
                Some("archive".to_owned()),
                true,
                "2001-01-01T00:00:00Z".to_owned()
            )
        );
    }
}
