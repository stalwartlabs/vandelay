/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod calendar;
mod contacts;
mod labels;
mod mail;
mod mbox;
mod run;
mod walk;

use std::path::PathBuf;

use crate::archive::sources::SourceChange;
use crate::imap::automap::Automap;

#[derive(Debug)]
pub struct TakeoutImport {
    pub takeout_root: PathBuf,
    pub source_change: SourceChange,
    pub automap: Automap,
}
