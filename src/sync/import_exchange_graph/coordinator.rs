/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::Connection;
use serde_json::Value;

use crate::db;
use crate::db::sources::SourceKey;
use crate::error::Error;
use crate::exchange_graph::api::{DEFAULT_API_BASE, Endpoints};
use crate::exchange_graph::client::GraphClient;
use crate::exchange_graph::error::GraphError;
use crate::exchange_graph::oauth::{
    AcquiredToken, OAuthFlow, acquire, default_authority, refresh_access_token,
};
use crate::exchange_graph::types::{EventBodyFormat, MailboxKind, Surfaces, synthetic_account_id};
use crate::jmap::http::RetryPolicy;
use crate::logging::LEVEL_DEFAULT;
use crate::sync::{CommonConfig, Summary, TypeCounts};

#[derive(Debug, Clone)]
pub enum GraphAuth {
    PreAcquired {
        token: String,
    },
    DeviceCode {
        authority: String,
        client_id: String,
    },
}

#[derive(Debug, Clone)]
pub struct GraphImportConfig {
    pub auth: GraphAuth,
    pub api_base: String,
    pub user_target: Option<String>,
    pub mailbox_kind: MailboxKind,
    pub surfaces: Surfaces,
    pub event_body_format: EventBodyFormat,
    pub graph_connections: usize,
    pub top: usize,
    pub exception_window_years: i32,
    pub contact_photos: bool,
    pub event_attachments: bool,
    pub allow_source_change: bool,
}

pub const CHUNK_SIZE: usize = 100;

pub struct GraphCoordinator<'a> {
    pub client: &'a GraphClient,
    pub endpoints: &'a Endpoints,
    pub source_id: i64,
    pub top: usize,
    pub workers: usize,
    pub logger: crate::logging::Logger,
    pub event_body_format: EventBodyFormat,
    pub exception_window_years: i32,
    pub contact_photos: bool,
    pub event_attachments: bool,
}

pub fn run(common: CommonConfig, config: GraphImportConfig) -> Result<Summary, Error> {
    let logger = common.logger;
    let mut conn = db::init::open(&common.archive)?;

    let acquired = acquire_with_flow(&config.auth, common.allow_invalid_certs)?;
    let client = GraphClient::new(
        acquired.access_token.clone(),
        RetryPolicy::new(common.max_retries),
        common.allow_invalid_certs,
    );
    client.set_logger(logger);

    let endpoints = resolve_endpoints(&config, &client)?;
    let principal = resolve_principal(&client, &endpoints, acquired.upn.as_deref())?;

    let session_url = canonical_session_url(&config);
    let account_id = synthetic_account_id(&principal.id, config.mailbox_kind);

    if !common.dry_run
        && let Some((url, acc)) =
            db::sources::conflicting_source(&conn, "exchange_graph", &session_url, &account_id)?
        && !config.allow_source_change
    {
        return Err(Error::SourceChange(format!(
            "archive already records exchange_graph source ({url}, account {acc}); \
             pass --allow-source-change to import a different account"
        )));
    }

    if matches!(config.mailbox_kind, MailboxKind::Archive) {
        logger.warn(
            "--mailbox-kind archive: Microsoft Online Archive holds only mail; \
             calendar and contact surfaces are skipped for this run",
        );
    }

    if common.dry_run {
        return run_dry(
            &client,
            &endpoints,
            config.mailbox_kind,
            &logger,
            config.top,
        );
    }

    let source_id = db::sources::upsert_source(
        &conn,
        &SourceKey {
            kind: "exchange_graph".to_owned(),
            session_url: session_url.clone(),
            account_id: account_id.clone(),
        },
        Some(&principal.user_principal_name),
        &principal.user_principal_name,
    )?;

    let _refresher = spawn_token_refresher(
        &client,
        &config.auth,
        &acquired,
        common.allow_invalid_certs,
        logger,
    );

    let mut summary = Summary::default();
    let mut mailbox_counts = TypeCounts::default();
    let mut email_counts = TypeCounts::default();
    let mut calendar_counts = TypeCounts::default();
    let mut event_counts = TypeCounts::default();
    let mut addressbook_counts = TypeCounts::default();
    let mut contact_counts = TypeCounts::default();
    let mut file_counts = TypeCounts::default();

    let ctx = GraphCoordinator {
        client: &client,
        endpoints: &endpoints,
        source_id,
        top: config.top.clamp(1, 1000),
        workers: config.graph_connections.clamp(1, 16),
        logger,
        event_body_format: config.event_body_format,
        exception_window_years: config.exception_window_years,
        contact_photos: config.contact_photos,
        event_attachments: config.event_attachments,
    };

    let primary = !matches!(config.mailbox_kind, MailboxKind::Archive);
    let want_mail = config.surfaces.mail;
    let want_calendar = config.surfaces.calendar && primary;
    let want_contacts = config.surfaces.contacts && primary;
    let want_files = config.surfaces.files && primary;

    if want_mail {
        let folders = super::folders::reconcile_mail(
            &mut conn,
            &ctx,
            config.mailbox_kind,
            &mut mailbox_counts,
        )?;
        if logger.enabled(LEVEL_DEFAULT) {
            eprintln!(
                "graph mail folders: created={} fetched={} deleted={}",
                mailbox_counts.created, mailbox_counts.fetched, mailbox_counts.deleted
            );
        }
        super::messages::reconcile_all(&mut conn, &ctx, &folders, &mut email_counts)?;
    }

    if want_calendar {
        let calendars = super::folders::reconcile_calendars(&mut conn, &ctx, &mut calendar_counts)?;
        super::calendar::reconcile_all(&mut conn, &ctx, &calendars, &mut event_counts)?;
    }

    if want_contacts {
        let books =
            super::folders::reconcile_address_books(&mut conn, &ctx, &mut addressbook_counts)?;
        super::contacts::reconcile_all(&mut conn, &ctx, &books, &mut contact_counts)?;
    }

    if want_files {
        super::files::reconcile_all(&mut conn, &ctx, &mut file_counts)?;
    }

    summary.per_type.push(("mailbox", mailbox_counts));
    summary.per_type.push(("email", email_counts));
    summary.per_type.push(("calendar", calendar_counts));
    summary.per_type.push(("calendarevent", event_counts));
    summary.per_type.push(("addressbook", addressbook_counts));
    summary.per_type.push(("contactcard", contact_counts));
    summary.per_type.push(("filenode", file_counts));

    if !summary.any_failed()
        && let Err(e) = run_gc(&conn)
    {
        logger.warn(&format!("blob GC skipped: {e}"));
    }

    summary.retries_observed = client.retries_observed();
    summary.retry_after_sleeps = client.retry_after_sleeps();
    Ok(summary)
}

fn run_dry(
    client: &GraphClient,
    endpoints: &Endpoints,
    mailbox_kind: MailboxKind,
    logger: &crate::logging::Logger,
    top: usize,
) -> Result<Summary, Error> {
    let mut mailbox_counts = TypeCounts::default();
    let mut calendar_counts = TypeCounts::default();
    let mut addressbook_counts = TypeCounts::default();
    let folders =
        enumerate_mail_folders(client, endpoints, mailbox_kind, top).map_err(Error::from)?;
    mailbox_counts.created = folders.len() as u64;
    if !matches!(mailbox_kind, MailboxKind::Archive) {
        let calendars =
            crate::exchange_graph::api::collect_all_values(client, &endpoints.calendars(top), &[])
                .map_err(Error::from)?;
        calendar_counts.created = calendars.len() as u64;
        let books = crate::exchange_graph::api::collect_all_values(
            client,
            &endpoints.contact_folders(top),
            &[],
        )
        .map_err(Error::from)?;
        addressbook_counts.created = books.len() as u64;
    }
    if logger.enabled(LEVEL_DEFAULT) {
        eprintln!(
            "dry-run: mailbox={} calendar={} addressbook={}",
            mailbox_counts.created, calendar_counts.created, addressbook_counts.created
        );
    }
    let mut summary = Summary::default();
    summary.per_type.push(("mailbox", mailbox_counts));
    summary.per_type.push(("calendar", calendar_counts));
    summary.per_type.push(("addressbook", addressbook_counts));
    Ok(summary)
}

pub fn enumerate_mail_folders(
    client: &GraphClient,
    endpoints: &Endpoints,
    mailbox_kind: MailboxKind,
    top: usize,
) -> Result<Vec<Value>, GraphError> {
    let mut all = Vec::new();
    let mut frontier: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let initial = endpoints.mail_folders_root(mailbox_kind, top);
    let level = crate::exchange_graph::api::collect_all_values(client, &initial, &[])?;
    for folder in level {
        let Some(id) = folder.get("id").and_then(Value::as_str) else {
            continue;
        };
        if seen.insert(id.to_owned()) {
            frontier.push(id.to_owned());
            all.push(folder);
        }
    }
    while let Some(parent) = frontier.pop() {
        let url = endpoints.mail_folder_child_folders(&parent, top);
        let children = crate::exchange_graph::api::collect_all_values(client, &url, &[])?;
        for child in children {
            let Some(id) = child.get("id").and_then(Value::as_str) else {
                continue;
            };
            if seen.insert(id.to_owned()) {
                frontier.push(id.to_owned());
                all.push(child);
            }
        }
    }
    Ok(all)
}

fn acquire_with_flow(auth: &GraphAuth, allow_invalid_certs: bool) -> Result<AcquiredToken, Error> {
    let flow = match auth {
        GraphAuth::PreAcquired { token } => OAuthFlow::PreAcquired {
            token: token.clone(),
        },
        GraphAuth::DeviceCode {
            authority,
            client_id,
        } => OAuthFlow::DeviceCode {
            authority: authority.clone(),
            client_id: client_id.clone(),
        },
    };
    acquire(&flow, allow_invalid_certs).map_err(Error::from)
}

fn resolve_endpoints(config: &GraphImportConfig, client: &GraphClient) -> Result<Endpoints, Error> {
    if let Some(target) = config.user_target.as_deref() {
        let resolved = if looks_like_uuid(target) {
            target.to_owned()
        } else {
            resolve_user_id(client, &config.api_base, target)
                .map_err(|e| Error::Connection(format!("user resolution: {e}")))?
        };
        Ok(Endpoints::for_user(&config.api_base, &resolved))
    } else {
        Ok(Endpoints::for_me(&config.api_base))
    }
}

fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36
        && s.chars().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

fn resolve_user_id(
    client: &GraphClient,
    api_base: &str,
    upn_or_id: &str,
) -> Result<String, GraphError> {
    let url = format!(
        "{}/users/{}?$select=id,userPrincipalName",
        api_base.trim_end_matches('/'),
        upn_or_id
    );
    let body = client.get_json(&url)?;
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| GraphError::Malformed("user resolution missing id".to_owned()))?
        .to_owned();
    Ok(id)
}

fn resolve_principal(
    client: &GraphClient,
    endpoints: &Endpoints,
    fallback_upn: Option<&str>,
) -> Result<crate::exchange_graph::ResolvedPrincipal, Error> {
    let url = endpoints.me_select_id_upn();
    let value = client.get_json(&url).map_err(Error::from)?;
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Connection("graph principal has no id".to_owned()))?
        .to_owned();
    let upn = value
        .get("userPrincipalName")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| fallback_upn.map(str::to_owned))
        .unwrap_or_default();
    Ok(crate::exchange_graph::ResolvedPrincipal {
        id,
        user_principal_name: upn,
    })
}

fn canonical_session_url(config: &GraphImportConfig) -> String {
    let authority = match &config.auth {
        GraphAuth::DeviceCode { authority, .. } => authority.clone(),
        GraphAuth::PreAcquired { .. } => default_authority("common"),
    };
    format!(
        "{}|{}",
        authority.trim_end_matches('/'),
        config.api_base.trim_end_matches('/')
    )
}

fn spawn_token_refresher(
    client: &GraphClient,
    auth: &GraphAuth,
    initial: &AcquiredToken,
    allow_invalid_certs: bool,
    logger: crate::logging::Logger,
) -> Option<TokenRefresher> {
    let (authority, client_id) = match auth {
        GraphAuth::DeviceCode {
            authority,
            client_id,
        } => (authority.clone(), client_id.clone()),
        GraphAuth::PreAcquired { .. } => return None,
    };
    let refresh = initial.refresh_token.clone()?;
    let mut deadline_unix = initial_deadline_unix(initial)?;
    let client = client.clone();
    let mut refresh_token = refresh;
    let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown_signal = shutdown.clone();
    let handle = std::thread::Builder::new()
        .name("vandelay-graph-token-refresh".to_owned())
        .spawn(move || {
            while !shutdown_signal.load(std::sync::atomic::Ordering::Relaxed) {
                let now = unix_now();
                let refresh_at = deadline_unix.saturating_sub(60);
                if refresh_at > now {
                    let wait = std::time::Duration::from_secs(refresh_at - now);
                    let chunk = std::time::Duration::from_secs(2);
                    let mut left = wait;
                    while left > std::time::Duration::ZERO
                        && !shutdown_signal.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        let step = chunk.min(left);
                        std::thread::sleep(step);
                        left = left.saturating_sub(step);
                    }
                    if shutdown_signal.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                }
                match refresh_access_token(
                    &authority,
                    &client_id,
                    &refresh_token,
                    allow_invalid_certs,
                ) {
                    Ok(tok) => {
                        client.set_bearer(tok.access_token.clone());
                        if let Some(new_refresh) = tok.refresh_token {
                            refresh_token = new_refresh;
                        }
                        deadline_unix = unix_now()
                            + tok.expires_in.unwrap_or_else(|| {
                                crate::exchange_graph::oauth::decode_jwt_claims(&tok.access_token)
                                    .and_then(|c| c.exp)
                                    .and_then(|exp| exp.checked_sub(unix_now()))
                                    .unwrap_or(50 * 60)
                            });
                    }
                    Err(e) => {
                        logger.warn(&format!(
                            "graph token refresh failed: {e}; sleeping 60s before retry"
                        ));
                        std::thread::sleep(std::time::Duration::from_secs(60));
                    }
                }
            }
        })
        .ok()?;
    Some(TokenRefresher {
        shutdown,
        handle: Some(handle),
    })
}

pub struct TokenRefresher {
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for TokenRefresher {
    fn drop(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn initial_deadline_unix(initial: &AcquiredToken) -> Option<u64> {
    if let Some(expires_in) = initial.expires_in {
        return Some(unix_now() + expires_in);
    }
    crate::exchange_graph::oauth::decode_jwt_claims(&initial.access_token).and_then(|c| c.exp)
}

fn run_gc(conn: &Connection) -> Result<(), Error> {
    let tx = conn.unchecked_transaction()?;
    db::blobs::gc_orphan_blobs(&tx)?;
    tx.commit()?;
    Ok(())
}

pub fn default_api_base() -> String {
    DEFAULT_API_BASE.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_session_url_concatenates_authority_and_base() {
        let config = GraphImportConfig {
            auth: GraphAuth::DeviceCode {
                authority: "https://login.microsoftonline.com/common".to_owned(),
                client_id: "uuid".to_owned(),
            },
            api_base: "https://graph.microsoft.com/v1.0".to_owned(),
            user_target: None,
            mailbox_kind: MailboxKind::Primary,
            surfaces: Surfaces::ALL,
            event_body_format: EventBodyFormat::Text,
            graph_connections: 4,
            top: 100,
            exception_window_years: 5,
            contact_photos: true,
            event_attachments: true,
            allow_source_change: false,
        };
        let url = canonical_session_url(&config);
        assert_eq!(
            url,
            "https://login.microsoftonline.com/common|https://graph.microsoft.com/v1.0"
        );
    }

    #[test]
    fn looks_like_uuid_only_matches_well_formed() {
        assert!(looks_like_uuid("12345678-1234-1234-1234-123456789abc"));
        assert!(!looks_like_uuid("alice@x.com"));
        assert!(!looks_like_uuid("12345678123412341234123456789abc"));
    }
}
