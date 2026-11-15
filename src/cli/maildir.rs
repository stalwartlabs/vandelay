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
use crate::import::imap::DeletedMessages;
use crate::import::maildir::MaildirImport;

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

impl MaildirImportArgs {
    pub(super) fn resolve(self) -> Result<Action, Error> {
        let common = self.global.common_config(self.archive);
        if !self.folder.is_empty() && (!self.include.is_empty() || !self.exclude.is_empty()) {
            return Err(Error::Usage(
                "--folder is mutually exclusive with --include/--exclude".to_owned(),
            ));
        }
        let include = FolderFilter::Include.compile(&self.include)?;
        let exclude = FolderFilter::Exclude.compile(&self.exclude)?;
        Ok(Action::import(
            common,
            MaildirImport {
                maildir: self.maildir,
                include,
                exclude,
                folder: self.folder,
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
                source_change: if self.allow_source_change {
                    SourceChange::Allow
                } else {
                    SourceChange::Reject
                },
            },
        ))
    }
}
