/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::path::PathBuf;

use clap::Args;

use super::Action;
use super::global::GlobalArgs;
use crate::archive::sources::SourceChange;
use crate::error::Error;
use crate::imap::automap::Automap;
use crate::import::takeout::TakeoutImport;

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

impl TakeoutImportArgs {
    pub(super) fn resolve(self) -> Result<Action, Error> {
        let common = self.global.common_config(self.archive);
        Ok(Action::import(
            common,
            TakeoutImport {
                takeout_root: self.path,
                source_change: if self.allow_source_change {
                    SourceChange::Allow
                } else {
                    SourceChange::Reject
                },
                automap: if self.noautomap {
                    Automap::Disabled
                } else {
                    Automap::Enabled
                },
            },
        ))
    }
}
