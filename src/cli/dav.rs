/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::path::PathBuf;

use clap::Args;

use super::Action;
use super::auth::AuthFlags;
use super::global::GlobalArgs;
use crate::archive::sources::SourceChange;
use crate::error::Error;
use crate::import::dav::{DavImport, DavKindArg};

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

    #[arg(
        long,
        conflicts_with = "allow_source_change",
        help = "Remove other recorded sources of this kind, and their items, before importing"
    )]
    replace_source: bool,

    #[command(flatten)]
    global: GlobalArgs,

    #[arg(
        value_name = "ARCHIVE",
        help = "Local SQLite archive (created if absent)"
    )]
    archive: PathBuf,
}

impl DavImportArgs {
    pub(super) fn resolve(self, kind: DavKindArg) -> Result<Action, Error> {
        let common = self.global.common_config(self.archive);
        let auth = AuthFlags {
            basic: self.auth_basic.as_deref(),
            password: self.auth_password.as_deref(),
            bearer: self.auth_bearer.as_ref().map(Option::as_deref),
        }
        .resolve()?;
        let dav_connections = self.dav_connections.clamp(1, 8).min(common.threads.max(1));
        let multiget_batch = self.multiget_batch.max(1);
        Ok(Action::import(
            common,
            DavImport {
                kind,
                url: self.url,
                auth,
                allow_cleartext: self.allow_cleartext,
                dav_connections,
                multiget_batch,
                source_change: match (self.replace_source, self.allow_source_change) {
                    (true, _) => SourceChange::Replace,
                    (false, true) => SourceChange::Allow,
                    (false, false) => SourceChange::Reject,
                },
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::cli::{Cli, env_lock as lock};
    use crate::import::Import;

    fn webdav_source_change(extra: &[&str]) -> Result<SourceChange, clap::Error> {
        let mut argv = vec![
            "vandelay",
            "import",
            "webdav",
            "--url",
            "https://dav.example/files/u/",
            "--auth-bearer",
            "tok",
        ];
        argv.extend_from_slice(extra);
        argv.push("archive.sqlite");
        let cli = Cli::try_parse_from(argv)?;
        match cli.resolve() {
            Ok(Action::Import(_, Import::Dav(config))) => Ok(config.source_change),
            _ => panic!("expected ImportDav"),
        }
    }

    // --replace-source maps to Replace and cannot be combined with --allow-source-change.
    #[test]
    fn dav_source_change_flags_resolve() {
        let _g = lock();
        assert_eq!(webdav_source_change(&[]).unwrap(), SourceChange::Reject);
        assert_eq!(
            webdav_source_change(&["--allow-source-change"]).unwrap(),
            SourceChange::Allow
        );
        assert_eq!(
            webdav_source_change(&["--replace-source"]).unwrap(),
            SourceChange::Replace
        );
        assert!(webdav_source_change(&["--replace-source", "--allow-source-change"]).is_err());
    }
}
