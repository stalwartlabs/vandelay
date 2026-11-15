/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt::Write;

use url::Url;

use crate::archive::sources::{SourceChange, SourceKey, SourceKind};
use crate::archive::{Archive, SourceId};
use crate::console::{Counted, Grouped};
use crate::dav::client::DavClient;
use crate::dav::discover::{DavKind, Discovery, DiscoveryError};
use crate::dav::href::Href;
use crate::dav::xml::Propfind;
use crate::error::Error;
use crate::logging::Logger;
use crate::net::auth::Auth;
use crate::sync::run::{GcFailure, ImportRun, SourceLabel};
use crate::sync::{RunMode, RunOptions, RunOutcome, Summary, TypeCounts};
use crate::types::ObjectType;

use super::collections::{CollectionKind, CollectionSync};
use super::items::ItemSync;
use super::replace::Removed;
use super::tree::FileTree;

const BEARER_USERNAME: &str = "(bearer)";

#[derive(Debug, Clone, Copy)]
pub enum DavKindArg {
    Caldav,
    Carddav,
    Webdav,
}

impl DavKindArg {
    pub fn kind(self) -> DavKind {
        match self {
            DavKindArg::Caldav => DavKind::Caldav,
            DavKindArg::Carddav => DavKind::Carddav,
            DavKindArg::Webdav => DavKind::Webdav,
        }
    }

    pub fn source_kind(self) -> SourceKind {
        match self {
            DavKindArg::Caldav => SourceKind::CalDav,
            DavKindArg::Carddav => SourceKind::CardDav,
            DavKindArg::Webdav => SourceKind::WebDav,
        }
    }
}

#[derive(Debug)]
pub struct DavImport {
    pub kind: DavKindArg,
    pub url: String,
    pub auth: Auth,
    pub allow_cleartext: bool,
    pub dav_connections: usize,
    pub multiget_batch: usize,
    pub source_change: SourceChange,
}

impl DavImport {
    pub fn run(self, options: RunOptions) -> RunOutcome {
        let failed = |error: Error| RunOutcome {
            summary: Summary::default(),
            error: Some(error),
        };
        if let Err(e) = self.check_scheme() {
            return failed(e);
        }
        let archive = match Archive::open(&options.archive) {
            Ok(archive) => archive,
            Err(e) => return failed(e.into()),
        };
        let client = DavClient::new(self.auth.clone(), options.client_options());
        let discovery = match Discovery::run(&client, self.kind.kind(), &self.url) {
            Ok(discovery) => discovery,
            Err(e) => return failed(e.into()),
        };
        let session_url = SourceIdentity::base_url(&self.url);
        let account_id = SourceIdentity::account_url(
            discovery
                .principal_url
                .as_deref()
                .unwrap_or(&discovery.home_set_url),
        );
        let key = SourceKey {
            kind: self.kind.source_kind(),
            url: &session_url,
            account_id: &account_id,
        };
        let username = self.username();
        let label = SourceLabel {
            name: Some(&account_id),
            username: &username,
        };
        let logger = options.logger;
        let report = || {
            logger.progress(format_args!(
                "Found {} under {} (principal {})",
                Counted::new(
                    discovery.collections.len() as u64,
                    "collection",
                    "collections"
                ),
                discovery.home_set_url,
                discovery.principal_url.as_deref().unwrap_or("unknown")
            ))
        };
        let replace = self.source_change == SourceChange::Replace;
        let mut removed = Removed::default();
        if replace {
            report();
            if options.mode == RunMode::Apply {
                removed = match Removed::other_sources(archive.store(), &key, logger) {
                    Ok(removed) => removed,
                    Err(e) => return failed(e),
                };
            }
        }
        let run = match ImportRun::start_on(archive, options, &key, label, self.source_change) {
            Ok(run) => run,
            Err(e) => return failed(e),
        };
        if !replace {
            report();
        }
        let mut dav = DavRun {
            run,
            client,
            discovery,
            config: self,
        };
        let result = dav.reconcile(&key, removed);
        let stats = dav.client.stats();
        match result {
            Ok(()) => dav.run.finish(stats, GcFailure::WarnAsPartial),
            Err(e) => dav.run.abort(e, stats),
        }
    }

    fn username(&self) -> String {
        match &self.auth {
            Auth::Basic { user, .. } => user.clone(),
            Auth::Bearer { .. } => BEARER_USERNAME.to_owned(),
        }
    }

    fn check_scheme(&self) -> Result<(), Error> {
        let url = &self.url;
        let parsed =
            Url::parse(url).map_err(|e| Error::Usage(format!("invalid --url {url:?}: {e}")))?;
        match parsed.scheme() {
            "https" => Ok(()),
            "http" if self.allow_cleartext => Ok(()),
            "http" => Err(Error::Connection(
                "http:// URL requires --allow-cleartext".to_owned(),
            )),
            other => Err(Error::Usage(format!(
                "--url scheme must be http or https, got {other}"
            ))),
        }
    }
}

impl From<DiscoveryError> for Error {
    fn from(err: DiscoveryError) -> Error {
        match err {
            DiscoveryError::NotFound { url } => Error::Usage(format!(
                "no DAV collections found under --url {url:?}; \
                 check the URL and that the account has access"
            )),
            DiscoveryError::Transport(e) => Error::from(e),
            DiscoveryError::Href(e) => Error::Connection(format!("DAV discovery: bad href: {e}")),
        }
    }
}

struct SourceIdentity;

impl SourceIdentity {
    fn base_url(url: &str) -> String {
        let Ok(parsed) = Url::parse(url) else {
            return url.to_owned();
        };
        let mut base = format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or(""));
        if let Some(port) = parsed.port() {
            let _ = write!(base, ":{port}");
        }
        base
    }

    fn account_url(url: &str) -> String {
        let Ok(mut parsed) = Url::parse(url) else {
            return url.to_owned();
        };
        parsed.set_query(None);
        parsed.set_fragment(None);
        if !parsed.path().ends_with('/') {
            let path = format!("{}/", parsed.path());
            parsed.set_path(&path);
        }
        parsed.into()
    }
}

#[derive(Default)]
struct PhaseCounts {
    container: TypeCounts,
    items: TypeCounts,
}

struct DavRun {
    run: ImportRun,
    client: DavClient,
    discovery: Discovery,
    config: DavImport,
}

impl DavRun {
    fn logger(&self) -> Logger {
        self.run.options().logger
    }

    fn phase(&self) -> Option<CollectionKind> {
        match self.config.kind {
            DavKindArg::Caldav => Some(CollectionKind::Calendar),
            DavKindArg::Carddav => Some(CollectionKind::AddressBook),
            DavKindArg::Webdav => None,
        }
    }

    fn reconcile(&mut self, key: &SourceKey<'_>, removed: Removed) -> Result<(), Error> {
        if self.run.options().mode == RunMode::DryRun {
            if self.config.source_change == SourceChange::Replace {
                let logger = self.logger();
                for other in self.run.store().sources().others(key)? {
                    logger.info(format_args!(
                        "Would remove the {} source {} (account {})",
                        key.kind.label(),
                        other.url,
                        other.account_id
                    ));
                }
            }
            return self.dry_run();
        }
        let result = match self.phase() {
            Some(phase) => self.collections(phase),
            None => self.files(),
        };
        removed.add_to(self.run.summary());
        result
    }

    fn source(&self) -> Result<SourceId, Error> {
        self.run
            .claim()
            .id()
            .ok_or_else(|| Error::partial("DAV source is not registered"))
    }

    fn collections(&mut self, kind: CollectionKind) -> Result<(), Error> {
        let mut counts = PhaseCounts::default();
        let result = self.collections_into(kind, &mut counts);
        *self.run.counts(kind.container()) = counts.container;
        *self.run.counts(kind.item()) = counts.items;
        result
    }

    fn collections_into(
        &self,
        kind: CollectionKind,
        counts: &mut PhaseCounts,
    ) -> Result<(), Error> {
        let logger = self.logger();
        let source = self.source()?;
        let store = self.run.store();
        let container = kind.container();
        let upserted = CollectionSync {
            store,
            source,
            logger,
        }
        .reconcile(kind, &self.discovery.collections, &mut counts.container)?;
        logger.info(format_args!(
            "{}: {} upserted, {} deleted, {} failed",
            container.label(),
            Grouped(counts.container.created + counts.container.fetched),
            Grouped(counts.container.deleted),
            Grouped(counts.container.failed)
        ));
        let items = ItemSync {
            client: &self.client,
            store,
            source,
            base_url: &self.discovery.home_set_url,
            multiget_batch: self.config.multiget_batch,
            dav_connections: self.config.dav_connections,
            logger,
        };
        for (href, local) in upserted {
            match items.reconcile(kind, href.as_str(), local, &mut counts.items) {
                Ok(()) => {}
                Err(e) if e.aborts_run() => return Err(e),
                Err(e) => {
                    logger.warn(format_args!(
                        "items of {} {} failed: {e}",
                        container.noun(),
                        href.as_str()
                    ));
                    counts.items.failed += 1;
                }
            }
        }
        Ok(())
    }

    fn files(&mut self) -> Result<(), Error> {
        let mut file_counts = TypeCounts::default();
        let result = match self.discovery.collections.first() {
            Some(root) => FileTree {
                client: &self.client,
                store: self.run.store(),
                source: self.source()?,
                base_url: &self.discovery.home_set_url,
                dav_connections: self.config.dav_connections,
                logger: self.logger(),
            }
            .reconcile(root, &mut file_counts),
            None => Ok(()),
        };
        *self.run.counts(ObjectType::FileNode) = file_counts;
        result
    }

    fn dry_run(&mut self) -> Result<(), Error> {
        let mut counts = PhaseCounts::default();
        let mut plan = PhasePlan::default();
        let result = self.dry_run_into(&mut counts, &mut plan);
        let (container, item) = match self.phase() {
            Some(kind) => (kind.container(), Some(kind.item())),
            None => (ObjectType::FileNode, None),
        };
        let summary = self.run.summary();
        *summary.counts(container) = counts.container;
        if let Some(item) = item {
            *summary.counts(item) = counts.items;
        }
        if result.is_ok() {
            match item {
                Some(item) => {
                    summary.plan(container).new = plan.containers;
                    summary.plan(item).new = plan.items;
                }
                None => summary.plan(container).new = plan.items,
            }
        }
        result
    }

    fn dry_run_into(&self, counts: &mut PhaseCounts, plan: &mut PhasePlan) -> Result<(), Error> {
        let logger = self.logger();
        let discovery = &self.discovery;
        let container = self
            .phase()
            .map_or(ObjectType::FileNode, |kind| kind.container());
        plan.containers = discovery.collections.len() as u64;
        logger.note(format_args!(
            "a DAV dry run does not compare with the archive; every {} counts as new",
            container.noun()
        ));
        let (targets, listing, failed) = match self.config.kind {
            DavKindArg::Webdav => (
                discovery.collections.get(..1).unwrap_or_default(),
                Propfind::WebdavListing,
                &mut counts.container.failed,
            ),
            DavKindArg::Caldav | DavKindArg::Carddav => (
                discovery.collections.as_slice(),
                Propfind::DavItems,
                &mut counts.items.failed,
            ),
        };
        for coll in targets {
            let url = Href::join_absolute(&discovery.home_set_url, coll.href.as_str())
                .map_err(Error::partial)?;
            match self
                .client
                .propfind_responses(&url, 1, listing.body(), &url)
                .map_err(Error::partial)
            {
                Ok(ms) => {
                    plan.items += ms
                        .responses
                        .iter()
                        .filter(|r| !r.props.is_collection)
                        .count() as u64;
                }
                Err(e) if e.aborts_run() => return Err(e),
                Err(e) => {
                    logger.warn(format_args!(
                        "cannot list {} {}: {e}",
                        container.noun(),
                        coll.href.as_str()
                    ));
                    *failed += 1;
                }
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct PhasePlan {
    containers: u64,
    items: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn import(url: &str, allow_cleartext: bool) -> DavImport {
        DavImport {
            kind: DavKindArg::Caldav,
            url: url.to_owned(),
            auth: Auth::Bearer {
                token: "t".to_owned(),
            },
            allow_cleartext,
            dav_connections: 1,
            multiget_batch: 1,
            source_change: SourceChange::Reject,
        }
    }

    #[test]
    fn https_url_is_accepted_without_allow_cleartext() {
        assert!(import("https://x/", false).check_scheme().is_ok());
    }

    #[test]
    fn http_url_requires_allow_cleartext() {
        let err = import("http://x/", false).check_scheme().unwrap_err();
        assert!(matches!(err, Error::Connection(_)));
        assert!(import("http://x/", true).check_scheme().is_ok());
    }

    #[test]
    fn ftp_url_rejected_as_usage() {
        let err = import("ftp://x/", false).check_scheme().unwrap_err();
        assert!(matches!(err, Error::Usage(_)));
    }

    #[test]
    fn base_url_strips_path() {
        assert_eq!(
            SourceIdentity::base_url("https://dav.example.com/cal/"),
            "https://dav.example.com"
        );
    }

    #[test]
    fn base_url_keeps_explicit_port() {
        assert_eq!(
            SourceIdentity::base_url("https://dav.example.com:8443/cal/"),
            "https://dav.example.com:8443"
        );
    }

    #[test]
    fn account_url_drops_query_and_adds_trailing_slash() {
        assert_eq!(
            SourceIdentity::account_url("https://dav.example.com/p/alice?x=1#f"),
            "https://dav.example.com/p/alice/"
        );
    }
}
