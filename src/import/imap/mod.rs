/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod control;
mod dry_run;
pub mod fetch;
pub mod folders;
pub mod internaldate;
pub mod keywords;
pub mod messages;
pub mod pool;
mod reconcile;
mod run;

use regex::Regex;

use crate::archive::sources::SourceChange;
use crate::imap::automap::Automap;
use crate::imap::session::Compress;
use crate::net::stream::Cleartext;

pub use crate::net::sasl::SaslAuth;
pub use folders::FolderScope;
pub use keywords::DeletedMessages;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MimeKeywords {
    Skip,
    Extract,
}

pub struct ImapImport {
    pub url: String,
    pub auth: SaslAuth,
    pub cleartext: Cleartext,
    pub compress: Compress,
    pub include: Vec<Regex>,
    pub exclude: Vec<Regex>,
    pub exclude_special: Vec<String>,
    pub folder: Vec<String>,
    pub scope: FolderScope,
    pub automap: Automap,
    pub deleted: DeletedMessages,
    pub mime_keywords: MimeKeywords,
    pub fetch_batch: usize,
    pub imap_connections: usize,
    pub source_change: SourceChange,
}
