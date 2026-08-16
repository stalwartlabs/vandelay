/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::Connection;

use crate::dav::client::DavClient;
use crate::dav::discover::{DavKind, Discovery, DiscoveryError, discover};
use crate::db;
use crate::db::sources::SourceKey;
use crate::error::Error;
use crate::jmap::http::{Auth, RetryPolicy};
use crate::logging::{LEVEL_DEFAULT, LEVEL_PROGRESS, Logger};
use crate::sync::{CommonConfig, RunOutcome, Summary, TypeCounts};

use super::collections;
use super::items;
use super::tree;

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

    pub fn source_kind(self) -> &'static str {
        match self {
            DavKindArg::Caldav => "caldav",
            DavKindArg::Carddav => "carddav",
            DavKindArg::Webdav => "webdav",
        }
    }
}

#[derive(Debug, Clone)]
pub enum DavAuth {
    Basic { user: String, password: String },
    Bearer { token: String },
}

impl DavAuth {
    pub fn to_jmap_auth(&self) -> Auth {
        match self {
            DavAuth::Basic { user, password } => Auth::Basic {
                user: user.clone(),
                password: password.clone(),
            },
            DavAuth::Bearer { token } => Auth::Bearer {
                token: token.clone(),
            },
        }
    }

    pub fn username(&self) -> String {
        match self {
            DavAuth::Basic { user, .. } => user.clone(),
            DavAuth::Bearer { .. } => "(bearer)".to_owned(),
        }
    }
}

#[derive(Debug)]
pub struct DavImportConfig {
    pub kind: DavKindArg,
    pub url: String,
    pub auth: DavAuth,
    pub allow_cleartext: bool,
    pub dav_connections: usize,
    pub multiget_batch: usize,
    pub allow_source_change: bool,
}

pub fn run(common: CommonConfig, config: DavImportConfig) -> Result<Summary, Error> {
    run_reporting(common, config).into_result()
}

pub fn run_reporting(common: CommonConfig, config: DavImportConfig) -> RunOutcome {
    let mut summary = Summary::default();
    let error = run_into(common, config, &mut summary).err();
    RunOutcome { summary, error }
}

fn run_into(
    common: CommonConfig,
    config: DavImportConfig,
    summary: &mut Summary,
) -> Result<(), Error> {
    let logger = common.logger;
    enforce_tls_policy(&config.url, config.allow_cleartext)?;

    let mut conn = db::init::open(&common.archive)?;

    let client = DavClient::new(
        config.auth.to_jmap_auth(),
        RetryPolicy::new(common.max_retries),
        common.allow_invalid_certs,
    );
    client.set_logger(logger);

    let discovery =
        discover(&client, config.kind.kind(), &config.url).map_err(map_discovery_error)?;

    let session_url = normalise_base_url(&config.url);
    let account_id_raw = discovery
        .principal_url
        .clone()
        .unwrap_or_else(|| discovery.home_set_url.clone());
    let account_id = normalise_account_url(&account_id_raw);

    let key = SourceKey {
        kind: config.kind.source_kind().to_owned(),
        session_url: session_url.clone(),
        account_id: account_id.clone(),
    };

    if !common.dry_run
        && let Some((url, acc)) = db::sources::conflicting_source(
            &conn,
            config.kind.source_kind(),
            &session_url,
            &account_id,
        )
        .map_err(|e| Error::Partial(e.to_string()))?
        && !config.allow_source_change
    {
        return Err(Error::SourceChange(format!(
            "archive already records {} source ({url}, account {acc}); \
             pass --allow-source-change to import a different account",
            config.kind.source_kind()
        )));
    }

    if logger.enabled(LEVEL_PROGRESS) {
        eprintln!(
            "DAV discovery: principal={:?} home_set={} collections={}",
            discovery.principal_url,
            discovery.home_set_url,
            discovery.collections.len()
        );
    }

    if common.dry_run {
        let result = run_dry_diff(&conn, &client, &discovery, &config, logger, summary);
        summary.retries_observed = client.retries_observed();
        summary.retry_after_sleeps = client.retry_after_sleeps();
        return result;
    }

    let username = config.auth.username();
    let source_id = db::sources::upsert_source(&conn, &key, Some(&account_id), &username)
        .map_err(|e| Error::Partial(e.to_string()))?;

    let phase = match config.kind {
        DavKindArg::Caldav => {
            run_caldav(&mut conn, &client, source_id, &discovery, &config, logger)
        }
        DavKindArg::Carddav => {
            run_carddav(&mut conn, &client, source_id, &discovery, &config, logger)
        }
        DavKindArg::Webdav => {
            run_webdav(&mut conn, &client, source_id, &discovery, &config, logger)
        }
    };

    *summary = phase.summary;
    summary.retries_observed = client.retries_observed();
    summary.retry_after_sleeps = client.retry_after_sleeps();
    if let Some(e) = phase.error {
        return Err(e);
    }

    if !summary.any_failed()
        && let Err(e) = run_gc(&conn)
    {
        logger.warn(&format!("blob GC skipped: {e}"));
    }

    Ok(())
}

type ReconcileCollections = fn(
    &mut Connection,
    i64,
    &[crate::dav::discover::DiscoveredCollection],
    &mut TypeCounts,
    Logger,
) -> Result<Vec<(String, i64)>, Error>;

type ReconcileItems =
    fn(&mut Connection, &items::ItemRunCtx<'_>, &str, i64, &mut TypeCounts) -> Result<(), Error>;

struct ItemPhase {
    container_label: &'static str,
    item_label: &'static str,
    reconcile_collections: ReconcileCollections,
    reconcile_items: ReconcileItems,
}

fn run_collection_phase(
    conn: &mut Connection,
    client: &DavClient,
    source_id: i64,
    discovery: &Discovery,
    config: &DavImportConfig,
    logger: Logger,
    phase: ItemPhase,
) -> RunOutcome {
    let mut summary = Summary::default();
    let mut counts = PhaseCounts::default();
    let error = collection_phase_into(
        conn,
        client,
        source_id,
        PhaseInput {
            discovery,
            config,
            logger,
            phase: &phase,
        },
        &mut counts,
    )
    .err();

    summary
        .per_type
        .push((phase.container_label, counts.container));
    summary.per_type.push((phase.item_label, counts.items));
    RunOutcome { summary, error }
}

#[derive(Default)]
struct PhaseCounts {
    container: TypeCounts,
    items: TypeCounts,
}

struct PhaseInput<'a> {
    discovery: &'a Discovery,
    config: &'a DavImportConfig,
    logger: Logger,
    phase: &'a ItemPhase,
}

fn collection_phase_into(
    conn: &mut Connection,
    client: &DavClient,
    source_id: i64,
    input: PhaseInput<'_>,
    counts: &mut PhaseCounts,
) -> Result<(), Error> {
    let PhaseInput {
        discovery,
        config,
        logger,
        phase,
    } = input;

    let upserted = (phase.reconcile_collections)(
        conn,
        source_id,
        &discovery.collections,
        &mut counts.container,
        logger,
    )?;
    if logger.enabled(LEVEL_DEFAULT) {
        eprintln!(
            "import: {} done (upserted={} deleted={} failed={})",
            phase.container_label,
            counts.container.created + counts.container.fetched,
            counts.container.deleted,
            counts.container.failed
        );
    }

    let ctx = items::ItemRunCtx {
        client,
        source_id,
        base_url: &discovery.home_set_url,
        multiget_batch: config.multiget_batch,
        dav_connections: config.dav_connections,
        logger,
    };
    for (collection_href, local_id) in &upserted {
        match (phase.reconcile_items)(conn, &ctx, collection_href, *local_id, &mut counts.items) {
            Ok(()) => {}
            Err(e) if e.aborts_run() => return Err(e),
            Err(e) => {
                logger.warn(&format!(
                    "{} {collection_href:?}: items failed: {e}",
                    phase.container_label
                ));
                counts.items.failed += 1;
            }
        }
    }
    Ok(())
}

fn run_dry_diff(
    conn: &rusqlite::Connection,
    client: &DavClient,
    discovery: &Discovery,
    config: &DavImportConfig,
    logger: Logger,
    summary: &mut Summary,
) -> Result<(), Error> {
    use crate::dav::href::join_absolute;
    use crate::dav::xml;
    use crate::db::dav_ids;
    let (container_label, item_label, container_type, item_type) = match config.kind {
        DavKindArg::Caldav => (
            "calendar",
            "calendarevent",
            dav_ids::CALENDAR,
            dav_ids::CALENDAR_EVENT,
        ),
        DavKindArg::Carddav => (
            "addressbook",
            "contactcard",
            dav_ids::ADDRESS_BOOK,
            dav_ids::CONTACT_CARD,
        ),
        DavKindArg::Webdav => (
            "filenode",
            "filenode",
            dav_ids::FILE_NODE,
            dav_ids::FILE_NODE,
        ),
    };

    let mut container_counts = TypeCounts::default();
    let mut item_counts = TypeCounts::default();
    container_counts.created = discovery.collections.len() as u64;
    if logger.enabled(LEVEL_DEFAULT) {
        eprintln!(
            "dry-run: {} new={} (no archive to diff against)",
            container_label,
            discovery.collections.len(),
        );
    }

    if !matches!(config.kind, DavKindArg::Webdav) {
        for coll in &discovery.collections {
            let url = join_absolute(&discovery.home_set_url, coll.href.as_str())
                .map_err(|e| Error::Partial(e.to_string()))?;
            match client
                .propfind_responses(&url, 1, &xml::propfind_dav_items(), &url)
                .map_err(super::per_collection_failure)
            {
                Ok(ms) => {
                    let new_count = ms
                        .responses
                        .iter()
                        .filter(|r| !r.props.is_collection)
                        .count();
                    item_counts.created += new_count as u64;
                    if logger.enabled(LEVEL_DEFAULT) {
                        eprintln!("  {} items: {new_count}", coll.href.as_str());
                    }
                }
                Err(e) if e.aborts_run() => return Err(e),
                Err(e) => {
                    logger.warn(&format!(
                        "{container_label} {:?}: enumeration failed: {e}",
                        coll.href.as_str()
                    ));
                    item_counts.failed += 1;
                }
            }
        }
    } else if let Some(root) = discovery.collections.first() {
        let url = join_absolute(&discovery.home_set_url, root.href.as_str())
            .map_err(|e| Error::Partial(e.to_string()))?;
        match client
            .propfind_responses(&url, 1, &xml::propfind_webdav_listing(), &url)
            .map_err(super::per_collection_failure)
        {
            Ok(ms) => {
                let new_count = ms
                    .responses
                    .iter()
                    .filter(|r| !r.props.is_collection)
                    .count();
                item_counts.created += new_count as u64;
                if logger.enabled(LEVEL_DEFAULT) {
                    eprintln!("  root {} files: {new_count}", root.href.as_str());
                }
            }
            Err(e) if e.aborts_run() => return Err(e),
            Err(e) => {
                logger.warn(&format!(
                    "{container_label} {:?}: enumeration failed: {e}",
                    root.href.as_str()
                ));
                container_counts.failed += 1;
            }
        }
    }

    let _ = container_type;
    let _ = item_type;
    let _ = conn;
    summary.per_type.push((container_label, container_counts));
    if !matches!(config.kind, DavKindArg::Webdav) {
        summary.per_type.push((item_label, item_counts));
    }
    Ok(())
}

fn run_caldav(
    conn: &mut Connection,
    client: &DavClient,
    source_id: i64,
    discovery: &Discovery,
    config: &DavImportConfig,
    logger: Logger,
) -> RunOutcome {
    run_collection_phase(
        conn,
        client,
        source_id,
        discovery,
        config,
        logger,
        ItemPhase {
            container_label: "calendar",
            item_label: "calendarevent",
            reconcile_collections: collections::reconcile_calendars,
            reconcile_items: items::reconcile_calendar_events,
        },
    )
}

fn run_carddav(
    conn: &mut Connection,
    client: &DavClient,
    source_id: i64,
    discovery: &Discovery,
    config: &DavImportConfig,
    logger: Logger,
) -> RunOutcome {
    run_collection_phase(
        conn,
        client,
        source_id,
        discovery,
        config,
        logger,
        ItemPhase {
            container_label: "addressbook",
            item_label: "contactcard",
            reconcile_collections: collections::reconcile_address_books,
            reconcile_items: items::reconcile_contact_cards,
        },
    )
}

fn run_webdav(
    conn: &mut Connection,
    client: &DavClient,
    source_id: i64,
    discovery: &Discovery,
    config: &DavImportConfig,
    logger: Logger,
) -> RunOutcome {
    let mut summary = Summary::default();
    let mut file_counts = TypeCounts::default();

    if discovery.collections.is_empty() {
        summary.per_type.push(("filenode", file_counts));
        return RunOutcome {
            summary,
            error: None,
        };
    }
    let root = &discovery.collections[0];
    let ctx = tree::WebDavCtx {
        client,
        source_id,
        base_url: &discovery.home_set_url,
        dav_connections: config.dav_connections,
        logger,
    };
    let error = tree::reconcile_filenodes(conn, &ctx, root, &mut file_counts).err();

    summary.per_type.push(("filenode", file_counts));
    RunOutcome { summary, error }
}

fn map_discovery_error(err: DiscoveryError) -> Error {
    match err {
        DiscoveryError::NotFound { url } => Error::Usage(format!(
            "no DAV collections found under --url {url:?}; \
             check the URL and that the account has access"
        )),
        DiscoveryError::Transport(e) => Error::from(e),
        DiscoveryError::Parse(e) => {
            Error::Connection(format!("DAV discovery: malformed multistatus: {e}"))
        }
        DiscoveryError::Href(e) => Error::Connection(format!("DAV discovery: bad href: {e}")),
    }
}

fn run_gc(conn: &Connection) -> Result<(), Error> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| Error::Partial(e.to_string()))?;
    db::blobs::gc_orphan_blobs(&tx).map_err(|e| Error::Partial(e.to_string()))?;
    tx.commit().map_err(|e| Error::Partial(e.to_string()))?;
    Ok(())
}

fn enforce_tls_policy(url: &str, allow_cleartext: bool) -> Result<(), Error> {
    let parsed =
        url::Url::parse(url).map_err(|e| Error::Usage(format!("invalid --url {url:?}: {e}")))?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" => {
            if allow_cleartext {
                Ok(())
            } else {
                Err(Error::Connection(
                    "http:// URL requires --allow-cleartext".to_owned(),
                ))
            }
        }
        other => Err(Error::Usage(format!(
            "--url scheme must be http or https, got {other}"
        ))),
    }
}

fn normalise_base_url(url: &str) -> String {
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return url.to_owned(),
    };
    let scheme = parsed.scheme();
    let host = parsed.host_str().unwrap_or("");
    let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    format!("{scheme}://{host}{port}")
}

fn normalise_account_url(url: &str) -> String {
    let mut parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return url.to_owned(),
    };
    parsed.set_query(None);
    parsed.set_fragment(None);
    let path = parsed.path().to_owned();
    if !path.ends_with('/') {
        parsed.set_path(&format!("{path}/"));
    }
    parsed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_url_is_accepted_without_allow_cleartext() {
        assert!(enforce_tls_policy("https://x/", false).is_ok());
    }

    #[test]
    fn http_url_requires_allow_cleartext() {
        let err = enforce_tls_policy("http://x/", false).unwrap_err();
        assert!(matches!(err, Error::Connection(_)));
        assert!(enforce_tls_policy("http://x/", true).is_ok());
    }

    #[test]
    fn ftp_url_rejected_as_usage() {
        let err = enforce_tls_policy("ftp://x/", false).unwrap_err();
        assert!(matches!(err, Error::Usage(_)));
    }

    #[test]
    fn normalise_base_url_strips_path() {
        assert_eq!(
            normalise_base_url("https://dav.example.com/cal/"),
            "https://dav.example.com"
        );
    }

    #[test]
    fn normalise_base_url_keeps_explicit_port() {
        assert_eq!(
            normalise_base_url("https://dav.example.com:8443/cal/"),
            "https://dav.example.com:8443"
        );
    }
}
