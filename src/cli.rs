/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand};
use regex::Regex;

use crate::error::Error;
use crate::exchange_ews::oauth::OAuthFlow;
use crate::exchange_ews::types::MailboxKind;
use crate::exchange_graph::types::{EventBodyFormat, MailboxKind as GraphMailboxKind, Surfaces};
use crate::inspect::InspectConfig;
use crate::jmap::account::AccountSelector;
use crate::jmap::http::Auth;
use crate::logging::Logger;
use crate::secret;
use crate::sync::import_dav::{DavAuth, DavImportConfig, DavKindArg};
use crate::sync::import_exchange_ews::{EwsAuth, EwsImportConfig};
use crate::sync::import_exchange_graph::{GraphAuth, GraphImportConfig};
use crate::sync::import_imap::{ImapAuth, ImapImportConfig};
use crate::sync::import_maildir::MaildirImportConfig;
use crate::sync::import_managesieve::{ManageSieveAuth, ManageSieveImportConfig};
use crate::sync::import_takeout::TakeoutImportConfig;
use crate::sync::{CommonConfig, ConnectConfig, ExportConfig, ImportConfig};
use crate::types::{ObjectType, parse_object_list};

#[derive(Parser)]
#[command(
    name = "vandelay",
    version,
    about = "Vandelay: the JMAP importer-exporter"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Read a source account into a local archive")]
    Import {
        #[command(subcommand)]
        source: Source,
    },
    #[command(about = "Push a local archive into a target JMAP server")]
    Export(Box<ExportArgs>),
    #[command(about = "Show the contents of a local archive (read-only)")]
    Inspect(Box<InspectArgs>),
}

#[derive(Subcommand)]
enum Source {
    #[command(about = "JMAP server")]
    Jmap(Box<JmapImportArgs>),
    #[command(about = "IMAP server")]
    Imap(Box<ImapImportArgs>),
    #[command(about = "CalDAV server")]
    Caldav(Box<DavImportArgs>),
    #[command(about = "CardDAV server")]
    Carddav(Box<DavImportArgs>),
    #[command(about = "WebDAV server (plain files)")]
    Webdav(Box<DavImportArgs>),
    #[command(about = "ManageSieve server (Sieve scripts only)")]
    Managesieve(Box<ManageSieveImportArgs>),
    #[command(about = "Local Maildir++ tree")]
    Maildir(Box<MaildirImportArgs>),
    #[command(about = "Google Takeout (or any mbox/ics/vcf directory tree)")]
    Takeout(Box<TakeoutImportArgs>),
    #[command(about = "MS Exchange via EWS (SOAP/XML)", name = "exchange-ews")]
    ExchangeEws(Box<ExchangeEwsImportArgs>),
    #[command(
        about = "MS Exchange Online via Microsoft Graph",
        name = "exchange-graph"
    )]
    ExchangeGraph(Box<ExchangeGraphImportArgs>),
}

#[derive(Args)]
struct GlobalArgs {
    #[arg(
        short = 'j',
        long,
        value_name = "N",
        help = "Worker pool size (default: logical CPUs)"
    )]
    threads: Option<usize>,

    #[arg(long, help = "Compute and report the full plan; perform no writes")]
    dry_run: bool,

    #[arg(
        short = 'v',
        long,
        action = ArgAction::Count,
        conflicts_with = "quiet",
        help = "Increase log verbosity (repeatable, max -vvv)",
        long_help = "Increase log verbosity (repeatable, max -vvv).\n  \
            (default) per-type start/finish lines and a final summary\n  \
            -v   per-chunk progress, account resolution, retry notices\n  \
            -vv  every protocol call (method/command, target, status, timing)\n  \
            -vvv full request/response bodies and backoff delays (stderr)"
    )]
    verbose: u8,

    #[arg(
        short = 'q',
        long,
        help = "Quiet: warnings and errors only (verbosity level 0)"
    )]
    quiet: bool,

    #[arg(
        long,
        value_name = "N",
        default_value_t = 5,
        help = "Max retry attempts per request on transient failures"
    )]
    max_retries: u32,

    #[arg(long, help = "Accept self-signed / invalid TLS certificates")]
    allow_invalid_certs: bool,
}

#[derive(Args)]
#[command(group(clap::ArgGroup::new("auth").required(true).args(["auth_basic", "auth_bearer"])))]
#[command(group(clap::ArgGroup::new("account").required(true).args(["account_id", "account_name"])))]
struct JmapImportArgs {
    #[arg(long, value_name = "URL", help = "Base URL or full JMAP session URL")]
    url: String,

    #[arg(long, value_name = "USER", help = "HTTP Basic user")]
    auth_basic: Option<String>,

    #[arg(
        long,
        value_name = "PASS",
        help = "Basic password (prefer $VANDELAY_PASSWORD or prompt)"
    )]
    auth_password: Option<String>,

    #[arg(
        long,
        value_name = "TOKEN",
        num_args = 0..=1,
        help = "Bearer token (value optional: falls back to $VANDELAY_TOKEN or prompt)"
    )]
    auth_bearer: Option<Option<String>>,

    #[arg(long, value_name = "ID", help = "Source account id")]
    account_id: Option<String>,

    #[arg(long, value_name = "NAME", help = "Source account name")]
    account_name: Option<String>,

    #[arg(
        long,
        value_name = "LIST",
        help = "Comma-separated type list (default: all supported)"
    )]
    objects: Option<String>,

    #[arg(
        long,
        help = "Permit importing when the archive records a different JMAP source"
    )]
    allow_source_change: bool,

    #[command(flatten)]
    global: GlobalArgs,

    #[arg(
        value_name = "ARCHIVE",
        help = "Local SQLite archive (created if absent)"
    )]
    archive: PathBuf,
}

#[derive(Args)]
#[command(group(clap::ArgGroup::new("auth").required(true).args(["auth_basic", "auth_bearer"])))]
struct ImapImportArgs {
    #[arg(
        long,
        value_name = "URL",
        help = "imap://host[:port] or imaps://host[:port]"
    )]
    url: String,

    #[arg(
        long,
        value_name = "USER",
        help = "PLAIN (preferred) with LOGIN fallback"
    )]
    auth_basic: Option<String>,

    #[arg(
        long,
        value_name = "PASS",
        help = "Basic password (prefer $VANDELAY_PASSWORD or prompt)"
    )]
    auth_password: Option<String>,

    #[arg(
        long,
        value_name = "TOKEN",
        num_args = 0..=1,
        help = "Bearer token for XOAUTH2 (value optional: falls back to $VANDELAY_TOKEN or prompt)"
    )]
    auth_bearer: Option<Option<String>>,

    #[arg(
        long,
        value_name = "USER",
        help = "XOAUTH2 identity (required with --auth-bearer)"
    )]
    auth_user: Option<String>,

    #[arg(long, help = "Permit credentials on imap:// without STARTTLS")]
    allow_cleartext: bool,

    #[arg(long, help = "Enable COMPRESS=DEFLATE post-auth (off by default)")]
    compress: bool,

    #[arg(
        long,
        value_name = "REGEX",
        action = ArgAction::Append,
        help = "Folder include filter (repeatable)"
    )]
    include: Vec<String>,

    #[arg(
        long,
        value_name = "REGEX",
        action = ArgAction::Append,
        help = "Folder exclude filter (repeatable)"
    )]
    exclude: Vec<String>,

    #[arg(
        long,
        value_name = "ROLE",
        action = ArgAction::Append,
        help = "Drop folders matching a role (repeatable)"
    )]
    exclude_special: Vec<String>,

    #[arg(
        long,
        value_name = "NAME",
        action = ArgAction::Append,
        help = "Exact include; mutually exclusive with --include/--exclude"
    )]
    folder: Vec<String>,

    #[arg(long, help = "Keep only folders returned by LSUB")]
    subscribed_only: bool,

    #[arg(long, help = "Disable SPECIAL-USE name-heuristic fallback")]
    noautomap: bool,

    #[arg(long, help = "Import \\Deleted messages with the $deleted keyword")]
    include_deleted: bool,

    #[arg(
        long,
        value_name = "N",
        default_value_t = 256,
        help = "UIDs per metadata FETCH chunk"
    )]
    fetch_batch: usize,

    #[arg(
        long,
        value_name = "N",
        help = "Parallel IMAP connections for body fetch (1..=8). Default: min(--threads, 8)"
    )]
    imap_connections: Option<usize>,

    #[arg(
        long,
        help = "Permit importing when the archive records a different IMAP source"
    )]
    allow_source_change: bool,

    #[command(flatten)]
    global: GlobalArgs,

    #[arg(
        value_name = "ARCHIVE",
        help = "Local SQLite archive (created if absent)"
    )]
    archive: PathBuf,
}

#[derive(Args)]
#[command(group(clap::ArgGroup::new("auth").required(true).args(["auth_basic", "auth_bearer"])))]
#[command(group(clap::ArgGroup::new("account").required(true).args(["account_id", "account_name"])))]
struct ExportArgs {
    #[arg(long, value_name = "URL", help = "Base URL or full JMAP session URL")]
    url: String,

    #[arg(long, value_name = "USER", help = "HTTP Basic user")]
    auth_basic: Option<String>,

    #[arg(
        long,
        value_name = "PASS",
        help = "Basic password (prefer $VANDELAY_PASSWORD or prompt)"
    )]
    auth_password: Option<String>,

    #[arg(
        long,
        value_name = "TOKEN",
        num_args = 0..=1,
        help = "Bearer token (value optional: falls back to $VANDELAY_TOKEN or prompt)"
    )]
    auth_bearer: Option<Option<String>>,

    #[arg(long, value_name = "ID", help = "Target account id")]
    account_id: Option<String>,

    #[arg(long, value_name = "NAME", help = "Target account name")]
    account_name: Option<String>,

    #[arg(
        long,
        value_name = "LIST",
        help = "Types to export (default: all present in archive)"
    )]
    objects: Option<String>,

    #[arg(long, help = "Delete target objects that match nothing in the archive")]
    prune: bool,

    #[arg(long, help = "Skip the interactive --prune confirmation")]
    yes: bool,

    #[command(flatten)]
    global: GlobalArgs,

    #[arg(value_name = "ARCHIVE", help = "Local SQLite archive")]
    archive: PathBuf,
}

#[derive(Args)]
struct InspectArgs {
    #[arg(value_name = "ARCHIVE", help = "Local SQLite archive")]
    archive: PathBuf,

    #[arg(
        value_name = "TYPE",
        help = "Object type to dump (omit for a summary of all types)"
    )]
    target: Option<String>,

    #[arg(
        long,
        value_name = "N",
        help = "Maximum rows to show (omit to dump every row)"
    )]
    limit: Option<usize>,

    #[arg(
        long,
        value_name = "N",
        default_value_t = 0,
        help = "Skip the first N rows"
    )]
    offset: usize,
}

pub enum Action {
    Import(CommonConfig, ImportConfig),
    ImportImap(CommonConfig, ImapImportConfig),
    ImportDav(CommonConfig, DavImportConfig),
    ImportManageSieve(CommonConfig, ManageSieveImportConfig),
    ImportMaildir(CommonConfig, MaildirImportConfig),
    ImportTakeout(CommonConfig, TakeoutImportConfig),
    ImportExchangeEws(CommonConfig, EwsImportConfig),
    ImportExchangeGraph(CommonConfig, GraphImportConfig),
    Export(CommonConfig, ExportConfig),
    Inspect(InspectConfig),
}

#[derive(Args)]
#[command(group(clap::ArgGroup::new("auth").required(true).args(["auth_basic", "auth_bearer"])))]
pub struct ManageSieveImportArgs {
    #[arg(
        long,
        value_name = "URL",
        help = "sieve://host[:port] or sieves://host[:port]"
    )]
    url: String,

    #[arg(
        long,
        value_name = "USER",
        help = "SASL PLAIN (preferred) with LOGIN fallback"
    )]
    auth_basic: Option<String>,

    #[arg(
        long,
        value_name = "PASS",
        help = "Basic password (prefer $VANDELAY_PASSWORD or prompt)"
    )]
    auth_password: Option<String>,

    #[arg(
        long,
        value_name = "TOKEN",
        num_args = 0..=1,
        help = "Bearer token for OAUTHBEARER (value optional: $VANDELAY_TOKEN or prompt)"
    )]
    auth_bearer: Option<Option<String>>,

    #[arg(
        long,
        value_name = "USER",
        help = "OAUTHBEARER identity (required with --auth-bearer)"
    )]
    auth_user: Option<String>,

    #[arg(long, help = "Permit credentials on sieve:// without STARTTLS")]
    allow_cleartext: bool,

    #[arg(
        long,
        help = "Permit importing when the archive records a different ManageSieve source"
    )]
    allow_source_change: bool,

    #[command(flatten)]
    global: GlobalArgs,

    #[arg(
        value_name = "ARCHIVE",
        help = "Local SQLite archive (created if absent)"
    )]
    archive: PathBuf,
}

#[derive(Args)]
#[command(group(clap::ArgGroup::new("auth").required(true).args(["auth_basic", "auth_bearer"])))]
pub struct DavImportArgs {
    #[arg(long, value_name = "URL", help = "http(s)://host[:port][/path]")]
    url: String,

    #[arg(long, value_name = "USER", help = "HTTP Basic user")]
    auth_basic: Option<String>,

    #[arg(
        long,
        value_name = "PASS",
        help = "Basic password (prefer $VANDELAY_PASSWORD or prompt)"
    )]
    auth_password: Option<String>,

    #[arg(
        long,
        value_name = "TOKEN",
        num_args = 0..=1,
        help = "Bearer token (value optional: $VANDELAY_TOKEN or prompt)"
    )]
    auth_bearer: Option<Option<String>>,

    #[arg(long, help = "Permit credentials on http:// (no TLS)")]
    allow_cleartext: bool,

    #[arg(
        long,
        value_name = "N",
        default_value_t = 4,
        help = "Parallel DAV worker connections (hard cap 8)"
    )]
    dav_connections: usize,

    #[arg(
        long,
        value_name = "N",
        default_value_t = 50,
        help = "Hrefs per calendar-/addressbook-multiget REPORT batch"
    )]
    multiget_batch: usize,

    #[arg(
        long,
        help = "Permit importing when the archive records a different DAV source"
    )]
    allow_source_change: bool,

    #[command(flatten)]
    global: GlobalArgs,

    #[arg(
        value_name = "ARCHIVE",
        help = "Local SQLite archive (created if absent)"
    )]
    archive: PathBuf,
}

impl Cli {
    pub fn resolve(self) -> Result<Action, Error> {
        match self.command {
            Command::Import { source } => resolve_import(source),
            Command::Export(args) => resolve_export(*args),
            Command::Inspect(args) => resolve_inspect(*args),
        }
    }
}

fn resolve_import(source: Source) -> Result<Action, Error> {
    match source {
        Source::Jmap(args) => resolve_jmap_import(*args),
        Source::Imap(args) => resolve_imap_import(*args),
        Source::Caldav(args) => resolve_dav_import(*args, DavKindArg::Caldav),
        Source::Carddav(args) => resolve_dav_import(*args, DavKindArg::Carddav),
        Source::Webdav(args) => resolve_dav_import(*args, DavKindArg::Webdav),
        Source::Managesieve(args) => resolve_managesieve_import(*args),
        Source::Maildir(args) => resolve_maildir_import(*args),
        Source::Takeout(args) => resolve_takeout_import(*args),
        Source::ExchangeEws(args) => resolve_exchange_ews_import(*args),
        Source::ExchangeGraph(args) => resolve_exchange_graph_import(*args),
    }
}

fn resolve_managesieve_import(args: ManageSieveImportArgs) -> Result<Action, Error> {
    let common = common_config(&args.global, args.archive);
    let auth = resolve_managesieve_auth(
        args.auth_basic.as_deref(),
        args.auth_password.as_deref(),
        args.auth_bearer.as_ref(),
        args.auth_user.as_deref(),
    )?;
    Ok(Action::ImportManageSieve(
        common,
        ManageSieveImportConfig {
            url: args.url,
            auth,
            allow_cleartext: args.allow_cleartext,
            allow_source_change: args.allow_source_change,
        },
    ))
}

fn resolve_managesieve_auth(
    auth_basic: Option<&str>,
    auth_password: Option<&str>,
    auth_bearer: Option<&Option<String>>,
    auth_user: Option<&str>,
) -> Result<ManageSieveAuth, Error> {
    if let Some(user) = auth_basic {
        if auth_user.is_some() {
            return Err(Error::Usage(
                "--auth-user is only valid with --auth-bearer".to_owned(),
            ));
        }
        let password = secret::resolve(auth_password, "VANDELAY_PASSWORD", "password")?;
        return Ok(ManageSieveAuth::Basic {
            user: user.to_owned(),
            password,
        });
    }
    if auth_password.is_some() {
        return Err(Error::Usage(
            "--auth-password is only valid together with --auth-basic".to_owned(),
        ));
    }
    let bearer = auth_bearer.ok_or_else(|| {
        Error::Usage("exactly one of --auth-basic / --auth-bearer is required".to_owned())
    })?;
    let user = auth_user
        .ok_or_else(|| Error::Usage("--auth-user is required with --auth-bearer".to_owned()))?
        .to_owned();
    let token = secret::resolve(bearer.as_deref(), "VANDELAY_TOKEN", "bearer token")?;
    Ok(ManageSieveAuth::Bearer { user, token })
}

#[derive(Args)]
pub struct MaildirImportArgs {
    #[arg(
        value_name = "MAILDIR",
        help = "Path to the Maildir++ root (a directory containing cur/ new/ tmp/)"
    )]
    maildir: PathBuf,

    #[arg(
        value_name = "ARCHIVE",
        help = "Local SQLite archive (created if absent)"
    )]
    archive: PathBuf,

    #[arg(
        long,
        value_name = "REGEX",
        action = ArgAction::Append,
        help = "Folder include filter against canonical name (repeatable)"
    )]
    include: Vec<String>,

    #[arg(
        long,
        value_name = "REGEX",
        action = ArgAction::Append,
        help = "Folder exclude filter against canonical name (repeatable)"
    )]
    exclude: Vec<String>,

    #[arg(
        long,
        value_name = "NAME",
        action = ArgAction::Append,
        help = "Exact include; mutually exclusive with --include/--exclude"
    )]
    folder: Vec<String>,

    #[arg(long, help = "Disable the name-heuristic role assignment")]
    noautomap: bool,

    #[arg(long, help = "Import T-flagged messages with the $deleted keyword")]
    include_deleted: bool,

    #[arg(
        long,
        help = "Permit importing when the archive records a different maildir path"
    )]
    allow_source_change: bool,

    #[command(flatten)]
    global: GlobalArgs,
}

fn resolve_maildir_import(args: MaildirImportArgs) -> Result<Action, Error> {
    let common = common_config(&args.global, args.archive);
    if !args.folder.is_empty() && (!args.include.is_empty() || !args.exclude.is_empty()) {
        return Err(Error::Usage(
            "--folder is mutually exclusive with --include/--exclude".to_owned(),
        ));
    }
    let include = compile_regexes(&args.include, "--include")?;
    let exclude = compile_regexes(&args.exclude, "--exclude")?;
    Ok(Action::ImportMaildir(
        common,
        MaildirImportConfig {
            maildir: args.maildir,
            include,
            exclude,
            folder: args.folder,
            automap: !args.noautomap,
            include_deleted: args.include_deleted,
            allow_source_change: args.allow_source_change,
        },
    ))
}

#[derive(Args)]
pub struct TakeoutImportArgs {
    #[arg(
        value_name = "PATH",
        help = "Directory tree to scan recursively for .mbox / .ics / .vcf files"
    )]
    path: PathBuf,

    #[arg(
        value_name = "ARCHIVE",
        help = "Local SQLite archive (created if absent)"
    )]
    archive: PathBuf,

    #[arg(long, help = "Disable the system-label role assignment")]
    noautomap: bool,

    #[arg(
        long,
        help = "Permit importing when the archive records a takeout source at a different path"
    )]
    allow_source_change: bool,

    #[command(flatten)]
    global: GlobalArgs,
}

fn resolve_takeout_import(args: TakeoutImportArgs) -> Result<Action, Error> {
    let common = common_config(&args.global, args.archive);
    Ok(Action::ImportTakeout(
        common,
        TakeoutImportConfig {
            takeout_root: args.path,
            allow_source_change: args.allow_source_change,
            automap: !args.noautomap,
        },
    ))
}

fn resolve_dav_import(args: DavImportArgs, kind: DavKindArg) -> Result<Action, Error> {
    let common = common_config(&args.global, args.archive);
    let auth = resolve_dav_auth(
        args.auth_basic.as_deref(),
        args.auth_password.as_deref(),
        args.auth_bearer.as_ref(),
    )?;
    let dav_connections = args.dav_connections.clamp(1, 8).min(common.threads.max(1));
    let multiget_batch = args.multiget_batch.max(1);
    Ok(Action::ImportDav(
        common,
        DavImportConfig {
            kind,
            url: args.url,
            auth,
            allow_cleartext: args.allow_cleartext,
            dav_connections,
            multiget_batch,
            allow_source_change: args.allow_source_change,
        },
    ))
}

fn resolve_dav_auth(
    auth_basic: Option<&str>,
    auth_password: Option<&str>,
    auth_bearer: Option<&Option<String>>,
) -> Result<DavAuth, Error> {
    if let Some(user) = auth_basic {
        let password = secret::resolve(auth_password, "VANDELAY_PASSWORD", "password")?;
        return Ok(DavAuth::Basic {
            user: user.to_owned(),
            password,
        });
    }
    if auth_password.is_some() {
        return Err(Error::Usage(
            "--auth-password is only valid together with --auth-basic".to_owned(),
        ));
    }
    let bearer = auth_bearer.ok_or_else(|| {
        Error::Usage("exactly one of --auth-basic / --auth-bearer is required".to_owned())
    })?;
    let token = secret::resolve(bearer.as_deref(), "VANDELAY_TOKEN", "bearer token")?;
    Ok(DavAuth::Bearer { token })
}

fn resolve_jmap_import(args: JmapImportArgs) -> Result<Action, Error> {
    let common = common_config(&args.global, args.archive);
    let auth = resolve_auth(
        args.auth_basic.as_deref(),
        args.auth_password.as_deref(),
        args.auth_bearer.as_ref(),
    )?;
    let account = resolve_account(args.account_id, args.account_name)?;
    let objects = args.objects.as_deref().map(parse_object_list).transpose()?;

    Ok(Action::Import(
        common,
        ImportConfig {
            connect: ConnectConfig {
                url: args.url,
                auth,
                account,
            },
            objects,
            allow_source_change: args.allow_source_change,
        },
    ))
}

fn resolve_imap_import(args: ImapImportArgs) -> Result<Action, Error> {
    let common = common_config(&args.global, args.archive);
    let auth = resolve_imap_auth(
        args.auth_basic.as_deref(),
        args.auth_password.as_deref(),
        args.auth_bearer.as_ref(),
        args.auth_user.as_deref(),
    )?;
    if !args.folder.is_empty() && (!args.include.is_empty() || !args.exclude.is_empty()) {
        return Err(Error::Usage(
            "--folder is mutually exclusive with --include/--exclude".to_owned(),
        ));
    }
    let include = compile_regexes(&args.include, "--include")?;
    let exclude = compile_regexes(&args.exclude, "--exclude")?;
    let imap_connections = args.imap_connections.unwrap_or(common.threads).clamp(1, 8);

    Ok(Action::ImportImap(
        common,
        ImapImportConfig {
            url: args.url,
            auth,
            allow_cleartext: args.allow_cleartext,
            compress: args.compress,
            include,
            exclude,
            exclude_special: args.exclude_special,
            folder: args.folder,
            subscribed_only: args.subscribed_only,
            automap: !args.noautomap,
            include_deleted: args.include_deleted,
            fetch_batch: args.fetch_batch,
            imap_connections,
            allow_source_change: args.allow_source_change,
        },
    ))
}

fn compile_regexes(raw: &[String], flag: &str) -> Result<Vec<Regex>, Error> {
    let mut out = Vec::with_capacity(raw.len());
    for r in raw {
        let re =
            Regex::new(r).map_err(|e| Error::Usage(format!("{flag} {r:?}: invalid regex: {e}")))?;
        out.push(re);
    }
    Ok(out)
}

fn resolve_imap_auth(
    auth_basic: Option<&str>,
    auth_password: Option<&str>,
    auth_bearer: Option<&Option<String>>,
    auth_user: Option<&str>,
) -> Result<ImapAuth, Error> {
    if let Some(user) = auth_basic {
        if auth_user.is_some() {
            return Err(Error::Usage(
                "--auth-user is only valid with --auth-bearer".to_owned(),
            ));
        }
        let password = secret::resolve(auth_password, "VANDELAY_PASSWORD", "password")?;
        return Ok(ImapAuth::Basic {
            user: user.to_owned(),
            password,
        });
    }
    if auth_password.is_some() {
        return Err(Error::Usage(
            "--auth-password is only valid together with --auth-basic".to_owned(),
        ));
    }
    let bearer = auth_bearer.ok_or_else(|| {
        Error::Usage("exactly one of --auth-basic / --auth-bearer is required".to_owned())
    })?;
    let user = auth_user
        .ok_or_else(|| Error::Usage("--auth-user is required with --auth-bearer".to_owned()))?
        .to_owned();
    let token = secret::resolve(bearer.as_deref(), "VANDELAY_TOKEN", "bearer token")?;
    Ok(ImapAuth::Bearer { user, token })
}

fn resolve_export(args: ExportArgs) -> Result<Action, Error> {
    let common = common_config(&args.global, args.archive);
    let auth = resolve_auth(
        args.auth_basic.as_deref(),
        args.auth_password.as_deref(),
        args.auth_bearer.as_ref(),
    )?;
    let account = resolve_account(args.account_id, args.account_name)?;
    let objects = args.objects.as_deref().map(parse_object_list).transpose()?;

    Ok(Action::Export(
        common,
        ExportConfig {
            connect: ConnectConfig {
                url: args.url,
                auth,
                account,
            },
            objects,
            prune: args.prune,
            yes: args.yes,
        },
    ))
}

fn resolve_inspect(args: InspectArgs) -> Result<Action, Error> {
    let target = match args.target.as_deref() {
        Some(token) => Some(ObjectType::parse(token)?),
        None => None,
    };
    if let Some(limit) = args.limit
        && limit == 0
    {
        return Err(Error::Usage("--limit must be greater than zero".to_owned()));
    }
    Ok(Action::Inspect(InspectConfig {
        archive: args.archive,
        target,
        limit: args.limit,
        offset: args.offset,
    }))
}

#[derive(Args)]
#[command(group(clap::ArgGroup::new("ews_auth").required(true).args([
    "auth_basic", "auth_bearer",
])))]
pub struct ExchangeEwsImportArgs {
    #[arg(long, value_name = "URL", help = "Fully-qualified EWS endpoint URL")]
    url: Option<String>,

    #[arg(
        long,
        value_name = "EMAIL",
        help = "Target mailbox SMTP address (required for autodiscover and app-only OAuth)"
    )]
    mailbox: Option<String>,

    #[arg(
        long,
        value_name = "KIND",
        default_value = "primary",
        help = "Mailbox surface: primary | archive | public-folders"
    )]
    mailbox_kind: String,

    #[arg(long, value_name = "USER", help = "Basic auth user (on-prem only)")]
    auth_basic: Option<String>,

    #[arg(
        long,
        value_name = "PASS",
        help = "Basic password (prefer $VANDELAY_PASSWORD or prompt)"
    )]
    auth_password: Option<String>,

    #[arg(
        long,
        value_name = "TOKEN",
        num_args = 0..=1,
        help = "Bearer token (value optional: $VANDELAY_TOKEN or device-code flow)"
    )]
    auth_bearer: Option<Option<String>>,

    #[arg(
        long,
        value_name = "TENANT",
        help = "Entra tenant id or .onmicrosoft.com short name"
    )]
    ews_tenant: Option<String>,

    #[arg(long, value_name = "ID", help = "Entra app (client) id")]
    ews_client_id: Option<String>,

    #[arg(
        long,
        value_name = "SECRET",
        help = "OAuth client secret (app-only flow; prefer $VANDELAY_EWS_CLIENT_SECRET)"
    )]
    ews_client_secret: Option<String>,

    #[arg(long, help = "Use the OAuth device-code interactive flow")]
    ews_device_code: bool,

    #[arg(
        long,
        value_name = "N",
        default_value_t = 4,
        help = "Parallel EWS worker connections (hard cap 8)"
    )]
    ews_connections: usize,

    #[arg(
        long,
        value_name = "N",
        default_value_t = 20,
        help = "ItemIds per GetItem batch"
    )]
    ews_getitem_batch: usize,

    #[arg(
        long,
        value_name = "N",
        default_value_t = 5,
        help = "AttachmentIds per GetAttachment batch"
    )]
    ews_attachment_batch: usize,

    #[arg(long, help = "Force FindItem even when SyncFolderItems would work")]
    ews_no_syncfolderitems: bool,

    #[arg(
        long,
        help = "Permit importing when the archive records a different EWS source"
    )]
    allow_source_change: bool,

    #[command(flatten)]
    global: GlobalArgs,

    #[arg(
        value_name = "ARCHIVE",
        help = "Local SQLite archive (created if absent)"
    )]
    archive: PathBuf,
}

fn resolve_exchange_ews_import(args: ExchangeEwsImportArgs) -> Result<Action, Error> {
    let archive = args.archive.clone();
    let common = common_config(&args.global, archive);
    let mailbox_kind = match args.mailbox_kind.as_str() {
        "primary" => MailboxKind::Primary,
        "archive" => MailboxKind::Archive,
        "public-folders" => MailboxKind::PublicFolders,
        other => {
            return Err(Error::Usage(format!(
                "--mailbox-kind must be primary | archive | public-folders, got {other:?}"
            )));
        }
    };
    let auth = resolve_exchange_ews_auth(&args)?;
    let ews_connections = args.ews_connections.clamp(1, 8).min(common.threads.max(1));
    let config = EwsImportConfig {
        url: args.url,
        mailbox: args.mailbox,
        mailbox_kind,
        auth,
        ews_connections,
        getitem_batch: args.ews_getitem_batch.max(1),
        attachment_batch: args.ews_attachment_batch.max(1),
        use_syncfolderitems: !args.ews_no_syncfolderitems,
        allow_source_change: args.allow_source_change,
    };
    Ok(Action::ImportExchangeEws(common, config))
}

fn resolve_exchange_ews_auth(args: &ExchangeEwsImportArgs) -> Result<EwsAuth, Error> {
    if let Some(user) = args.auth_basic.as_deref() {
        let password = secret::resolve(
            args.auth_password.as_deref(),
            "VANDELAY_PASSWORD",
            "password",
        )?;
        return Ok(EwsAuth::Basic {
            user: user.to_owned(),
            password,
        });
    }
    if args.auth_password.is_some() {
        return Err(Error::Usage(
            "--auth-password is only valid with --auth-basic".to_owned(),
        ));
    }
    let Some(bearer) = args.auth_bearer.as_ref() else {
        return Err(Error::Usage(
            "exactly one of --auth-basic / --auth-bearer is required".to_owned(),
        ));
    };
    if let Some(token) = bearer.clone() {
        return Ok(EwsAuth::Bearer { token });
    }
    let tenant = args
        .ews_tenant
        .clone()
        .unwrap_or_else(|| "common".to_owned());
    let client_id = args.ews_client_id.clone().ok_or_else(|| {
        Error::Usage(
            "--ews-client-id is required when --auth-bearer is used without an inline token"
                .to_owned(),
        )
    })?;
    if args.ews_device_code {
        return Ok(EwsAuth::OAuth(OAuthFlow::DeviceCode { tenant, client_id }));
    }
    if let Some(secret) = args.ews_client_secret.clone() {
        let resolved = secret::resolve(
            Some(secret.as_str()),
            "VANDELAY_EWS_CLIENT_SECRET",
            "client secret",
        )?;
        return Ok(EwsAuth::OAuth(OAuthFlow::ClientCredentials {
            tenant,
            client_id,
            client_secret: resolved,
        }));
    }
    if let Ok(env_secret) = std::env::var("VANDELAY_EWS_CLIENT_SECRET")
        && !env_secret.is_empty()
    {
        return Ok(EwsAuth::OAuth(OAuthFlow::ClientCredentials {
            tenant,
            client_id,
            client_secret: env_secret,
        }));
    }
    let token = secret::resolve(None, "VANDELAY_TOKEN", "bearer token")?;
    Ok(EwsAuth::Bearer { token })
}

#[derive(Args)]
pub struct ExchangeGraphImportArgs {
    #[arg(
        long,
        value_name = "UUID",
        help = "Entra app (client) id (required unless --access-token is given)"
    )]
    client_id: Option<String>,

    #[arg(
        long,
        value_name = "ID",
        default_value = "common",
        help = "Entra authority tenant"
    )]
    tenant: String,

    #[arg(
        long,
        value_name = "UPN|UUID",
        help = "Target a different mailbox the signed-in account can read"
    )]
    user: Option<String>,

    #[arg(
        long,
        value_name = "KIND",
        default_value = "primary",
        value_parser = parse_graph_mailbox_kind,
        help = "Mailbox surface: primary | archive (public-folders is rejected: use exchange-ews)"
    )]
    mailbox_kind: GraphMailboxKind,

    #[arg(
        long,
        value_name = "TOKEN",
        num_args = 0..=1,
        help = "Pre-acquired bearer token (skip device-code flow). Resolves: flag value -> $VANDELAY_GRAPH_TOKEN -> prompt."
    )]
    access_token: Option<Option<String>>,

    #[arg(
        long,
        value_name = "LIST",
        help = "Comma-separated surface list: mail | calendar | contacts | files (default: all)"
    )]
    objects: Option<String>,

    #[arg(
        long,
        value_name = "FMT",
        default_value = "text",
        value_parser = parse_event_body_format,
        help = "outlook.body-content-type for event GETs: text | html"
    )]
    event_body_format: EventBodyFormat,

    #[arg(
        long,
        value_name = "N",
        default_value_t = 4,
        help = "Parallel Graph worker connections (hard cap 16)"
    )]
    graph_connections: usize,

    #[arg(
        long,
        value_name = "N",
        default_value_t = 100,
        help = "Per-page size ($top query parameter, range [1, 1000])"
    )]
    top: usize,

    #[arg(
        long,
        value_name = "N",
        default_value_t = 5,
        help = "Years either side of today to scan for edited recurrence occurrences (max 5)",
        long_help = "Years either side of today to scan for edited recurrence occurrences.\n  \
            Graph reports a series exception only inside an expanded calendar view, and caps\n  \
            any single view at five years, so vandelay scans [today - N, today] and\n  \
            [today, today + N]. An occurrence edited outside that span imports with the\n  \
            series default instead of its edit."
    )]
    exception_window_years: i32,

    #[arg(
        long,
        value_name = "URL",
        default_value = "https://graph.microsoft.com/v1.0",
        help = "Microsoft Graph API base URL (override for national clouds or tests)",
        hide = true
    )]
    api_base: String,

    #[arg(
        long,
        help = "Permit importing when the archive records a different exchange_graph source"
    )]
    allow_source_change: bool,

    #[command(flatten)]
    global: GlobalArgs,

    #[arg(
        value_name = "ARCHIVE",
        help = "Local SQLite archive (created if absent)"
    )]
    archive: PathBuf,
}

fn parse_graph_mailbox_kind(s: &str) -> Result<GraphMailboxKind, String> {
    GraphMailboxKind::parse(s).map_err(|e| e.to_string())
}

fn parse_event_body_format(s: &str) -> Result<EventBodyFormat, String> {
    EventBodyFormat::parse(s).map_err(|e| e.to_string())
}

fn resolve_exchange_graph_import(args: ExchangeGraphImportArgs) -> Result<Action, Error> {
    let common = common_config(&args.global, args.archive.clone());
    let mailbox_kind = args.mailbox_kind;
    let event_body_format = args.event_body_format;
    let auth = resolve_graph_auth(
        args.access_token.as_ref(),
        args.client_id.as_deref(),
        &args.tenant,
    )?;
    let surfaces = args
        .objects
        .as_deref()
        .map(Surfaces::parse_list)
        .transpose()?
        .unwrap_or_default();
    let top = args.top.clamp(1, 1000);
    let graph_connections = args
        .graph_connections
        .clamp(1, 16)
        .min(common.threads.max(1));
    Ok(Action::ImportExchangeGraph(
        common,
        GraphImportConfig {
            auth,
            api_base: args.api_base,
            user_target: args.user,
            mailbox_kind,
            surfaces,
            event_body_format,
            graph_connections,
            top,
            exception_window_years: args.exception_window_years,
            allow_source_change: args.allow_source_change,
        },
    ))
}

fn resolve_graph_auth(
    access_token: Option<&Option<String>>,
    client_id: Option<&str>,
    tenant: &str,
) -> Result<GraphAuth, Error> {
    if let Some(slot) = access_token {
        let token = secret::resolve(slot.as_deref(), "VANDELAY_GRAPH_TOKEN", "bearer token")?;
        return Ok(GraphAuth::PreAcquired { token });
    }
    let client_id = client_id
        .ok_or_else(|| {
            Error::Usage(
                "--client-id is required for exchange-graph (unless --access-token is given)"
                    .to_owned(),
            )
        })?
        .to_owned();
    let authority = crate::exchange_graph::oauth::default_authority(tenant);
    Ok(GraphAuth::DeviceCode {
        authority,
        client_id,
    })
}

fn common_config(global: &GlobalArgs, archive: PathBuf) -> CommonConfig {
    let threads = global
        .threads
        .filter(|n| *n > 0)
        .unwrap_or_else(num_cpus::get)
        .max(1);
    CommonConfig {
        archive,
        threads,
        dry_run: global.dry_run,
        max_retries: global.max_retries,
        allow_invalid_certs: global.allow_invalid_certs,
        logger: Logger::from_flags(global.quiet, global.verbose),
    }
}

fn resolve_auth(
    auth_basic: Option<&str>,
    auth_password: Option<&str>,
    auth_bearer: Option<&Option<String>>,
) -> Result<Auth, Error> {
    if let Some(user) = auth_basic {
        let password = secret::resolve(auth_password, "VANDELAY_PASSWORD", "password")?;
        return Ok(Auth::Basic {
            user: user.to_owned(),
            password,
        });
    }
    if auth_password.is_some() {
        return Err(Error::Usage(
            "--auth-password is only valid together with --auth-basic".to_owned(),
        ));
    }
    let bearer = auth_bearer.ok_or_else(|| {
        Error::Usage("exactly one of --auth-basic / --auth-bearer is required".to_owned())
    })?;
    let token = secret::resolve(bearer.as_deref(), "VANDELAY_TOKEN", "bearer token")?;
    Ok(Auth::Bearer { token })
}

fn resolve_account(
    account_id: Option<String>,
    account_name: Option<String>,
) -> Result<AccountSelector, Error> {
    match (account_id, account_name) {
        (Some(id), None) => Ok(AccountSelector::Id(id)),
        (None, Some(name)) => Ok(AccountSelector::Name(name)),
        _ => Err(Error::Usage(
            "exactly one of --account-id / --account-name is required".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        static M: std::sync::Mutex<()> = std::sync::Mutex::new(());
        M.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn auth_password_without_basic_is_usage_error() {
        let _g = lock();
        unsafe {
            env::remove_var("VANDELAY_TOKEN");
            env::remove_var("VANDELAY_PASSWORD");
        }
        let bearer: Option<String> = Some("t".to_owned());
        let r = resolve_auth(None, Some("hunter2"), Some(&bearer));
        match r {
            Err(Error::Usage(m)) => {
                assert!(m.contains("auth-password"), "msg was: {m}");
            }
            other => panic!("expected Usage error, got {other:?}"),
        }
    }

    #[test]
    fn auth_basic_with_password_resolves() {
        let _g = lock();
        unsafe {
            env::remove_var("VANDELAY_TOKEN");
            env::remove_var("VANDELAY_PASSWORD");
        }
        let auth = resolve_auth(Some("alice"), Some("pw"), None).unwrap();
        match auth {
            Auth::Basic { user, password } => {
                assert_eq!(user, "alice");
                assert_eq!(password, "pw");
            }
            _ => panic!("expected Basic"),
        }
    }

    #[test]
    fn auth_bearer_inline_token_resolves() {
        let _g = lock();
        unsafe {
            env::remove_var("VANDELAY_TOKEN");
            env::remove_var("VANDELAY_PASSWORD");
        }
        let bearer: Option<String> = Some("t".to_owned());
        let auth = resolve_auth(None, None, Some(&bearer)).unwrap();
        assert!(matches!(auth, Auth::Bearer { token } if token == "t"));
    }

    fn graph_config(objects: &[&str]) -> Result<GraphImportConfig, Error> {
        let mut argv = vec![
            "vandelay",
            "import",
            "exchange-graph",
            "--access-token",
            "tok",
        ];
        argv.extend_from_slice(objects);
        argv.push("archive.sqlite");
        let cli = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("clap rejected args: {e}"));
        match cli.resolve()? {
            Action::ImportExchangeGraph(_, config) => Ok(config),
            _ => panic!("expected ImportExchangeGraph"),
        }
    }

    #[test]
    fn graph_objects_contacts_selects_only_contacts() {
        let _g = lock();
        let config = graph_config(&["--objects", "contacts"]).unwrap();
        assert_eq!(
            config.surfaces,
            Surfaces {
                mail: false,
                calendar: false,
                contacts: true,
                files: false
            }
        );
    }

    #[test]
    fn graph_objects_absent_selects_every_surface() {
        let _g = lock();
        let config = graph_config(&[]).unwrap();
        assert_eq!(config.surfaces, Surfaces::ALL);
    }

    #[test]
    fn graph_objects_rejects_jmap_type_names() {
        let _g = lock();
        match graph_config(&["--objects", "contactcard"]) {
            Err(Error::Usage(m)) => {
                assert!(m.contains("unknown surface: contactcard"), "msg was: {m}");
                assert!(
                    m.contains("valid: mail, calendar, contacts"),
                    "msg was: {m}"
                );
            }
            other => panic!("expected Usage error, got {:?}", other.map(|_| ())),
        }
    }
}
