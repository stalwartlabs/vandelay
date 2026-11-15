/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::path::PathBuf;

use clap::{ArgAction, Args};

use super::global::GlobalArgs;
use super::{Action, FolderFilter};
use crate::archive::sources::SourceChange;
use crate::error::Error;
use crate::imap::automap::Automap;
use crate::imap::session::Compress;
use crate::import::imap::pool::MAX_IMAP_CONNECTIONS;
use crate::import::imap::{DeletedMessages, FolderScope, ImapImport, MimeKeywords};
use crate::net::sasl::SaslAuth;
use crate::net::stream::Cleartext;
use crate::secret::Secret;

#[derive(Args)]
#[command(group(clap::ArgGroup::new("auth").required(true).args(["auth_basic", "auth_bearer"])))]
pub(super) struct ImapImportArgs {
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
        help = "Add the phrases of each message's Keywords: header (Exchange categories) as JMAP keywords"
    )]
    mime_keywords: bool,

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

impl ImapImportArgs {
    pub(super) fn resolve(self) -> Result<Action, Error> {
        let common = self.global.common_config(self.archive);
        let auth = SaslAuthFlags {
            basic: self.auth_basic.as_deref(),
            password: self.auth_password.as_deref(),
            bearer: self.auth_bearer.as_ref().map(Option::as_deref),
            user: self.auth_user.as_deref(),
        }
        .resolve()?;
        if !self.folder.is_empty() && (!self.include.is_empty() || !self.exclude.is_empty()) {
            return Err(Error::Usage(
                "--folder is mutually exclusive with --include/--exclude".to_owned(),
            ));
        }
        let include = FolderFilter::Include.compile(&self.include)?;
        let exclude = FolderFilter::Exclude.compile(&self.exclude)?;
        let imap_connections = self
            .imap_connections
            .unwrap_or(common.threads)
            .clamp(1, MAX_IMAP_CONNECTIONS);

        Ok(Action::import(
            common,
            ImapImport {
                url: self.url,
                auth,
                cleartext: if self.allow_cleartext {
                    Cleartext::Allow
                } else {
                    Cleartext::Forbid
                },
                compress: if self.compress {
                    Compress::Requested
                } else {
                    Compress::Off
                },
                include,
                exclude,
                exclude_special: self.exclude_special,
                folder: self.folder,
                scope: if self.subscribed_only {
                    FolderScope::Subscribed
                } else {
                    FolderScope::All
                },
                automap: if self.noautomap {
                    Automap::Disabled
                } else {
                    Automap::Enabled
                },
                deleted: if self.include_deleted {
                    DeletedMessages::Keep
                } else {
                    DeletedMessages::Skip
                },
                mime_keywords: if self.mime_keywords {
                    MimeKeywords::Extract
                } else {
                    MimeKeywords::Skip
                },
                fetch_batch: self.fetch_batch,
                imap_connections,
                source_change: if self.allow_source_change {
                    SourceChange::Allow
                } else {
                    SourceChange::Reject
                },
            },
        ))
    }
}

#[derive(Clone, Copy)]
pub(super) struct SaslAuthFlags<'a> {
    pub(super) basic: Option<&'a str>,
    pub(super) password: Option<&'a str>,
    pub(super) bearer: Option<Option<&'a str>>,
    pub(super) user: Option<&'a str>,
}

impl SaslAuthFlags<'_> {
    pub(super) fn resolve(self) -> Result<SaslAuth, Error> {
        if let Some(user) = self.basic {
            if self.user.is_some() {
                return Err(Error::Usage(
                    "--auth-user is only valid with --auth-bearer".to_owned(),
                ));
            }
            let password =
                Secret::resolve(self.password, "VANDELAY_PASSWORD", "password")?.into_inner();
            return Ok(SaslAuth::Basic {
                user: user.to_owned(),
                password,
            });
        }
        if self.password.is_some() {
            return Err(Error::Usage(
                "--auth-password is only valid together with --auth-basic".to_owned(),
            ));
        }
        let bearer = self.bearer.ok_or_else(|| {
            Error::Usage("exactly one of --auth-basic / --auth-bearer is required".to_owned())
        })?;
        let user = self
            .user
            .ok_or_else(|| Error::Usage("--auth-user is required with --auth-bearer".to_owned()))?
            .to_owned();
        let token = Secret::resolve(bearer, "VANDELAY_TOKEN", "bearer token")?.into_inner();
        Ok(SaslAuth::Bearer { user, token })
    }
}
