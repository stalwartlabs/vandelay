/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::sync::Arc;

use url::Url;

use crate::db;
use crate::db::sources::SourceKey;
use crate::error::Error;
use crate::imap::retry::BackoffState;
use crate::imap::transport::Connector;
use crate::logging::{LEVEL_DEFAULT, LEVEL_PROGRESS, Logger};
use crate::managesieve::error::SieveError;
use crate::managesieve::response::parse_listscripts;
use crate::managesieve::retry::{Disposition, classify, is_negotiation_failure};
use crate::managesieve::{ConnectMode, SieveClient, parse_getscript};
use crate::sync::keys::blake3_bytes;
use crate::sync::{CommonConfig, Summary, TypeCounts};

use super::reconcile::{
    Action, PresentOutcome, apply_active_assignment, apply_content_update, apply_delete, apply_new,
    load_local_state, plan,
};

pub struct ManageSieveImportConfig {
    pub url: String,
    pub auth: ManageSieveAuth,
    pub allow_cleartext: bool,
    pub allow_source_change: bool,
}

#[derive(Debug, Clone)]
pub enum ManageSieveAuth {
    Basic { user: String, password: String },
    Bearer { user: String, token: String },
}

#[derive(Debug, Clone)]
struct Endpoint {
    host: String,
    port: u16,
    implicit_tls: bool,
}

struct ControlCtx {
    connector: Arc<Connector>,
    host: String,
    port: u16,
    mode: ConnectMode,
    allow_cleartext: bool,
    auth: ManageSieveAuth,
    backoff: BackoffState,
    max_retries: u32,
    logger: Logger,
}

fn reconnect(client: &mut SieveClient, ctx: &ControlCtx) -> Result<(), SieveError> {
    let new = SieveClient::connect(
        &ctx.connector,
        &ctx.host,
        ctx.port,
        ctx.mode,
        ctx.allow_cleartext,
        ctx.logger,
    )?;
    *client = new;
    match do_authenticate(client, &ctx.auth) {
        Ok(()) => {}
        Err(SieveAuthError::Wire(e)) => return Err(e),
        Err(SieveAuthError::TerminallyRefused(text)) => {
            return Err(SieveError::Protocol(format!(
                "auth refused on reconnect: {text}"
            )));
        }
        Err(SieveAuthError::NoUsableMechanism(text)) => {
            return Err(SieveError::Unsupported(text));
        }
    }
    if !client.had_fresh_post_auth_caps()
        && let Err(e) = client.refresh_capabilities()
    {
        ctx.logger.warn(&format!(
            "post-reconnect CAPABILITY refresh failed: {e}; continuing"
        ));
    }
    Ok(())
}

pub fn run(common: CommonConfig, config: ManageSieveImportConfig) -> Result<Summary, Error> {
    let logger = common.logger;
    if common.threads > 1 {
        log_at(
            logger,
            LEVEL_PROGRESS,
            "managesieve importer is single-threaded; --threads value will be ignored",
        );
    }

    let mut conn = db::init::open(&common.archive)?;

    let endpoint = parse_endpoint(&config.url)?;
    let session_url = format!(
        "{}://{}:{}",
        if endpoint.implicit_tls {
            "sieves"
        } else {
            "sieve"
        },
        endpoint.host,
        endpoint.port
    );

    let connector = Arc::new(
        Connector::new(common.allow_invalid_certs).map_err(|e| Error::Connection(e.to_string()))?,
    );
    let mode = if endpoint.implicit_tls {
        ConnectMode::ImplicitTls
    } else {
        ConnectMode::StartTls
    };

    let mut client = SieveClient::connect(
        &connector,
        &endpoint.host,
        endpoint.port,
        mode,
        config.allow_cleartext,
        logger,
    )
    .map_err(|e| Error::Connection(e.to_string()))?;

    let account_id = authenticate(&mut client, &config.auth)?;
    log_at(
        logger,
        LEVEL_PROGRESS,
        &format!("authenticated as {account_id} on {session_url}"),
    );
    if !client.had_fresh_post_auth_caps()
        && let Err(e) = client.refresh_capabilities()
    {
        logger.warn(&format!(
            "post-auth CAPABILITY refresh failed: {e}; continuing"
        ));
    }

    let source_key = SourceKey {
        kind: "managesieve".to_owned(),
        session_url: session_url.clone(),
        account_id: account_id.clone(),
    };
    if let Some((existing_url, existing_account)) =
        db::sources::conflicting_source(&conn, "managesieve", &session_url, &account_id)?
        && !config.allow_source_change
    {
        return Err(Error::SourceChange(format!(
            "archive already records managesieve source {existing_url} / {existing_account}; \
             re-run with --allow-source-change or use a fresh archive"
        )));
    }
    let source_id = if common.dry_run {
        db::sources::find_source(&conn, &source_key)?.unwrap_or(-1)
    } else {
        db::sources::upsert_source(&conn, &source_key, Some(&account_id), &account_id)?
    };

    let backoff = BackoffState::new();
    let control_ctx = ControlCtx {
        connector: connector.clone(),
        host: endpoint.host.clone(),
        port: endpoint.port,
        mode,
        allow_cleartext: config.allow_cleartext,
        auth: config.auth.clone(),
        backoff: backoff.clone(),
        max_retries: common.max_retries,
        logger,
    };
    let server_listing = listscripts_with_retry(&mut client, &control_ctx)?;

    let mut server_with_bytes: Vec<(String, bool, [u8; 32], Vec<u8>)> =
        Vec::with_capacity(server_listing.len());
    let mut counts = TypeCounts::default();
    for (name, active) in &server_listing {
        match getscript_with_retry(&mut client, &control_ctx, name) {
            Ok(bytes) => {
                let hash = blake3_bytes(&bytes);
                server_with_bytes.push((name.clone(), *active, hash, bytes));
            }
            Err(e) => match classify(&e) {
                Disposition::Permanent
                | Disposition::PerScriptRecoverable
                | Disposition::Transient => {
                    logger.warn(&format!("GETSCRIPT {name:?} failed, skipping: {e}"));
                    counts.failed += 1;
                }
                Disposition::Referral => {
                    return Err(Error::Connection(format!(
                        "ManageSieve referral on GETSCRIPT {name:?}: {e}; vandelay does not follow referrals"
                    )));
                }
                Disposition::TransportDrop => {
                    return Err(Error::Connection(format!(
                        "GETSCRIPT {name:?} transport drop exhausted retries: {e}"
                    )));
                }
                Disposition::FeatureNegotiation => {
                    return Err(Error::Connection(format!("GETSCRIPT {name:?}: {e}")));
                }
            },
        }
    }

    if common.dry_run {
        let summary =
            build_dry_run_summary(&conn, source_id, &server_with_bytes, counts, &backoff)?;
        let _ = client.logout();
        return Ok(summary);
    }

    let local_map = db::managesieve_ids::all_names(&conn, source_id)?;
    let (local_hashes, local_active) = load_local_state(&conn, source_id)?;
    let server_keyed: Vec<(String, bool, [u8; 32])> = server_with_bytes
        .iter()
        .map(|(n, a, h, _)| (n.clone(), *a, *h))
        .collect();
    let plan = plan(&server_keyed, &local_map, &local_hashes, &local_active);

    let bytes_by_name: std::collections::HashMap<String, &Vec<u8>> = server_with_bytes
        .iter()
        .map(|(n, _, _, b)| (n.clone(), b))
        .collect();

    let mut active_local: Option<i64> = None;

    for action in &plan.actions {
        match action {
            Action::New { name, active } => {
                let bytes = bytes_by_name.get(name).cloned().unwrap();
                let tx = conn.transaction()?;
                match apply_new(&tx, source_id, name, bytes) {
                    Ok(local_id) => {
                        tx.commit()?;
                        if *active {
                            active_local = Some(local_id);
                        }
                        counts.created += 1;
                        counts.fetched += 1;
                    }
                    Err(e) => {
                        let _ = tx.rollback();
                        logger.warn(&format!("INSERT sieve_scripts {name:?} failed: {e}"));
                        counts.failed += 1;
                    }
                }
            }
            Action::Present {
                name,
                local_id,
                outcome,
                active,
            } => {
                match outcome {
                    PresentOutcome::Unchanged => {
                        counts.skipped += 1;
                    }
                    PresentOutcome::ActiveOnly => {
                        counts.skipped += 1;
                    }
                    PresentOutcome::ContentUpdated => {
                        let bytes = bytes_by_name.get(name).cloned().unwrap();
                        let tx = conn.transaction()?;
                        match apply_content_update(&tx, *local_id, bytes) {
                            Ok(()) => {
                                tx.commit()?;
                                counts.fetched += 1;
                            }
                            Err(e) => {
                                let _ = tx.rollback();
                                logger.warn(&format!("UPDATE sieve_scripts {name:?} failed: {e}"));
                                counts.failed += 1;
                            }
                        }
                    }
                }
                if *active {
                    active_local = Some(*local_id);
                }
            }
            Action::Vanished { name, local_id } => {
                let tx = conn.transaction()?;
                match apply_delete(&tx, source_id, name, *local_id) {
                    Ok(()) => {
                        tx.commit()?;
                        counts.deleted += 1;
                    }
                    Err(e) => {
                        let _ = tx.rollback();
                        logger.warn(&format!("DELETE sieve_scripts {name:?} failed: {e}"));
                        counts.failed += 1;
                    }
                }
            }
        }
    }

    apply_active_assignment(&mut conn, active_local)?;

    let _ = client.logout();

    if counts.failed == 0 {
        let tx = conn.unchecked_transaction()?;
        db::blobs::gc_orphan_blobs(&tx)?;
        tx.commit()?;
    }

    log_at(
        logger,
        LEVEL_DEFAULT,
        &format!(
            "managesieve: created={} fetched={} deleted={} skipped={} failed={}",
            counts.created, counts.fetched, counts.deleted, counts.skipped, counts.failed
        ),
    );

    Ok(Summary {
        per_type: vec![("sievescript", counts)],
        retries_observed: backoff.total_retries(),
        retry_after_sleeps: backoff.transient_retries() as u64,
    })
}

fn build_dry_run_summary(
    conn: &rusqlite::Connection,
    source_id: i64,
    server: &[(String, bool, [u8; 32], Vec<u8>)],
    mut counts: TypeCounts,
    backoff: &BackoffState,
) -> Result<Summary, Error> {
    let server_keyed: Vec<(String, bool, [u8; 32])> = server
        .iter()
        .map(|(n, a, h, _)| (n.clone(), *a, *h))
        .collect();
    let (local_map, local_hashes, local_active) = if source_id < 0 {
        (
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
        )
    } else {
        let local_map = db::managesieve_ids::all_names(conn, source_id)?;
        let (h, a) = load_local_state(conn, source_id)?;
        (local_map, h, a)
    };
    let plan = plan(&server_keyed, &local_map, &local_hashes, &local_active);
    for action in &plan.actions {
        match action {
            Action::New { .. } => {
                counts.created += 1;
                counts.fetched += 1;
            }
            Action::Present {
                outcome: PresentOutcome::ContentUpdated,
                ..
            } => counts.fetched += 1,
            Action::Present { .. } => counts.skipped += 1,
            Action::Vanished { .. } => counts.deleted += 1,
        }
    }
    Ok(Summary {
        per_type: vec![("sievescript", counts)],
        retries_observed: backoff.total_retries(),
        retry_after_sleeps: backoff.transient_retries() as u64,
    })
}

fn parse_endpoint(url: &str) -> Result<Endpoint, Error> {
    let parsed =
        Url::parse(url).map_err(|e| Error::Usage(format!("invalid --url {url:?}: {e}")))?;
    let scheme = parsed.scheme();
    let implicit_tls = match scheme {
        "sieves" => true,
        "sieve" => false,
        other => {
            return Err(Error::Usage(format!(
                "--url scheme must be sieve or sieves, got {other}"
            )));
        }
    };
    let host = parsed
        .host_str()
        .ok_or_else(|| Error::Usage(format!("--url missing host: {url}")))?
        .to_owned();
    let port = parsed.port().unwrap_or(4190);
    Ok(Endpoint {
        host,
        port,
        implicit_tls,
    })
}

fn authenticate(client: &mut SieveClient, auth: &ManageSieveAuth) -> Result<String, Error> {
    do_authenticate(client, auth).map_err(|e| match e {
        SieveAuthError::TerminallyRefused(text) => {
            Error::Connection(format!("auth failed: {text}"))
        }
        SieveAuthError::NoUsableMechanism(text) => Error::Connection(text),
        SieveAuthError::Wire(e) => Error::Connection(e.to_string()),
    })?;
    Ok(account_id_for(auth))
}

fn account_id_for(auth: &ManageSieveAuth) -> String {
    match auth {
        ManageSieveAuth::Basic { user, .. } => user.clone(),
        ManageSieveAuth::Bearer { user, .. } => user.clone(),
    }
}

#[derive(Debug)]
enum SieveAuthError {
    TerminallyRefused(String),

    NoUsableMechanism(String),

    Wire(SieveError),
}

fn do_authenticate(client: &mut SieveClient, auth: &ManageSieveAuth) -> Result<(), SieveAuthError> {
    match auth {
        ManageSieveAuth::Basic { user, password } => {
            let caps = client.capabilities().clone();
            let mut last_plain_error: Option<String> = None;
            if caps.has_sasl("PLAIN") {
                match client.authenticate_plain(user, password) {
                    Ok(()) => return Ok(()),
                    Err(e) if is_negotiation_failure(&e) => {
                        last_plain_error = Some(e.to_string());
                    }
                    Err(e) => return Err(SieveAuthError::Wire(e)),
                }
            }
            if caps.has_sasl("LOGIN") {
                match client.authenticate_login(user, password) {
                    Ok(()) => return Ok(()),
                    Err(e) if is_negotiation_failure(&e) => {
                        let combined = match last_plain_error {
                            Some(p) => format!("LOGIN: {e}; PLAIN: {p}"),
                            None => format!("LOGIN: {e}"),
                        };
                        return Err(SieveAuthError::TerminallyRefused(combined));
                    }
                    Err(e) => return Err(SieveAuthError::Wire(e)),
                }
            }
            if let Some(p) = last_plain_error {
                return Err(SieveAuthError::TerminallyRefused(format!("PLAIN: {p}")));
            }
            Err(SieveAuthError::NoUsableMechanism(format!(
                "server SASL list lacks PLAIN and LOGIN (advertised: {:?})",
                caps.sasl
            )))
        }
        ManageSieveAuth::Bearer { user, token } => {
            if !client.capabilities().has_sasl("OAUTHBEARER") {
                return Err(SieveAuthError::NoUsableMechanism(format!(
                    "server does not advertise OAUTHBEARER (advertised: {:?})",
                    client.capabilities().sasl
                )));
            }
            match client.authenticate_oauthbearer(user, token) {
                Ok(()) => Ok(()),
                Err(e) if is_negotiation_failure(&e) => Err(SieveAuthError::TerminallyRefused(
                    format!("OAUTHBEARER: {e}"),
                )),
                Err(e) => Err(SieveAuthError::Wire(e)),
            }
        }
    }
}

fn listscripts_with_retry(
    client: &mut SieveClient,
    ctx: &ControlCtx,
) -> Result<Vec<(String, bool)>, Error> {
    let mut transient_attempts: u32 = 0;
    let mut transport_attempts: u32 = 0;
    loop {
        match client.listscripts() {
            Ok(block) => {
                ctx.backoff.reset();
                let listed = parse_listscripts(&block.data)
                    .map_err(|e| Error::Connection(format!("LISTSCRIPTS parse: {e}")))?;
                return Ok(listed.into_iter().map(|s| (s.name, s.active)).collect());
            }
            Err(e) => match classify(&e) {
                Disposition::Transient => {
                    if transient_attempts >= ctx.max_retries {
                        return Err(Error::Connection(format!(
                            "LISTSCRIPTS retries exhausted: {e}"
                        )));
                    }
                    transient_attempts += 1;
                    log_at(
                        ctx.logger,
                        LEVEL_PROGRESS,
                        &format!("LISTSCRIPTS transient: {e}; retrying"),
                    );
                    std::thread::sleep(ctx.backoff.next_shared_delay());
                }
                Disposition::TransportDrop => {
                    if transport_attempts >= ctx.max_retries {
                        return Err(Error::Connection(format!(
                            "LISTSCRIPTS transport drop, retries exhausted: {e}"
                        )));
                    }
                    transport_attempts += 1;
                    std::thread::sleep(ctx.backoff.transport_delay(transport_attempts));
                    match reconnect(client, ctx) {
                        Ok(()) => {
                            log_at(
                                ctx.logger,
                                LEVEL_DEFAULT,
                                &format!("managesieve reconnected after transport drop ({e})"),
                            );
                        }
                        Err(e2) => {
                            return Err(Error::Connection(format!(
                                "LISTSCRIPTS reconnect failed: {e2}"
                            )));
                        }
                    }
                }
                Disposition::Referral => {
                    return Err(Error::Connection(format!(
                        "LISTSCRIPTS referral: {e}; vandelay does not follow referrals"
                    )));
                }
                _ => return Err(Error::Connection(format!("LISTSCRIPTS failed: {e}"))),
            },
        }
    }
}

fn getscript_with_retry(
    client: &mut SieveClient,
    ctx: &ControlCtx,
    name: &str,
) -> Result<Vec<u8>, SieveError> {
    let mut transient_attempts: u32 = 0;
    let mut transport_attempts: u32 = 0;
    loop {
        match client.getscript(name) {
            Ok(block) => {
                ctx.backoff.reset();
                return parse_getscript(&block.data);
            }
            Err(e) => match classify(&e) {
                Disposition::Transient => {
                    if transient_attempts >= ctx.max_retries {
                        return Err(e);
                    }
                    transient_attempts += 1;
                    log_at(
                        ctx.logger,
                        LEVEL_PROGRESS,
                        &format!("GETSCRIPT {name:?} transient: {e}; retrying"),
                    );
                    std::thread::sleep(ctx.backoff.next_shared_delay());
                }
                Disposition::TransportDrop => {
                    if transport_attempts >= ctx.max_retries {
                        return Err(e);
                    }
                    transport_attempts += 1;
                    std::thread::sleep(ctx.backoff.transport_delay(transport_attempts));
                    reconnect(client, ctx)?;
                    log_at(
                        ctx.logger,
                        LEVEL_DEFAULT,
                        &format!(
                            "managesieve reconnected mid-GETSCRIPT {name:?} after transport drop"
                        ),
                    );
                }
                _ => return Err(e),
            },
        }
    }
}

fn log_at(logger: Logger, level: u8, msg: &str) {
    if logger.enabled(level) {
        eprintln!("{msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_endpoint_sieves_defaults_to_4190() {
        let e = parse_endpoint("sieves://mail.example.com").unwrap();
        assert_eq!(e.host, "mail.example.com");
        assert_eq!(e.port, 4190);
        assert!(e.implicit_tls);
    }

    #[test]
    fn parse_endpoint_sieve_defaults_to_4190() {
        let e = parse_endpoint("sieve://mail.example.com").unwrap();
        assert_eq!(e.host, "mail.example.com");
        assert_eq!(e.port, 4190);
        assert!(!e.implicit_tls);
    }

    #[test]
    fn parse_endpoint_explicit_port() {
        let e = parse_endpoint("sieve://mail.example.com:14190").unwrap();
        assert_eq!(e.port, 14190);
        assert!(!e.implicit_tls);
    }

    #[test]
    fn parse_endpoint_rejects_other_schemes() {
        let err = parse_endpoint("imap://example.com").unwrap_err();
        assert!(matches!(err, Error::Usage(_)));
    }

    #[test]
    fn parse_endpoint_rejects_missing_host() {
        let err = parse_endpoint("sieve:///path").unwrap_err();
        assert!(matches!(err, Error::Usage(_)));
    }
}
