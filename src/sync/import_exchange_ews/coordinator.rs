/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::Connection;

use crate::db;
use crate::db::sources::SourceKey;
use crate::error::Error;
use crate::exchange_ews::EwsClient;
use crate::exchange_ews::autodiscover::{DiscoveryResult, discover};
use crate::exchange_ews::oauth::{AcquiredToken, OAuthFlow, acquire};
use crate::exchange_ews::types::MailboxKind;
use crate::jmap::http::{Auth, RetryPolicy};
use crate::logging::{LEVEL_DEFAULT, LEVEL_PROGRESS};
use crate::sync::{CommonConfig, Summary, TypeCounts};

use super::folders::{self, plan_folders};
use super::{calendar, contacts, messages};

#[derive(Debug, Clone)]
pub enum EwsAuth {
    Basic { user: String, password: String },
    Bearer { token: String },
    OAuth(OAuthFlow),
}

#[derive(Debug, Clone)]
pub struct EwsImportConfig {
    pub url: Option<String>,
    pub mailbox: Option<String>,
    pub mailbox_kind: MailboxKind,
    pub auth: EwsAuth,
    pub ews_connections: usize,
    pub getitem_batch: usize,
    pub attachment_batch: usize,
    pub use_syncfolderitems: bool,
    pub allow_source_change: bool,
}

pub fn run(common: CommonConfig, config: EwsImportConfig) -> Result<Summary, Error> {
    let logger = common.logger;
    let mut conn = db::init::open(&common.archive)?;

    let (auth, acquired) = resolve_auth(&config.auth, common.allow_invalid_certs)?;
    let discovery = run_autodiscover(&config, &acquired, common.allow_invalid_certs)?;
    if logger.enabled(LEVEL_PROGRESS) {
        eprintln!(
            "EWS discovery: url={} source={:?}",
            discovery.ews_url, discovery.source
        );
    }
    let mailbox = resolve_mailbox(&config, &acquired)?;
    let account_id = synthetic_account_id(&mailbox, config.mailbox_kind);
    let session_url = normalise_ews_url(&discovery.ews_url);
    enforce_basic_auth_policy(&config.auth, &session_url)?;
    let key = SourceKey {
        kind: "exchange_ews".to_owned(),
        session_url: session_url.clone(),
        account_id: account_id.clone(),
    };

    if !common.dry_run
        && let Some((url, acc)) =
            db::sources::conflicting_source(&conn, "exchange_ews", &session_url, &account_id)?
        && !config.allow_source_change
    {
        return Err(Error::SourceChange(format!(
            "archive already records exchange_ews source ({url}, account {acc}); \
             pass --allow-source-change to import a different account"
        )));
    }

    let client = EwsClient::new(
        auth,
        RetryPolicy::new(common.max_retries),
        common.allow_invalid_certs,
    );
    client.set_logger(logger);
    if matches!(config.mailbox_kind, MailboxKind::PublicFolders) {
        client.set_anchor_mailbox(None);
    } else {
        client.set_anchor_mailbox(Some(mailbox.clone()));
    }
    if let EwsAuth::OAuth(OAuthFlow::ClientCredentials { .. }) = &config.auth {
        client.set_impersonation(Some(mailbox.clone()));
    }
    spawn_token_refresher(
        &client,
        &config.auth,
        &acquired,
        common.allow_invalid_certs,
        logger,
    );

    let username = match &config.auth {
        EwsAuth::Basic { user, .. } => user.clone(),
        EwsAuth::Bearer { .. } | EwsAuth::OAuth(_) => acquired
            .as_ref()
            .and_then(|a| a.upn.clone())
            .unwrap_or_else(|| mailbox.clone()),
    };

    if common.dry_run {
        let summary = run_dry(&conn, &client, &session_url, &config, logger)?;
        return Ok(summary);
    }

    let account_name = acquired.as_ref().and_then(|a| a.name.clone());
    let source_id = db::sources::upsert_source(&conn, &key, account_name.as_deref(), &username)?;

    let mut summary = Summary::default();
    let mut mailbox_counts = TypeCounts::default();
    let mut calendar_counts = TypeCounts::default();
    let mut addressbook_counts = TypeCounts::default();
    let mut email_counts = TypeCounts::default();
    let mut calendar_event_counts = TypeCounts::default();
    let mut contact_counts = TypeCounts::default();

    let plan = plan_folders(
        &client,
        &session_url,
        config.mailbox_kind,
        config.mailbox.as_deref(),
        logger,
    )
    .map_err(Error::from)?;

    folders::reconcile(
        &mut conn,
        source_id,
        &plan,
        &mut folders::ReconcileCounts {
            mailbox: &mut mailbox_counts,
            calendar: &mut calendar_counts,
            addressbook: &mut addressbook_counts,
            email: &mut email_counts,
            calendar_event: &mut calendar_event_counts,
            contact: &mut contact_counts,
        },
        logger,
    )?;

    if logger.enabled(LEVEL_DEFAULT) {
        eprintln!(
            "import: mailbox={} calendar={} addressbook={}",
            mailbox_counts.created + mailbox_counts.fetched,
            calendar_counts.created + calendar_counts.fetched,
            addressbook_counts.created + addressbook_counts.fetched,
        );
    }

    let item_ctx = super::items::ItemRunCtx {
        client: &client,
        url: &session_url,
        source_id,
        batch_size: config.getitem_batch.max(1),
        attachment_batch: config.attachment_batch.max(1),
        connections: config.ews_connections.clamp(1, 8),
        use_syncfolderitems: config.use_syncfolderitems,
        sync_batch: super::items::SYNC_BATCH_MAX,
        logger,
    };

    messages::reconcile_all(&mut conn, &item_ctx, &plan, &mut email_counts)?;
    contacts::reconcile_all(&mut conn, &item_ctx, &plan, &mut contact_counts)?;
    calendar::reconcile_all(&mut conn, &item_ctx, &plan, &mut calendar_event_counts)?;

    summary.per_type.push(("mailbox", mailbox_counts));
    summary.per_type.push(("calendar", calendar_counts));
    summary.per_type.push(("addressbook", addressbook_counts));
    summary.per_type.push(("email", email_counts));
    summary
        .per_type
        .push(("calendarevent", calendar_event_counts));
    summary.per_type.push(("contactcard", contact_counts));

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
    _conn: &Connection,
    client: &EwsClient,
    session_url: &str,
    config: &EwsImportConfig,
    logger: crate::logging::Logger,
) -> Result<Summary, Error> {
    let plan = plan_folders(
        client,
        session_url,
        config.mailbox_kind,
        config.mailbox.as_deref(),
        logger,
    )
    .map_err(Error::from)?;
    let mut summary = Summary::default();
    let mut mailbox_counts = TypeCounts::default();
    let mut calendar_counts = TypeCounts::default();
    let mut addressbook_counts = TypeCounts::default();
    mailbox_counts.created = plan.mail.len() as u64;
    calendar_counts.created = plan.calendar.len() as u64;
    addressbook_counts.created = plan.contacts.len() as u64;
    if logger.enabled(LEVEL_DEFAULT) {
        eprintln!(
            "dry-run: mailbox={} calendar={} addressbook={}",
            mailbox_counts.created, calendar_counts.created, addressbook_counts.created
        );
    }
    summary.per_type.push(("mailbox", mailbox_counts));
    summary.per_type.push(("calendar", calendar_counts));
    summary.per_type.push(("addressbook", addressbook_counts));
    Ok(summary)
}

fn resolve_auth(
    auth: &EwsAuth,
    allow_invalid_certs: bool,
) -> Result<(Auth, Option<AcquiredToken>), Error> {
    match auth {
        EwsAuth::Basic { user, password } => Ok((
            Auth::Basic {
                user: user.clone(),
                password: password.clone(),
            },
            None,
        )),
        EwsAuth::Bearer { token } => {
            let acq = acquire(
                &OAuthFlow::PreAcquired {
                    token: token.clone(),
                },
                allow_invalid_certs,
            )
            .map_err(Error::from)?;
            Ok((
                Auth::Bearer {
                    token: acq.access_token.clone(),
                },
                Some(acq),
            ))
        }
        EwsAuth::OAuth(flow) => {
            let acq = acquire(flow, allow_invalid_certs).map_err(Error::from)?;
            Ok((
                Auth::Bearer {
                    token: acq.access_token.clone(),
                },
                Some(acq),
            ))
        }
    }
}

fn run_autodiscover(
    config: &EwsImportConfig,
    acquired: &Option<AcquiredToken>,
    allow_invalid_certs: bool,
) -> Result<DiscoveryResult, Error> {
    let email = config
        .mailbox
        .clone()
        .or_else(|| acquired.as_ref().and_then(|a| a.upn.clone()));
    let result = discover(
        config.url.as_deref(),
        email.as_deref(),
        None,
        allow_invalid_certs,
    )
    .map_err(Error::from)?;
    Ok(result)
}

fn resolve_mailbox(
    config: &EwsImportConfig,
    acquired: &Option<AcquiredToken>,
) -> Result<String, Error> {
    if let Some(mb) = config.mailbox.as_ref() {
        return Ok(mb.clone());
    }
    if let Some(acq) = acquired.as_ref()
        && let Some(upn) = acq.upn.as_ref()
    {
        return Ok(upn.clone());
    }
    if let EwsAuth::Basic { user, .. } = &config.auth
        && user.contains('@')
    {
        return Ok(user.clone());
    }
    Err(Error::Account(
        "could not resolve mailbox SMTP; pass --mailbox".to_owned(),
    ))
}

pub fn synthetic_account_id(mailbox: &str, kind: MailboxKind) -> String {
    match kind {
        MailboxKind::Primary => mailbox.to_owned(),
        MailboxKind::Archive => format!("{mailbox}#archive"),
        MailboxKind::PublicFolders => {
            let domain = mailbox.split('@').nth(1).unwrap_or("");
            format!("__public_folders__@{domain}")
        }
    }
}

pub fn normalise_ews_url(raw: &str) -> String {
    let Ok(parsed) = url::Url::parse(raw) else {
        return raw.to_owned();
    };
    let scheme = parsed.scheme().to_ascii_lowercase();
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "https" => 443,
        "http" => 80,
        _ => 0,
    };
    let port = parsed.port().filter(|p| *p != default_port);
    let path = if parsed.path().is_empty() || parsed.path() == "/" {
        "/EWS/Exchange.asmx".to_owned()
    } else {
        let last = parsed.path().rsplit('/').next().unwrap_or("");
        if last.eq_ignore_ascii_case("Exchange.asmx") {
            let mut p = String::new();
            let mut parts: Vec<&str> = parsed.path().split('/').collect();
            if let Some(last_part) = parts.last_mut() {
                *last_part = "Exchange.asmx";
            }
            if let Some(second_last_idx) = parts.len().checked_sub(2)
                && parts[second_last_idx].eq_ignore_ascii_case("EWS")
            {
                parts[second_last_idx] = "EWS";
            }
            p.push_str(&parts.join("/"));
            p
        } else {
            parsed.path().to_owned()
        }
    };
    match port {
        Some(p) => format!("{scheme}://{host}:{p}{path}"),
        None => format!("{scheme}://{host}{path}"),
    }
}

fn enforce_basic_auth_policy(auth: &EwsAuth, url: &str) -> Result<(), Error> {
    if let EwsAuth::Basic { .. } = auth {
        let parsed = url::Url::parse(url)
            .map_err(|e| Error::Usage(format!("invalid EWS URL {url:?}: {e}")))?;
        if parsed.scheme() != "https" {
            return Err(Error::Connection(
                "EWS requires https:// (basic auth refused on cleartext)".to_owned(),
            ));
        }
        let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
        let blocked = [
            "outlook.office365.com",
            "outlook.office.com",
            "outlook.office365.us",
            "office365.us",
        ];
        if blocked.iter().any(|b| host == *b || host.ends_with(b)) {
            return Err(Error::Connection(
                "Basic auth is disabled on Exchange Online; use --auth-bearer".to_owned(),
            ));
        }
    }
    Ok(())
}

fn spawn_token_refresher(
    client: &EwsClient,
    auth: &EwsAuth,
    initial: &Option<AcquiredToken>,
    allow_invalid_certs: bool,
    logger: crate::logging::Logger,
) {
    let flow = match auth {
        EwsAuth::OAuth(flow) => flow.clone(),
        _ => return,
    };
    let exp = initial.as_ref().and_then(|t| {
        crate::exchange_ews::oauth::decode_jwt_claims(&t.access_token).and_then(|c| c.exp)
    });
    let Some(exp_secs) = exp else {
        return;
    };
    let client = client.clone();
    let initial_refresh = initial.as_ref().and_then(|t| t.refresh_token.clone());
    std::thread::Builder::new()
        .name("vandelay-ews-token-refresh".to_owned())
        .spawn(move || {
            let mut next_exp = exp_secs;
            let mut refresh_token = initial_refresh;
            loop {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let refresh_at = next_exp.saturating_sub(5 * 60);
                if refresh_at > now {
                    std::thread::sleep(std::time::Duration::from_secs(refresh_at - now));
                }
                let result = if let (Some(rt), OAuthFlow::DeviceCode { tenant, client_id }) =
                    (refresh_token.as_deref(), &flow)
                {
                    crate::exchange_ews::oauth::refresh_with_token(
                        tenant,
                        client_id,
                        rt,
                        allow_invalid_certs,
                    )
                } else {
                    crate::exchange_ews::oauth::acquire(&flow, allow_invalid_certs)
                };
                match result {
                    Ok(tok) => {
                        client.set_auth(crate::jmap::http::Auth::Bearer {
                            token: tok.access_token.clone(),
                        });
                        if tok.refresh_token.is_some() {
                            refresh_token = tok.refresh_token.clone();
                        }
                        if let Some(new_exp) =
                            crate::exchange_ews::oauth::decode_jwt_claims(&tok.access_token)
                                .and_then(|c| c.exp)
                        {
                            next_exp = new_exp;
                        } else {
                            next_exp = next_exp.saturating_add(50 * 60);
                        }
                    }
                    Err(e) => {
                        logger.warn(&format!(
                            "EWS token refresh failed: {e}; sleeping 60s before retry"
                        ));
                        std::thread::sleep(std::time::Duration::from_secs(60));
                    }
                }
            }
        })
        .ok();
}

fn run_gc(conn: &Connection) -> Result<(), Error> {
    let tx = conn.unchecked_transaction()?;
    db::blobs::gc_orphan_blobs(&tx)?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_account_id_uses_smtp_for_primary() {
        assert_eq!(
            synthetic_account_id("alice@contoso.com", MailboxKind::Primary),
            "alice@contoso.com"
        );
    }

    #[test]
    fn synthetic_account_id_archive_suffix() {
        assert_eq!(
            synthetic_account_id("alice@contoso.com", MailboxKind::Archive),
            "alice@contoso.com#archive"
        );
    }

    #[test]
    fn synthetic_account_id_public_folders_uses_tenant() {
        assert_eq!(
            synthetic_account_id("alice@contoso.com", MailboxKind::PublicFolders),
            "__public_folders__@contoso.com"
        );
    }

    #[test]
    fn basic_against_office365_endpoint_is_refused() {
        let auth = EwsAuth::Basic {
            user: "alice@x".to_owned(),
            password: "p".to_owned(),
        };
        let res =
            enforce_basic_auth_policy(&auth, "https://outlook.office365.com/EWS/Exchange.asmx");
        assert!(matches!(res, Err(Error::Connection(_))));
    }

    #[test]
    fn basic_over_https_on_prem_is_allowed() {
        let auth = EwsAuth::Basic {
            user: "alice@x".to_owned(),
            password: "p".to_owned(),
        };
        assert!(
            enforce_basic_auth_policy(&auth, "https://exchange.example.com/EWS/Exchange.asmx")
                .is_ok()
        );
    }

    #[test]
    fn basic_over_http_is_refused() {
        let auth = EwsAuth::Basic {
            user: "alice".to_owned(),
            password: "p".to_owned(),
        };
        assert!(
            enforce_basic_auth_policy(&auth, "http://exchange.example.com/EWS/Exchange.asmx")
                .is_err()
        );
    }

    #[test]
    fn normalise_ews_url_lowercases_host_and_keeps_canonical_path() {
        assert_eq!(
            normalise_ews_url("https://OUTLOOK.OFFICE365.COM/EWS/Exchange.asmx"),
            "https://outlook.office365.com/EWS/Exchange.asmx"
        );
        assert_eq!(
            normalise_ews_url("https://outlook.office365.com:443/ews/exchange.asmx"),
            "https://outlook.office365.com/EWS/Exchange.asmx"
        );
        assert_eq!(
            normalise_ews_url("https://srv.example.com:8443/EWS/Exchange.asmx"),
            "https://srv.example.com:8443/EWS/Exchange.asmx"
        );
    }
}
