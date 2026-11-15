/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod keywords;
mod messages;
mod run;
mod tree;

use std::path::PathBuf;

use regex::Regex;

use crate::archive::sources::SourceChange;
use crate::imap::automap::Automap;
use crate::import::imap::DeletedMessages;

pub struct MaildirImport {
    pub maildir: PathBuf,
    pub include: Vec<Regex>,
    pub exclude: Vec<Regex>,
    pub folder: Vec<String>,
    pub automap: Automap,
    pub deleted: DeletedMessages,
    pub source_change: SourceChange,
}
