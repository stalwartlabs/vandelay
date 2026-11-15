/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::path::PathBuf;

use clap::{ArgGroup, Args};

use super::Action;
use super::auth::{AccountFlags, AuthArgs};
use super::global::GlobalArgs;
use crate::archive::sources::SourceChange;
use crate::error::Error;
use crate::import::jmap::JmapImport;
use crate::jmap::connect::ConnectConfig;
use crate::types::ObjectTypes;

#[derive(Args)]
#[command(group(ArgGroup::new("account").required(true).args(["account_id", "account_name"])))]
pub(super) struct JmapImportArgs {
    #[arg(long, value_name = "URL", help = "Base URL or full JMAP session URL")]
    url: String,

    #[command(flatten)]
    auth: AuthArgs,

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

impl JmapImportArgs {
    pub(super) fn resolve(self) -> Result<Action, Error> {
        let common = self.global.common_config(self.archive);
        let auth = self.auth.resolve()?;
        let account = AccountFlags {
            id: self.account_id,
            name: self.account_name,
        }
        .resolve()?;
        let objects = self
            .objects
            .as_deref()
            .map(str::parse::<ObjectTypes>)
            .transpose()?;

        Ok(Action::import(
            common,
            JmapImport {
                connect: ConnectConfig {
                    url: self.url,
                    auth,
                    account,
                },
                objects,
                source_change: if self.allow_source_change {
                    SourceChange::Allow
                } else {
                    SourceChange::Reject
                },
            },
        ))
    }
}
