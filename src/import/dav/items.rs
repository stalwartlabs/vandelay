/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};
use std::{slice, str};

use crate::archive::ids::dav::DavItem;
use crate::archive::rows::{CalendarEventRecord, ContactCardRecord};
use crate::archive::{LocalId, SourceId, Store};
use crate::console::Grouped;
use crate::dav::client::{DavClient, DavHttpResponse, MultiStatus};
use crate::dav::error::DavError;
use crate::dav::href::Href;
use crate::dav::parse::DavResponse;
use crate::dav::retry::DavOutcome;
use crate::dav::xml::{Multiget, Propfind};
use crate::error::Error;
use crate::logging::{Level, Logger};
use crate::sync::TypeCounts;
use crate::sync::calcard::{ExtractedFields, JsCalendarEntry, JsContact};
use crate::sync::pool::Pool;
use crate::sync::vanish::{Children, OnRowError, VanishedSweep};

use super::MAX_DAV_CONNECTIONS;
use super::collections::CollectionKind;

const COMMIT_BATCH: usize = 64;
const METHOD_NOT_ALLOWED: u16 = 405;
const NOT_IMPLEMENTED: u16 = 501;

pub(super) struct ItemSync<'a> {
    pub(super) client: &'a DavClient,
    pub(super) store: Store<'a>,
    pub(super) source: SourceId,
    pub(super) base_url: &'a str,
    pub(super) multiget_batch: usize,
    pub(super) dav_connections: usize,
    pub(super) logger: Logger,
}

impl ItemSync<'_> {
    pub(super) fn reconcile(
        &self,
        kind: CollectionKind,
        collection_href: &str,
        container: LocalId,
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        let url = Href::join_absolute(self.base_url, collection_href).map_err(Error::partial)?;
        let server = self.enumerate(&url)?;
        let local = self
            .store
            .dav_ids(self.source)
            .items_in(kind.item(), collection_href)
            .map_err(Error::partial)?;
        let collection = CollectionItems {
            sync: self,
            kind,
            href: collection_href,
            container,
            url,
            local,
        };
        collection.reconcile(server, counts)
    }

    fn enumerate(&self, url: &str) -> Result<Vec<ServerItem>, Error> {
        let ms = self
            .client
            .propfind_responses(url, 1, Propfind::DavItems.body(), url)
            .map_err(Error::partial)?;
        if DavOutcome::is_error(ms.status) {
            return Err(Error::Partial(format!(
                "enumerate {url}: http {}",
                ms.status
            )));
        }
        let self_href = Href::parse(url, "")
            .map(Href::into_string)
            .unwrap_or_default();
        Ok(ms
            .responses
            .into_iter()
            .filter(|r| r.href.as_str() != self_href && !r.props.is_collection)
            .map(|r| ServerItem {
                href: r.href.into_string(),
                etag: r.props.etag.unwrap_or_default(),
            })
            .collect())
    }
}

#[derive(Debug, Clone)]
struct ServerItem {
    href: String,
    etag: String,
}

struct MultigetJob {
    body: String,
    items: Vec<ServerItem>,
}

struct MultigetReply {
    items: Vec<ServerItem>,
    result: Result<MultiStatus, DavError>,
}

struct GetJob {
    url: String,
    item: ServerItem,
}

struct GetReply {
    item: ServerItem,
    result: Result<DavHttpResponse, DavError>,
}

struct CollectionItems<'a> {
    sync: &'a ItemSync<'a>,
    kind: CollectionKind,
    href: &'a str,
    container: LocalId,
    url: String,
    local: HashMap<String, DavItem>,
}

impl CollectionItems<'_> {
    fn reconcile(&self, server: Vec<ServerItem>, counts: &mut TypeCounts) -> Result<(), Error> {
        let server_hrefs: HashSet<&str> = server.iter().map(|s| s.href.as_str()).collect();
        let mut vanished: Vec<(String, LocalId)> = self
            .local
            .iter()
            .filter(|(href, _)| !server_hrefs.contains(href.as_str()))
            .map(|(href, item)| (href.clone(), item.local))
            .collect();
        vanished.sort_unstable();
        drop(server_hrefs);
        let to_fetch: Vec<ServerItem> = server
            .into_iter()
            .filter(|s| {
                self.local
                    .get(&s.href)
                    .is_none_or(|known| s.etag.is_empty() || s.etag != known.etag)
            })
            .collect();

        if !to_fetch.is_empty() {
            let fallback = self.multiget(to_fetch, counts)?;
            if !fallback.is_empty() {
                self.get_each(fallback, counts)?;
            }
        }
        self.sweep(&vanished, counts)?;

        self.sync.logger.progress(format_args!(
            "Collection {}: {} fetched, {} deleted, {} failed",
            self.href,
            Grouped(counts.fetched),
            Grouped(counts.deleted),
            Grouped(counts.failed)
        ));
        Ok(())
    }

    fn workers(&self) -> usize {
        self.sync.dav_connections.clamp(1, MAX_DAV_CONNECTIONS)
    }

    fn multiget(
        &self,
        to_fetch: Vec<ServerItem>,
        counts: &mut TypeCounts,
    ) -> Result<Vec<ServerItem>, Error> {
        let logger = self.sync.logger;
        let url = self.url.as_str();
        let batch_size = self.sync.multiget_batch.max(1);
        let client = self.sync.client.clone();
        let pool_url = self.url.clone();
        let pool: Pool<MultigetJob, MultigetReply> =
            Pool::new(self.workers(), move |job: MultigetJob| {
                let result = client.report_responses(&pool_url, 1, &job.body, &pool_url);
                MultigetReply {
                    items: job.items,
                    result,
                }
            });
        let query = self.kind.multiget();
        let mut submitted = 0usize;
        let mut to_fetch = to_fetch.into_iter();
        loop {
            let items: Vec<ServerItem> = to_fetch.by_ref().take(batch_size).collect();
            if items.is_empty() {
                break;
            }
            pool.submit(MultigetJob {
                body: query.body(items.iter().map(|s| s.href.as_str())),
                items,
            });
            submitted += 1;
        }

        let mut fallback = Vec::new();
        for _ in 0..submitted {
            let Ok(reply) = pool.results().recv() else {
                break;
            };
            match reply.result {
                Err(DavError::HttpStatus { status, .. })
                    if status == METHOD_NOT_ALLOWED || status == NOT_IMPLEMENTED =>
                {
                    logger.warn(format_args!(
                        "multiget {url} returned {status}; the server may not implement multiget, falling back to per-item GET"
                    ));
                    fallback.extend(reply.items);
                }
                Err(e) => {
                    logger.warn(format_args!(
                        "multiget {url}: {e}; falling back to per-item GET"
                    ));
                    fallback.extend(reply.items);
                }
                Ok(ms) if DavOutcome::is_error(ms.status) => {
                    logger.warn(format_args!(
                        "multiget {url} returned {}; chunk failed",
                        ms.status
                    ));
                    counts.failed += reply.items.len() as u64;
                }
                Ok(ms) => self.store_multiget(&reply.items, ms, counts)?,
            }
        }
        Ok(fallback)
    }

    fn store_multiget(
        &self,
        requested: &[ServerItem],
        ms: MultiStatus,
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        let requested: HashSet<&str> = requested.iter().map(|s| s.href.as_str()).collect();
        let mut tx = self.sync.store.begin().map_err(Error::partial)?;
        let mut pending = 0usize;
        for response in ms.responses {
            if !requested.contains(response.href.as_str()) {
                self.sync.logger.warn(format_args!(
                    "multiget {}: server returned unsolicited href {}; skipping",
                    self.url,
                    response.href.as_str()
                ));
                continue;
            }
            self.store_response(tx.store(), &response, counts);
            pending += 1;
            if pending >= COMMIT_BATCH {
                tx.commit().map_err(Error::partial)?;
                tx = self.sync.store.begin().map_err(Error::partial)?;
                pending = 0;
            }
        }
        tx.commit().map_err(Error::partial)
    }

    fn store_response(&self, store: Store<'_>, response: &DavResponse, counts: &mut TypeCounts) {
        if response.is_gone() {
            return;
        }
        let href = response.href.as_str();
        let etag = response.props.etag.as_deref().unwrap_or_default();
        let data = match self.kind {
            CollectionKind::Calendar => response.props.calendar_data.as_deref(),
            CollectionKind::AddressBook => response.props.address_data.as_deref(),
        };
        let Some(raw) = data else {
            self.sync.logger.warn(format_args!(
                "multiget {href}: missing {} payload",
                self.kind.data_field()
            ));
            counts.failed += 1;
            return;
        };
        match self.write(store, href, etag, raw) {
            Ok(()) => counts.fetched += 1,
            Err(e) => {
                self.sync.logger.warn(format_args!("item {href}: {e}"));
                counts.failed += 1;
            }
        }
    }

    fn get_each(&self, items: Vec<ServerItem>, counts: &mut TypeCounts) -> Result<(), Error> {
        let logger = self.sync.logger;
        let client = self.sync.client.clone();
        let pool: Pool<GetJob, GetReply> = Pool::new(self.workers(), move |job: GetJob| GetReply {
            result: client.get(&job.url),
            item: job.item,
        });

        let mut submitted = 0usize;
        for item in items {
            match Href::join_absolute(&self.url, &item.href) {
                Ok(url) => {
                    pool.submit(GetJob { url, item });
                    submitted += 1;
                }
                Err(e) => {
                    logger.warn(format_args!("bad item URL {}: {e}", item.href));
                    counts.failed += 1;
                }
            }
        }

        let mut tx = self.sync.store.begin().map_err(Error::partial)?;
        let mut pending = 0usize;
        for _ in 0..submitted {
            let Ok(reply) = pool.results().recv() else {
                break;
            };
            let href = reply.item.href.as_str();
            let body = match reply.result {
                Err(e) => {
                    logger.warn(format_args!("GET {href}: {e}"));
                    counts.failed += 1;
                    continue;
                }
                Ok(body) if DavOutcome::is_gone(body.status) => continue,
                Ok(body) if DavOutcome::is_error(body.status) => {
                    logger.warn(format_args!("GET {href} returned HTTP {}", body.status));
                    counts.failed += 1;
                    continue;
                }
                Ok(body) => body,
            };
            let etag = body.etag.as_deref().unwrap_or(&reply.item.etag);
            let raw = match str::from_utf8(&body.body) {
                Ok(raw) => raw,
                Err(e) => {
                    logger.warn(format_args!(
                        "GET {href} returned a body that is not UTF-8: {e}"
                    ));
                    counts.failed += 1;
                    continue;
                }
            };
            if let Err(e) = self.write(tx.store(), href, etag, raw) {
                logger.warn(format_args!("item {href}: {e}"));
                counts.failed += 1;
                continue;
            }
            counts.fetched += 1;
            pending += 1;
            if pending >= COMMIT_BATCH {
                tx.commit().map_err(Error::partial)?;
                tx = self.sync.store.begin().map_err(Error::partial)?;
                pending = 0;
            }
        }
        tx.commit().map_err(Error::partial)
    }

    fn write(&self, store: Store<'_>, href: &str, etag: &str, raw: &str) -> Result<(), Error> {
        let existing = self.local.get(href).map(|item| item.local);
        let local = match self.kind {
            CollectionKind::Calendar => self.write_event(store, existing, href, raw),
            CollectionKind::AddressBook => self.write_card(store, existing, href, raw),
        }?;
        let ids = store.dav_ids(self.sync.source);
        match existing {
            None => ids.insert(self.kind.item(), self.href, href, etag, local),
            Some(_) => ids.update_etag(self.kind.item(), href, etag),
        }
        .map_err(Error::partial)
    }

    fn write_event(
        &self,
        store: Store<'_>,
        existing: Option<LocalId>,
        href: &str,
        raw: &str,
    ) -> Result<LocalId, Error> {
        let entries = JsCalendarEntry::parse_all(raw)
            .map_err(|e| Error::Partial(format!("iCalendar parse: {e}")))?;
        let total = entries.len();
        let Some(mut first) = entries.into_iter().next() else {
            return Err(Error::Partial(
                "iCalendar contained no parseable entries".to_owned(),
            ));
        };
        let logger = self.sync.logger;
        if total > 1 && logger.enabled(Level::Default) {
            logger.warn(format_args!(
                "{href}: iCalendar resource has {total} entries; storing the first ({}), \
                 others dropped (recurrence overrides on the master are preserved)",
                first.data_type.as_str(),
            ));
        }
        let ExtractedFields {
            is_draft,
            use_default_alerts,
            ..
        } = first.take_extracted();
        let data = first.data.to_string();
        let record = CalendarEventRecord {
            calendars: &[self.container],
            is_draft,
            use_default_alerts,
            data_type: first.data_type,
            data: &data,
        };
        let events = store.calendar_events();
        match existing {
            Some(id) => events.update(id, &record).map(|_| id),
            None => events.insert(&record),
        }
        .map_err(Error::partial)
    }

    fn write_card(
        &self,
        store: Store<'_>,
        existing: Option<LocalId>,
        href: &str,
        raw: &str,
    ) -> Result<LocalId, Error> {
        let card = JsContact::from_vcard(raw, href)
            .map_err(|e| Error::Partial(format!("vCard parse: {e}")))?;
        let data = card.data.to_string();
        let record = ContactCardRecord {
            uid: &card.uid,
            address_books: &[self.container],
            data: &data,
        };
        let cards = store.contact_cards();
        match existing {
            Some(id) => cards.update(id, &record).map(|_| id),
            None => cards.insert(&record),
        }
        .map_err(Error::partial)
    }

    fn sweep(&self, vanished: &[(String, LocalId)], counts: &mut TypeCounts) -> Result<(), Error> {
        let source = self.sync.source;
        let item = self.kind.item();
        let sweep = VanishedSweep::new(self.sync.store, OnRowError::Abort, Children::Restrict);
        for row in vanished {
            sweep
                .run(
                    item,
                    slice::from_ref(row),
                    counts,
                    |store, href, _| store.dav_ids(source).delete(item, href),
                    |_, _, _| {},
                )
                .map_err(Error::partial)?;
        }
        Ok(())
    }
}

impl CollectionKind {
    fn multiget(self) -> Multiget {
        match self {
            CollectionKind::Calendar => Multiget::Calendar,
            CollectionKind::AddressBook => Multiget::Addressbook,
        }
    }

    fn data_field(self) -> &'static str {
        match self {
            CollectionKind::Calendar => "calendar_data",
            CollectionKind::AddressBook => "address_data",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Archive;
    use crate::archive::rows::{CalendarEventRecord, EventDataType};
    use crate::archive::sources::{SourceKey, SourceKind};
    use crate::net::auth::Auth;
    use crate::net::http::{ClientOptions, TlsPolicy};
    use crate::net::retry::RetryPolicy;
    use crate::types::ObjectType;

    #[test]
    fn vanished_item_delete_failure_aborts_the_collection_after_committing_earlier_rows() {
        let archive = Archive::open_in_memory().unwrap();
        let store = archive.store();
        let source = store
            .sources()
            .upsert(
                &SourceKey {
                    kind: SourceKind::CalDav,
                    url: "https://x",
                    account_id: "u",
                },
                Some("u"),
                "u",
            )
            .unwrap();
        let mut vanished = Vec::new();
        for name in ["a", "b", "c"] {
            let id = store
                .calendar_events()
                .insert(&CalendarEventRecord {
                    calendars: &[],
                    is_draft: false,
                    use_default_alerts: false,
                    data_type: EventDataType::Event,
                    data: "{}",
                })
                .unwrap();
            let href = format!("/cal/work/{name}.ics");
            store
                .dav_ids(source)
                .insert(ObjectType::CalendarEvent, "/cal/work/", &href, "\"v\"", id)
                .unwrap();
            vanished.push((href, id));
        }
        archive
            .connection()
            .execute_batch(&format!(
                "CREATE TRIGGER refuse BEFORE DELETE ON calendar_events WHEN old.id = {}
                 BEGIN SELECT RAISE(ABORT, 'refused'); END",
                vanished[1].1
            ))
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
        let sync = ItemSync {
            client: &client,
            store,
            source,
            base_url: "https://x",
            multiget_batch: 1,
            dav_connections: 1,
            logger,
        };
        let items = CollectionItems {
            sync: &sync,
            kind: CollectionKind::Calendar,
            href: "/cal/work/",
            container: LocalId::from_raw(1),
            url: String::new(),
            local: HashMap::new(),
        };
        let mut counts = TypeCounts::default();

        let err = items.sweep(&vanished, &mut counts).unwrap_err();

        assert!(matches!(err, Error::Partial(_)), "{err:?}");
        assert_eq!((counts.deleted, counts.failed), (1, 0));
        let ids = store.dav_ids(source);
        let left: Vec<bool> = vanished
            .iter()
            .map(|(href, _)| {
                ids.local_id(ObjectType::CalendarEvent, href)
                    .unwrap()
                    .is_some()
            })
            .collect();
        assert_eq!(left, [false, true, true]);
    }
}
