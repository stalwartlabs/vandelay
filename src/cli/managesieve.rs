/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::path::PathBuf;

use clap::Args;

use super::Action;
use super::global::GlobalArgs;
use super::imap::SaslAuthFlags;
use crate::archive::sources::SourceChange;
use crate::error::Error;
use crate::import::managesieve::ManageSieveImport;
use crate::net::stream::Cleartext;

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

impl ManageSieveImportArgs {
    pub(super) fn resolve(self) -> Result<Action, Error> {
        let common = self.global.common_config(self.archive);
        let auth = SaslAuthFlags {
            basic: self.auth_basic.as_deref(),
            password: self.auth_password.as_deref(),
            bearer: self.auth_bearer.as_ref().map(Option::as_deref),
            user: self.auth_user.as_deref(),
        }
        .resolve()?;
        Ok(Action::import(
            common,
            ManageSieveImport {
                url: self.url,
                auth,
                cleartext: if self.allow_cleartext {
                    Cleartext::Allow
                } else {
                    Cleartext::Forbid
                },
                source_change: if self.allow_source_change {
                    SourceChange::Allow
                } else {
                    SourceChange::Reject
                },
            },
        ))
    }
}
