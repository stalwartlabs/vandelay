/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};

use crossbeam_channel::RecvTimeoutError;
use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use url::Url;

use crate::db;
use crate::db::sources::SourceKey;
use crate::error::Error;
use crate::imap::client::{ConnectMode, ImapClient};
use crate::imap::command;
use crate::imap::error::ImapError;
use crate::imap::name::{alternate_mailbox_name, encode_mailbox_name_with};
use crate::imap::response::{NamespaceEntry, Untagged};
use crate::imap::retry::{
    BackoffState, Disposition, RetryPolicy, classify, is_negotiation_failure,
};
use crate::imap::transport::Connector;
use crate::logging::{LEVEL_DEFAULT, LEVEL_PROGRESS, Logger};
use crate::sync::emailmeta::email_index_from_blob;
use crate::sync::keys::index_to_json;
use crate::sync::{CommonConfig, RunOutcome, Summary, TypeCounts};

use super::fetch;
use super::folders::{
    FolderFilters, ResolvedFolder, apply_filters, collect_from_list, sort_by_depth,
    vanished_depth_sort, vanished_folders,
};
use super::internaldate::imap_internaldate_to_rfc3339;
use super::keywords::translate_flags;
use super::messages::{chunks, diff_uids, select_uids};
use super::pool::{FetchEvent, FetchJob, WorkerArgs, WorkerPool};

const EMAIL_TYPE: &str = "email";

#[derive(Clone, Copy)]
pub(super) struct RunOpts {
    source_id: i64,
    fetch_batch: usize,
    include_deleted: bool,
    logger: Logger,
}

#[derive(Clone)]
pub(super) struct ControlCtx {
    pub connector: std::sync::Arc<Connector>,
    pub endpoint: std::sync::Arc<Endpoint>,
    pub mode: ConnectMode,
    pub auth: ImapAuth,
    pub compress: bool,
    pub policy: RetryPolicy,
    pub backoff: BackoffState,
    pub logger: Logger,
}

pub(super) fn control_run_collect(
    client: &mut ImapClient,
    ctx: &ControlCtx,
    command: &str,
) -> Result<crate::imap::CollectedResponse, Error> {
    let mut transient_attempts: u32 = 0;
    let mut transport_attempts: u32 = 0;
    loop {
        match client.run_collect(command) {
            Ok(r) => {
                ctx.backoff.reset();
                return Ok(r);
            }
            Err(e) => match classify(&e) {
                Disposition::TransportDrop => {
                    if transport_attempts >= ctx.policy.max_retries {
                        return Err(Error::Connection(format!("control connection lost: {e}")));
                    }
                    transport_attempts += 1;
                    std::thread::sleep(ctx.backoff.transport_delay(transport_attempts));
                    match reconnect_control(client, ctx) {
                        Ok(()) => {
                            log_at(ctx.logger, LEVEL_DEFAULT, "control connection reconnected");
                        }
                        Err(e2) => {
                            return Err(Error::Connection(format!(
                                "control reconnect failed: {e2}"
                            )));
                        }
                    }
                }
                Disposition::Transient => {
                    if transient_attempts >= ctx.policy.max_retries {
                        return Err(Error::Partial(format!(
                            "{command:?}: {e} (max retries exhausted)"
                        )));
                    }
                    transient_attempts += 1;
                    std::thread::sleep(ctx.backoff.next_shared_delay());
                }
                _ => {
                    return Err(Error::Partial(format!("{command:?}: {e}")));
                }
            },
        }
    }
}

pub(super) fn select_folder(
    client: &mut ImapClient,
    ctx: &ControlCtx,
    folder: &str,
) -> Result<crate::imap::CollectedResponse, Error> {
    let utf8_accept = client.utf8_accept();
    let wire_name = encode_mailbox_name_with(folder, utf8_accept);
    let first = control_run_collect(client, ctx, &command::select(&wire_name));
    let Err(Error::Partial(_)) = &first else {
        return first;
    };
    let Some(alternate) = alternate_mailbox_name(folder, utf8_accept) else {
        return first;
    };
    match control_run_collect(client, ctx, &command::select(&alternate)) {
        Ok(r) => {
            log_at(
                ctx.logger,
                LEVEL_DEFAULT,
                &format!("folder {folder:?}: selected with the alternate mailbox-name encoding"),
            );
            Ok(r)
        }
        Err(_) => first,
    }
}

pub(super) fn call_with_retry<F, T>(
    client: &mut ImapClient,
    ctx: &ControlCtx,
    mut attempt: F,
) -> Result<T, ImapError>
where
    F: FnMut(&mut ImapClient) -> Result<T, ImapError>,
{
    let mut transient_attempts: u32 = 0;
    let mut transport_attempts: u32 = 0;
    loop {
        match attempt(client) {
            Ok(v) => {
                ctx.backoff.reset();
                return Ok(v);
            }
            Err(e) => match classify(&e) {
                Disposition::TransportDrop => {
                    if transport_attempts >= ctx.policy.max_retries {
                        return Err(e);
                    }
                    transport_attempts += 1;
                    std::thread::sleep(ctx.backoff.transport_delay(transport_attempts));
                    reconnect_control(client, ctx)?;
                }
                Disposition::Transient => {
                    if transient_attempts >= ctx.policy.max_retries {
                        return Err(e);
                    }
                    transient_attempts += 1;
                    std::thread::sleep(ctx.backoff.next_shared_delay());
                }
                _ => return Err(e),
            },
        }
    }
}

fn reconnect_control(client: &mut ImapClient, ctx: &ControlCtx) -> Result<(), ImapError> {
    let new_client = ImapClient::connect(
        &ctx.connector,
        &ctx.endpoint.host,
        ctx.endpoint.port,
        ctx.mode,
        ctx.logger,
    )?;
    *client = new_client;
    authenticate_client(client, &ctx.auth).map_err(|e| ImapError::AuthFailed(e.to_string()))?;
    let _ = client.refresh_capabilities();
    if ctx.compress && client.has_capability("COMPRESS=DEFLATE") {
        client.compress_deflate()?;
    }
    if client.has_capability("ENABLE") && client.has_capability("UTF8=ACCEPT") {
        let _ = client.enable(&["UTF8=ACCEPT"]);
    }
    Ok(())
}

fn log_at(logger: Logger, level: u8, msg: &str) {
    if logger.enabled(level) {
        println!("{msg}");
    }
}

pub struct ImapImportConfig {
    pub url: String,
    pub auth: ImapAuth,
    pub allow_cleartext: bool,
    pub compress: bool,
    pub include: Vec<Regex>,
    pub exclude: Vec<Regex>,
    pub exclude_special: Vec<String>,
    pub folder: Vec<String>,
    pub subscribed_only: bool,
    pub automap: bool,
    pub include_deleted: bool,
    pub fetch_batch: usize,
    pub imap_connections: usize,
    pub allow_source_change: bool,
}

#[derive(Debug, Clone)]
pub enum ImapAuth {
    Basic { user: String, password: String },
    Bearer { user: String, token: String },
}

pub fn run(common: CommonConfig, config: ImapImportConfig) -> Result<Summary, Error> {
    run_reporting(common, config).into_result()
}

pub fn run_reporting(common: CommonConfig, config: ImapImportConfig) -> RunOutcome {
    let mut summary = Summary::default();
    let error = run_into(common, config, &mut summary).err();
    RunOutcome { summary, error }
}

fn run_into(
    common: CommonConfig,
    config: ImapImportConfig,
    summary: &mut Summary,
) -> Result<(), Error> {
    let logger = common.logger;
    let mut conn = db::init::open(&common.archive)?;

    let endpoint = parse_endpoint(&config.url)?;
    let session_url = format!(
        "{}://{}:{}",
        if endpoint.implicit_tls {
            "imaps"
        } else {
            "imap"
        },
        endpoint.host,
        endpoint.port
    );

    let connector = std::sync::Arc::new(
        Connector::new(common.allow_invalid_certs).map_err(|e| Error::Connection(e.to_string()))?,
    );
    let endpoint = std::sync::Arc::new(endpoint);
    let mode = if endpoint.implicit_tls {
        ConnectMode::ImplicitTls
    } else if config.allow_cleartext {
        ConnectMode::Plain
    } else {
        ConnectMode::StartTls
    };
    let mut client = ImapClient::connect(&connector, &endpoint.host, endpoint.port, mode, logger)
        .map_err(|e| Error::Connection(e.to_string()))?;

    let account_id = authenticate(&mut client, &config.auth)?;
    log_at(
        logger,
        LEVEL_PROGRESS,
        &format!("authenticated as {account_id} on {session_url}"),
    );
    client
        .refresh_capabilities()
        .map_err(|e| Error::Connection(format!("post-auth CAPABILITY: {e}")))?;

    if config.compress {
        if client.has_capability("COMPRESS=DEFLATE") {
            client
                .compress_deflate()
                .map_err(|e| Error::Connection(format!("COMPRESS DEFLATE: {e}")))?;
            log_at(logger, LEVEL_PROGRESS, "COMPRESS DEFLATE enabled");
        } else {
            log_at(
                logger,
                LEVEL_PROGRESS,
                "--compress requested but server does not advertise COMPRESS=DEFLATE; ignored",
            );
        }
    }

    if client.has_capability("ENABLE") && client.has_capability("UTF8=ACCEPT") {
        match client.enable(&["UTF8=ACCEPT"]) {
            Ok(_) => log_at(logger, LEVEL_PROGRESS, "ENABLE UTF8=ACCEPT"),
            Err(e) => log_at(
                logger,
                LEVEL_PROGRESS,
                &format!("ENABLE UTF8=ACCEPT failed, falling back to modified UTF-7: {e}"),
            ),
        }
    }

    let source_key = SourceKey {
        kind: "imap".to_owned(),
        session_url: session_url.clone(),
        account_id: account_id.clone(),
    };
    if let Some((existing_url, existing_account)) =
        db::sources::conflicting_source(&conn, "imap", &session_url, &account_id)?
        && !config.allow_source_change
    {
        return Err(Error::SourceChange(format!(
            "archive already records imap source {existing_url} / {existing_account}; \
             re-run with --allow-source-change or use a fresh archive"
        )));
    }
    let source_id = if common.dry_run {
        -1
    } else {
        db::sources::upsert_source(&conn, &source_key, Some(&account_id), &account_id)?
    };

    let backoff = BackoffState::new();
    let policy = RetryPolicy::new(common.max_retries);
    let control_ctx = ControlCtx {
        connector: connector.clone(),
        endpoint: endpoint.clone(),
        mode,
        auth: config.auth.clone(),
        compress: config.compress,
        policy,
        backoff: backoff.clone(),
        logger,
    };

    let namespace_prefix = discover_namespace(&mut client, logger)?;

    let extended_supported = client.has_capability("LIST-EXTENDED");
    let special_use_supported = client.has_capability("SPECIAL-USE");
    let list_status_supported = extended_supported && client.has_capability("LIST-STATUS");
    let list_cmd = if extended_supported {
        let mut return_opts: Vec<&str> = Vec::new();
        if special_use_supported {
            return_opts.push("SPECIAL-USE");
        }
        return_opts.push("SUBSCRIBED");
        if list_status_supported {
            return_opts.push("CHILDREN");
            return_opts.push("STATUS (UIDVALIDITY UIDNEXT MESSAGES)");
        }
        command::list_extended("", &["*"], &[], &return_opts)
    } else {
        command::list("", "*")
    };
    let list_resp = control_run_collect(&mut client, &control_ctx, &list_cmd)?;
    let mut all_responses = list_resp.untagged;
    if (!extended_supported || !list_status_supported)
        && let Ok(lsub_resp) =
            control_run_collect(&mut client, &control_ctx, &command::lsub("", "*"))
    {
        all_responses.extend(lsub_resp.untagged);
    }

    let discovered = collect_from_list(&all_responses, client.utf8_accept())
        .map_err(|e| Error::Partial(format!("LIST parse: {e}")))?;
    let filters = FolderFilters {
        include: config.include,
        exclude: config.exclude,
        exclude_special: config.exclude_special,
        explicit: config.folder,
        subscribed_only: config.subscribed_only,
        automap_enabled: config.automap,
        namespace_prefix,
    };
    let mut resolved = apply_filters(discovered, &filters);
    sort_by_depth(&mut resolved);

    if common.dry_run {
        let existing_source = db::sources::find_source(&conn, &source_key)?;
        *summary = dry_run_summary(
            &conn,
            existing_source,
            &mut client,
            &control_ctx,
            &resolved,
            logger,
        )?;
        return Ok(());
    }

    let mut mailbox_counts = TypeCounts::default();
    let mut email_counts = TypeCounts::default();

    let local_mailboxes = db::imap_ids::mailbox_folders(&conn, source_id)?;
    let server_folder_set: HashSet<String> = resolved.iter().map(|f| f.name.clone()).collect();

    upsert_mailboxes(&mut conn, source_id, &resolved, &mut mailbox_counts)?;

    let server_delimiter = resolved.iter().find_map(|f| f.delimiter).unwrap_or('/');

    let mut vanished = vanished_folders(&local_mailboxes, &server_folder_set);
    vanished_depth_sort(&mut vanished, server_delimiter);
    if !vanished.is_empty() {
        delete_vanished_folders(
            &mut conn,
            source_id,
            &vanished,
            &mut mailbox_counts,
            &mut email_counts,
            logger,
        )?;
    }

    let opts = RunOpts {
        source_id,
        fetch_batch: config.fetch_batch.max(1),
        include_deleted: config.include_deleted,
        logger,
    };

    let pool = WorkerPool::start(
        WorkerArgs {
            connector: connector.clone(),
            endpoint: endpoint.clone(),
            mode,
            auth: config.auth.clone(),
            compress: config.compress,
            policy,
            backoff: backoff.clone(),
            logger,
        },
        config.imap_connections.max(1),
    )
    .map_err(|e| Error::Connection(format!("worker pool: {e}")))?;

    for (i, folder) in resolved.iter().enumerate() {
        if i > 0 {
            let _ = control_run_collect(&mut client, &control_ctx, "NOOP");
        }
        match reconcile_folder(
            &mut conn,
            &mut client,
            &control_ctx,
            &pool,
            folder,
            opts,
            &mut email_counts,
        ) {
            Ok(()) => {}
            Err(e) if e.aborts_run() => {
                pool.shutdown();
                let _ = client.logout();
                *summary = Summary {
                    per_type: vec![("mailbox", mailbox_counts), ("email", email_counts)],
                    retries_observed: backoff.total_retries(),
                    retry_after_sleeps: backoff.transient_retries() as u64,
                };
                return Err(e);
            }
            Err(e) => {
                log_at(
                    logger,
                    LEVEL_DEFAULT,
                    &format!("folder {:?}: {e}", folder.name),
                );
                email_counts.failed += 1;
            }
        }
    }

    pool.shutdown();
    let _ = client.logout();

    *summary = Summary {
        per_type: vec![("mailbox", mailbox_counts), ("email", email_counts)],
        retries_observed: backoff.total_retries(),
        retry_after_sleeps: backoff.transient_retries() as u64,
    };
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub implicit_tls: bool,
}

fn parse_endpoint(url: &str) -> Result<Endpoint, Error> {
    let parsed =
        Url::parse(url).map_err(|e| Error::Usage(format!("invalid --url {url:?}: {e}")))?;
    let scheme = parsed.scheme();
    let implicit_tls = match scheme {
        "imaps" => true,
        "imap" => false,
        other => {
            return Err(Error::Usage(format!(
                "--url scheme must be imap or imaps, got {other}"
            )));
        }
    };
    let host = parsed
        .host_str()
        .ok_or_else(|| Error::Usage(format!("--url missing host: {url}")))?
        .to_owned();
    let port = parsed
        .port()
        .unwrap_or(if implicit_tls { 993 } else { 143 });
    Ok(Endpoint {
        host,
        port,
        implicit_tls,
    })
}

pub(super) fn authenticate_client(client: &mut ImapClient, auth: &ImapAuth) -> Result<(), Error> {
    authenticate(client, auth).map(|_| ())
}

fn authenticate(client: &mut ImapClient, auth: &ImapAuth) -> Result<String, Error> {
    match auth {
        ImapAuth::Basic { user, password } => {
            if client.has_capability("AUTH=PLAIN") {
                match client.authenticate_plain(user, password) {
                    Ok(()) => return Ok(user.clone()),
                    Err(e) if is_auth_failed(&e) => {
                        return Err(Error::Connection(format!("auth failed: {e}")));
                    }
                    Err(e) if is_negotiation_failure(&e) => {}
                    Err(e) => return Err(Error::Connection(e.to_string())),
                }
            }
            match client.login(user, password) {
                Ok(()) => Ok(user.clone()),
                Err(e) if is_auth_failed(&e) => Err(Error::Connection(format!("auth failed: {e}"))),
                Err(e) => Err(Error::Connection(format!("LOGIN failed: {e}"))),
            }
        }
        ImapAuth::Bearer { user, token } => {
            let has_xoauth2 = client.has_capability("AUTH=XOAUTH2");
            let has_oauthbearer = client.has_capability("AUTH=OAUTHBEARER");
            if !has_xoauth2 && !has_oauthbearer {
                return Err(Error::Connection(
                    "server does not advertise AUTH=XOAUTH2 or AUTH=OAUTHBEARER".into(),
                ));
            }
            if has_xoauth2 {
                match client.authenticate_xoauth2(user, token) {
                    Ok(()) => return Ok(user.clone()),
                    Err(e) if is_auth_failed(&e) => {
                        return Err(Error::Connection(format!("auth failed: {e}")));
                    }
                    Err(e) if is_negotiation_failure(&e) && has_oauthbearer => {}
                    Err(e) => return Err(Error::Connection(format!("XOAUTH2 failed: {e}"))),
                }
            }
            client
                .authenticate_oauthbearer(user, token)
                .map_err(|e| Error::Connection(format!("OAUTHBEARER failed: {e}")))?;
            Ok(user.clone())
        }
    }
}

fn is_auth_failed(e: &ImapError) -> bool {
    match e {
        ImapError::AuthFailed(_) => true,
        ImapError::No(no) => no.is_auth_failed(),
        _ => false,
    }
}

fn discover_namespace(client: &mut ImapClient, logger: Logger) -> Result<String, Error> {
    if !client.has_capability("NAMESPACE") {
        return Ok(String::new());
    }
    let resp = match client.run_collect(command::namespace()) {
        Ok(r) => r,
        Err(e) => {
            log_at(
                logger,
                LEVEL_PROGRESS,
                &format!("NAMESPACE failed, ignoring: {e}"),
            );
            return Ok(String::new());
        }
    };
    for u in resp.untagged {
        if let Untagged::Namespace { personal, .. } = u
            && let Some(NamespaceEntry { prefix, .. }) = personal.into_iter().next()
        {
            return Ok(prefix);
        }
    }
    Ok(String::new())
}

fn upsert_mailboxes(
    conn: &mut Connection,
    source_id: i64,
    folders: &[ResolvedFolder],
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let tx = conn.transaction()?;
    let mut local_ids: HashMap<String, i64> = HashMap::new();
    for folder in folders {
        let parent_local = match &folder.parent_path {
            Some(p) => local_ids.get(p.as_str()).copied().or_else(|| {
                db::imap_ids::local_for_mailbox(&tx, source_id, p)
                    .ok()
                    .flatten()
            }),
            None => None,
        };
        let existing = db::imap_ids::local_for_mailbox(&tx, source_id, &folder.name)?;
        let id = if let Some(id) = existing {
            let role = db::roles::unique_role(&tx, folder.role, Some(id))?;
            tx.execute(
                "UPDATE mailboxes SET name = ?1, parent_id = ?2, role = ?3,
                 is_subscribed = ?4 WHERE id = ?5",
                params![
                    folder.leaf,
                    parent_local,
                    role,
                    folder.subscribed as i64,
                    id
                ],
            )?;
            id
        } else {
            let role = db::roles::unique_role(&tx, folder.role, None)?;
            tx.execute(
                "INSERT INTO mailboxes (name, parent_id, role, sort_order, is_subscribed)
                 VALUES (?1, ?2, ?3, 0, ?4)",
                params![folder.leaf, parent_local, role, folder.subscribed as i64],
            )?;
            let new_id = tx.last_insert_rowid();
            db::imap_ids::insert_mailbox(&tx, source_id, &folder.name, new_id)?;
            counts.created += 1;
            counts.fetched += 1;
            new_id
        };
        local_ids.insert(folder.name.clone(), id);
    }
    tx.commit()?;
    Ok(())
}

fn delete_vanished_folders(
    conn: &mut Connection,
    source_id: i64,
    folders: &[String],
    mailbox_counts: &mut TypeCounts,
    email_counts: &mut TypeCounts,
    logger: Logger,
) -> Result<(), Error> {
    let tx = conn.transaction()?;
    for name in folders {
        let local_id = match db::imap_ids::local_for_mailbox(&tx, source_id, name)? {
            Some(id) => id,
            None => continue,
        };
        let surviving_child: Option<i64> = tx
            .query_row(
                "SELECT id FROM mailboxes WHERE parent_id = ?1 LIMIT 1",
                params![local_id],
                |row| row.get(0),
            )
            .optional()?;
        if surviving_child.is_some() {
            log_at(
                logger,
                LEVEL_DEFAULT,
                &format!(
                    "folder {name:?}: vanished from server but still has child mailboxes in the archive; skipping delete (parent_id RESTRICT)"
                ),
            );
            mailbox_counts.failed += 1;
            continue;
        }
        let email_ids: Vec<i64> = tx
            .prepare(
                "SELECT local_id FROM sync_id_imap
                 WHERE source_id = ?1 AND type_name = ?2 AND folder = ?3",
            )?
            .query_map(params![source_id, EMAIL_TYPE, name], |row| row.get(0))?
            .collect::<Result<Vec<i64>, _>>()?;
        for eid in &email_ids {
            tx.execute("DELETE FROM emails WHERE id = ?1", params![eid])?;
            email_counts.deleted += 1;
        }
        db::imap_ids::delete_all_emails_in_folder(&tx, source_id, name)?;
        db::imap_state::delete(&tx, source_id, name)?;
        tx.execute("DELETE FROM mailboxes WHERE id = ?1", params![local_id])?;
        db::imap_ids::delete_mailbox(&tx, source_id, name)?;
        mailbox_counts.deleted += 1;
    }
    tx.commit()?;
    Ok(())
}

fn reconcile_folder(
    conn: &mut Connection,
    client: &mut ImapClient,
    control_ctx: &ControlCtx,
    pool: &WorkerPool,
    folder: &ResolvedFolder,
    opts: RunOpts,
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let RunOpts {
        source_id,
        fetch_batch,
        include_deleted: _,
        logger,
    } = opts;

    if let Some(status) = folder.status.as_ref()
        && status.messages == Some(0)
        && let Some(uv) = status.uidvalidity
        && let Some(un) = status.uidnext
    {
        let local = db::imap_ids::email_uids_in_folder(conn, source_id, &folder.name)?;
        if local.is_empty() {
            let now = OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .unwrap_or_else(|_| String::from("1970-01-01T00:00:00Z"));
            db::imap_state::upsert(conn, source_id, &folder.name, uv as u32, un as u32, &now)?;
            log_at(
                logger,
                LEVEL_PROGRESS,
                &format!("folder {:?}: empty (LIST-STATUS), skipped", folder.name),
            );
            return Ok(());
        }
    }

    let resp = select_folder(client, control_ctx, &folder.name)?;
    let mut uidvalidity: u32 = 0;
    let mut uidnext: u32 = 0;
    for u in &resp.untagged {
        if let Untagged::StatusLine(line) = u
            && let Some(code) = &line.code
            && let Some(args) = &line.code_args
        {
            match code.as_str() {
                "UIDVALIDITY" => {
                    if let Ok(n) = args.parse() {
                        uidvalidity = n;
                    }
                }
                "UIDNEXT" => {
                    if let Ok(n) = args.parse() {
                        uidnext = n;
                    }
                }
                _ => {}
            }
        }
    }
    if uidvalidity == 0 {
        return Err(Error::Partial(format!(
            "SELECT {:?}: server gave no UIDVALIDITY",
            folder.name
        )));
    }

    if let Some(prev) = db::imap_state::get(conn, source_id, &folder.name)?
        && prev.uidvalidity != uidvalidity
    {
        log_at(
            logger,
            LEVEL_DEFAULT,
            &format!(
                "folder {:?}: UIDVALIDITY changed {} -> {} (wiping local rows)",
                folder.name, prev.uidvalidity, uidvalidity
            ),
        );
        wipe_folder_emails(conn, source_id, &folder.name, counts)?;
    }

    let server_uids = call_with_retry(client, control_ctx, select_uids)
        .map_err(|e| Error::Partial(format!("UID enum {:?}: {e}", folder.name)))?;
    let local_uids =
        db::imap_ids::email_uids_at_validity(conn, source_id, &folder.name, uidvalidity)?;
    let diff = diff_uids(&local_uids, &server_uids);

    log_at(
        logger,
        LEVEL_PROGRESS,
        &format!(
            "folder {:?}: new={} vanished={} present={}",
            folder.name,
            diff.new.len(),
            diff.vanished.len(),
            diff.present.len()
        ),
    );

    if !diff.vanished.is_empty() {
        delete_vanished_emails(
            conn,
            source_id,
            &folder.name,
            uidvalidity,
            &diff.vanished,
            counts,
        )?;
    }

    if !diff.new.is_empty() {
        let mailbox_local = db::imap_ids::local_for_mailbox(conn, source_id, &folder.name)?
            .ok_or_else(|| {
                Error::Partial(format!(
                    "mailbox {:?} missing from sync_id_imap",
                    folder.name
                ))
            })?;
        let batches: Vec<&[u32]> = chunks(&diff.new, fetch_batch);
        let n_batches = batches.len();
        for batch in &batches {
            pool.submit(FetchJob {
                folder: folder.name.clone(),
                uidvalidity,
                uids: batch.to_vec(),
            });
        }
        let keepalive_interval = std::time::Duration::from_secs(45);
        let mut chunks_done: usize = 0;
        let tx = conn.transaction()?;
        let target = FetchTarget {
            folder: folder.name.as_str(),
            uidvalidity,
            mailbox_local,
        };
        while chunks_done < n_batches {
            let event = loop {
                match pool.recv_timeout(keepalive_interval) {
                    Ok(r) => break r,
                    Err(RecvTimeoutError::Timeout) => {
                        let _ = control_run_collect(client, control_ctx, "NOOP");
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        return Err(Error::Partial("worker channel disconnected".to_owned()));
                    }
                }
            };
            match event {
                FetchEvent::Item { attrs, .. } => {
                    insert_single_message(&tx, &target, &attrs, opts, counts)?;
                }
                FetchEvent::ChunkDone {
                    folder: chunk_folder,
                    outcome,
                    uids_requested,
                    ..
                } => {
                    chunks_done += 1;
                    if let Err(e) = outcome {
                        log_at(
                            logger,
                            LEVEL_DEFAULT,
                            &format!("UID FETCH chunk failed for folder {chunk_folder:?}: {e}",),
                        );
                        counts.failed += uids_requested.len() as u64;
                    }
                }
            }
        }
        tx.commit()?;
    }

    if !diff.present.is_empty() {
        counts.updated += refresh_present_flags(
            conn,
            client,
            control_ctx,
            &folder.name,
            uidvalidity,
            &diff.present,
            opts,
        )?;
    }

    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| String::from("1970-01-01T00:00:00Z"));
    db::imap_state::upsert(conn, source_id, &folder.name, uidvalidity, uidnext, &now)?;
    Ok(())
}

fn wipe_folder_emails(
    conn: &mut Connection,
    source_id: i64,
    folder: &str,
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let tx = conn.transaction()?;
    let ids: Vec<i64> = tx
        .prepare(
            "SELECT local_id FROM sync_id_imap
             WHERE source_id = ?1 AND type_name = ?2 AND folder = ?3",
        )?
        .query_map(params![source_id, EMAIL_TYPE, folder], |row| row.get(0))?
        .collect::<Result<Vec<i64>, _>>()?;
    for id in &ids {
        tx.execute("DELETE FROM emails WHERE id = ?1", params![id])?;
        counts.deleted += 1;
    }
    db::imap_ids::delete_all_emails_in_folder(&tx, source_id, folder)?;
    tx.commit()?;
    Ok(())
}

fn delete_vanished_emails(
    conn: &mut Connection,
    source_id: i64,
    folder: &str,
    uidvalidity: u32,
    uids: &[u32],
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let tx = conn.transaction()?;
    for &uid in uids {
        if let Some(local_id) =
            db::imap_ids::local_for_email(&tx, source_id, folder, uidvalidity, uid)?
        {
            tx.execute("DELETE FROM emails WHERE id = ?1", params![local_id])?;
            db::imap_ids::delete_email(&tx, source_id, folder, uidvalidity, uid)?;
            counts.deleted += 1;
        }
    }
    tx.commit()?;
    Ok(())
}

pub(super) struct FetchTarget<'a> {
    pub folder: &'a str,
    pub uidvalidity: u32,
    pub mailbox_local: i64,
}

fn insert_single_message(
    tx: &rusqlite::Transaction<'_>,
    target: &FetchTarget<'_>,
    attrs: &fetch::FetchAttrs,
    opts: RunOpts,
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let RunOpts {
        source_id,
        fetch_batch: _,
        include_deleted,
        logger,
    } = opts;
    let folder = target.folder;
    let uidvalidity = target.uidvalidity;
    let mailbox_local = target.mailbox_local;

    let Some(uid) = attrs.uid else {
        return Ok(());
    };
    let Some(body) = attrs.body.as_ref() else {
        counts.skipped += 1;
        return Ok(());
    };
    if let Some(declared_size) = attrs.size
        && (declared_size as usize) != body.len()
    {
        log_at(
            logger,
            LEVEL_DEFAULT,
            &format!(
                "folder {folder:?} uid {uid}: BODY[] {} bytes vs RFC822.SIZE {declared_size}, importing the fetched literal",
                body.len()
            ),
        );
    }
    let translation = translate_flags(&attrs.flags, include_deleted);
    if translation.has_deleted_flag && !include_deleted {
        counts.skipped += 1;
        return Ok(());
    }
    let received_at = match attrs.internaldate.as_deref() {
        Some(s) => imap_internaldate_to_rfc3339(s)?,
        None => OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned()),
    };
    let blob_id = db::blobs::intern_blob(tx, body)?;
    let message_match = index_to_json(&email_index_from_blob(body));
    let mailbox_ids = Value::Array(vec![Value::from(mailbox_local)]);
    let keywords = Value::Array(
        translation
            .keywords
            .iter()
            .map(|k| Value::String(k.clone()))
            .collect(),
    );
    tx.execute(
        "INSERT INTO emails (blob_id, received_at, mailbox_ids, keywords, message_match)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            blob_id,
            received_at,
            mailbox_ids.to_string(),
            keywords.to_string(),
            message_match,
        ],
    )?;
    let email_local = tx.last_insert_rowid();
    db::imap_ids::insert_email(tx, source_id, folder, uidvalidity, uid, email_local)?;
    counts.created += 1;
    counts.fetched += 1;
    Ok(())
}

fn refresh_present_flags(
    conn: &mut Connection,
    client: &mut ImapClient,
    control_ctx: &ControlCtx,
    folder: &str,
    uidvalidity: u32,
    present: &[u32],
    opts: RunOpts,
) -> Result<u64, Error> {
    let RunOpts {
        source_id,
        fetch_batch,
        include_deleted,
        logger: _,
    } = opts;
    let mut updated: u64 = 0;
    let tx = conn.transaction()?;
    for batch in chunks(present, fetch_batch.max(1)) {
        let set = command::format_uid_set(batch, true);
        let resp = control_run_collect(
            client,
            control_ctx,
            &command::uid_fetch(&set, &["UID", "FLAGS"]),
        )?;
        for u in &resp.untagged {
            let Some(attrs) = fetch::extract(u) else {
                continue;
            };
            let Some(uid) = attrs.uid else {
                continue;
            };
            let translation = translate_flags(&attrs.flags, include_deleted);
            if translation.has_deleted_flag && !include_deleted {
                continue;
            }
            let Some((local_id, existing)) =
                load_email_keywords(&tx, source_id, folder, uidvalidity, uid)?
            else {
                continue;
            };
            if keyword_set_differs(&existing, &translation.keywords) {
                let json = Value::Array(
                    translation
                        .keywords
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                );
                tx.execute(
                    "UPDATE emails SET keywords = ?1 WHERE id = ?2",
                    params![json.to_string(), local_id],
                )?;
                updated += 1;
            }
        }
    }
    tx.commit()?;
    Ok(updated)
}

fn load_email_keywords(
    tx: &rusqlite::Transaction<'_>,
    source_id: i64,
    folder: &str,
    uidvalidity: u32,
    uid: u32,
) -> Result<Option<(i64, Vec<String>)>, Error> {
    let Some(local_id) = db::imap_ids::local_for_email(tx, source_id, folder, uidvalidity, uid)?
    else {
        return Ok(None);
    };
    let kw_json: String = tx.query_row(
        "SELECT keywords FROM emails WHERE id = ?1",
        params![local_id],
        |r| r.get(0),
    )?;
    let kws: Vec<String> = serde_json::from_str(&kw_json).unwrap_or_default();
    Ok(Some((local_id, kws)))
}

fn keyword_set_differs(a: &[String], b: &[String]) -> bool {
    let sa: HashSet<&str> = a.iter().map(String::as_str).collect();
    let sb: HashSet<&str> = b.iter().map(String::as_str).collect();
    sa != sb
}

fn dry_run_summary(
    conn: &Connection,
    source_id: Option<i64>,
    client: &mut ImapClient,
    control_ctx: &ControlCtx,
    folders: &[ResolvedFolder],
    logger: Logger,
) -> Result<Summary, Error> {
    let mut mailbox = TypeCounts::default();
    let mut email = TypeCounts::default();

    let server_set: HashSet<String> = folders.iter().map(|f| f.name.clone()).collect();
    let local_mailboxes = match source_id {
        Some(sid) => db::imap_ids::mailbox_folders(conn, sid)?,
        None => HashMap::new(),
    };
    let new_folders: Vec<&str> = folders
        .iter()
        .map(|f| f.name.as_str())
        .filter(|n| !local_mailboxes.contains_key(*n))
        .collect();
    let vanished_folder_names: Vec<&str> = local_mailboxes
        .keys()
        .filter(|n| !server_set.contains(n.as_str()))
        .map(|s| s.as_str())
        .collect();
    mailbox.created += new_folders.len() as u64;
    mailbox.deleted += vanished_folder_names.len() as u64;
    mailbox.fetched += new_folders.len() as u64;
    if !new_folders.is_empty() {
        log_at(
            logger,
            LEVEL_PROGRESS,
            &format!(
                "dry-run: folders new={} vanished={}",
                new_folders.len(),
                vanished_folder_names.len()
            ),
        );
    }

    for folder in folders {
        let select_resp = match select_folder(client, control_ctx, &folder.name) {
            Ok(r) => r,
            Err(e) => {
                log_at(
                    logger,
                    LEVEL_DEFAULT,
                    &format!("dry-run SELECT {:?}: {e}", folder.name),
                );
                email.failed += 1;
                continue;
            }
        };
        let mut server_uidvalidity: u32 = 0;
        for u in &select_resp.untagged {
            if let Untagged::StatusLine(line) = u
                && line.code.as_deref() == Some("UIDVALIDITY")
                && let Some(args) = &line.code_args
                && let Ok(n) = args.parse()
            {
                server_uidvalidity = n;
                break;
            }
        }
        let server_uids = match call_with_retry(client, control_ctx, select_uids) {
            Ok(v) => v,
            Err(e) => {
                log_at(
                    logger,
                    LEVEL_DEFAULT,
                    &format!("dry-run UID enum {:?}: {e}", folder.name),
                );
                email.failed += 1;
                continue;
            }
        };
        let local_pairs = match source_id {
            Some(sid) => db::imap_ids::email_uids_in_folder(conn, sid, &folder.name)?,
            None => HashMap::new(),
        };
        let prev_state = match source_id {
            Some(sid) => db::imap_state::get(conn, sid, &folder.name)?,
            None => None,
        };
        let uv_changed = prev_state
            .as_ref()
            .is_some_and(|p| p.uidvalidity != server_uidvalidity && server_uidvalidity != 0);
        let local_uids: Vec<u32> = if uv_changed {
            Vec::new()
        } else {
            local_pairs
                .keys()
                .filter(|(v, _)| server_uidvalidity == 0 || *v == server_uidvalidity)
                .map(|(_, u)| *u)
                .collect()
        };
        let diff = diff_uids(&local_uids, &server_uids);
        email.created += diff.new.len() as u64;
        email.deleted += diff.vanished.len() as u64;
        email.fetched += diff.new.len() as u64;
        let uv_note = if uv_changed {
            format!(
                " (UIDVALIDITY {} -> {})",
                prev_state.as_ref().map_or(0, |p| p.uidvalidity),
                server_uidvalidity,
            )
        } else {
            String::new()
        };
        log_at(
            logger,
            LEVEL_PROGRESS,
            &format!(
                "dry-run: {:?} new={} vanished={} present={}{uv_note}",
                folder.name,
                diff.new.len(),
                diff.vanished.len(),
                diff.present.len(),
            ),
        );
    }
    for name in &vanished_folder_names {
        if let Some(sid) = source_id {
            let n = db::imap_ids::email_uids_in_folder(conn, sid, name)?.len() as u64;
            email.deleted += n;
            log_at(
                logger,
                LEVEL_PROGRESS,
                &format!(
                    "dry-run: folder {:?} vanished ({n} emails would be removed)",
                    name
                ),
            );
        }
    }

    let _ = client.logout();
    Ok(Summary {
        per_type: vec![("mailbox", mailbox), ("email", email)],
        retries_observed: control_ctx.backoff.total_retries(),
        retry_after_sleeps: control_ctx.backoff.transient_retries() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_endpoint_imaps_defaults_to_993() {
        let e = parse_endpoint("imaps://mail.example.com").unwrap();
        assert_eq!(e.host, "mail.example.com");
        assert_eq!(e.port, 993);
        assert!(e.implicit_tls);
    }

    #[test]
    fn parse_endpoint_imap_defaults_to_143() {
        let e = parse_endpoint("imap://mail.example.com").unwrap();
        assert_eq!(e.host, "mail.example.com");
        assert_eq!(e.port, 143);
        assert!(!e.implicit_tls);
    }

    #[test]
    fn parse_endpoint_explicit_port() {
        let e = parse_endpoint("imaps://mail.example.com:1993").unwrap();
        assert_eq!(e.port, 1993);
    }

    #[test]
    fn parse_endpoint_rejects_other_schemes() {
        let err = parse_endpoint("http://example.com").unwrap_err();
        assert!(matches!(err, Error::Usage(_)));
    }
}
