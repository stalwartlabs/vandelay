/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::path::PathBuf;

use clap::Args;

use super::Action;
use crate::console::Console;
use crate::error::Error;
use crate::inspect::{Inspect, Page};
use crate::types::ObjectType;

#[derive(Args)]
pub(super) struct InspectArgs {
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

impl InspectArgs {
    pub(super) fn resolve(self) -> Result<Action, Error> {
        let target = match self.target.as_deref() {
            Some(token) => Some(token.parse::<ObjectType>()?),
            None => None,
        };
        if let Some(limit) = self.limit
            && limit == 0
        {
            return Err(Error::Usage("--limit must be greater than zero".to_owned()));
        }
        Ok(Action::Inspect(Inspect {
            archive: self.archive,
            target,
            page: Page {
                limit: self.limit,
                offset: self.offset,
            },
            console: Console::default(),
        }))
    }
}
