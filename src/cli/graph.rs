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
use crate::exchange::graph::types::{EventBodyFormat, MailboxKind as GraphMailboxKind, Surfaces};
use crate::exchange::oauth::Authority;
use crate::import::graph::coordinator::{MAX_GRAPH_CONNECTIONS, MAX_PAGE_SIZE};
use crate::import::graph::{ContactPhotos, EventAttachments, GraphAuth, GraphImport};
use crate::secret::Secret;

#[derive(Args)]
pub struct ExchangeGraphImportArgs {
    #[arg(
        long,
        value_name = "UUID",
        help = "Entra app (client) id (required unless --access-token is given)"
    )]
    client_id: Option<String>,

    #[arg(
        long,
        value_name = "ID",
        default_value = "common",
        help = "Entra authority tenant"
    )]
    tenant: String,

    #[arg(
        long,
        value_name = "UPN|UUID",
        help = "Target a different mailbox the signed-in account can read"
    )]
    user: Option<String>,

    #[arg(
        long,
        value_name = "KIND",
        default_value = "primary",
        value_parser = GraphMailboxKind::parse,
        help = "Mailbox surface: primary | archive (public-folders is rejected: use exchange-ews)"
    )]
    mailbox_kind: GraphMailboxKind,

    #[arg(
        long,
        value_name = "TOKEN",
        num_args = 0..=1,
        help = "Pre-acquired bearer token (skip device-code flow). Resolves: flag value -> $VANDELAY_GRAPH_TOKEN -> prompt."
    )]
    access_token: Option<Option<String>>,

    #[arg(
        long,
        value_name = "LIST",
        help = "Comma-separated surface list: mail | calendar | contacts | files (default: all)"
    )]
    objects: Option<String>,

    #[arg(
        long,
        value_name = "FMT",
        default_value = "text",
        value_parser = EventBodyFormat::parse,
        help = "outlook.body-content-type for event GETs: text | html"
    )]
    event_body_format: EventBodyFormat,

    #[arg(
        long,
        value_name = "N",
        default_value_t = 4,
        help = "Parallel Graph worker connections (hard cap 16)"
    )]
    graph_connections: usize,

    #[arg(
        long,
        value_name = "N",
        default_value_t = 100,
        help = "Per-page size ($top query parameter, range [1, 1000])"
    )]
    top: usize,

    #[arg(
        long,
        value_name = "N",
        default_value_t = 5,
        help = "Years either side of today to scan for edited recurrence occurrences (max 5)",
        long_help = "Years either side of today to scan for edited recurrence occurrences.\n  \
            Graph reports a series exception only inside an expanded calendar view, and caps\n  \
            any single view at five years, so vandelay scans [today - N, today] and\n  \
            [today, today + N]. An occurrence edited outside that span imports with the\n  \
            series default instead of its edit."
    )]
    exception_window_years: i32,

    #[arg(
        long,
        help = "Skip contact photos (saves one request per contact on large address books)",
        long_help = "Skip contact photos.\n              Graph will not say which contacts have a photo: $expand=photo comes back empty\n              even when one exists, so the only way to find out is to ask for the bytes.\n              vandelay therefore spends one extra request per contact. Pass this to skip\n              photos entirely on a large address book."
    )]
    skip_contact_photos: bool,

    #[arg(
        long,
        help = "Skip event file attachments (they are stored as JSCalendar enclosure links)"
    )]
    skip_event_attachments: bool,

    #[arg(
        long,
        value_name = "URL",
        default_value = "https://graph.microsoft.com/v1.0",
        help = "Microsoft Graph API base URL (override for national clouds or tests)",
        hide = true
    )]
    api_base: String,

    #[arg(
        long,
        help = "Permit importing when the archive records a different exchange_graph source"
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

impl ExchangeGraphImportArgs {
    pub(super) fn resolve(self) -> Result<Action, Error> {
        let common = self.global.common_config(self.archive.clone());
        let mailbox_kind = self.mailbox_kind;
        let event_body_format = self.event_body_format;
        let auth = self.auth()?;
        let surfaces = self
            .objects
            .as_deref()
            .map(Surfaces::parse_list)
            .transpose()?
            .unwrap_or_default();
        let top = self.top.clamp(1, MAX_PAGE_SIZE);
        let graph_connections = self
            .graph_connections
            .clamp(1, MAX_GRAPH_CONNECTIONS)
            .min(common.threads.max(1));
        Ok(Action::import(
            common,
            GraphImport {
                auth,
                api_base: self.api_base,
                user_target: self.user,
                mailbox_kind,
                surfaces,
                event_body_format,
                graph_connections,
                top,
                exception_window_years: self.exception_window_years,
                contact_photos: if self.skip_contact_photos {
                    ContactPhotos::Skip
                } else {
                    ContactPhotos::Fetch
                },
                event_attachments: if self.skip_event_attachments {
                    EventAttachments::Skip
                } else {
                    EventAttachments::Fetch
                },
                source_change: if self.allow_source_change {
                    SourceChange::Allow
                } else {
                    SourceChange::Reject
                },
            },
        ))
    }

    fn auth(&self) -> Result<GraphAuth, Error> {
        if let Some(slot) = &self.access_token {
            let token = Secret::resolve(slot.as_deref(), "VANDELAY_GRAPH_TOKEN", "bearer token")?
                .into_inner();
            return Ok(GraphAuth::PreAcquired { token });
        }
        let client_id = self.client_id.clone().ok_or_else(|| {
            Error::Usage(
                "--client-id is required for exchange-graph (unless --access-token is given)"
                    .to_owned(),
            )
        })?;
        Ok(GraphAuth::DeviceCode {
            authority: Authority::for_tenant(&self.tenant).into_string(),
            client_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::cli::{Cli, env_lock as lock};
    use crate::import::Import;

    fn graph_config(objects: &[&str]) -> Result<GraphImport, Error> {
        let mut argv = vec![
            "vandelay",
            "import",
            "exchange-graph",
            "--access-token",
            "tok",
        ];
        argv.extend_from_slice(objects);
        argv.push("archive.sqlite");
        let cli = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("clap rejected args: {e}"));
        match cli.resolve()? {
            Action::Import(_, Import::Graph(config)) => Ok(config),
            _ => panic!("expected ImportExchangeGraph"),
        }
    }

    #[test]
    fn graph_objects_contacts_selects_only_contacts() {
        let _g = lock();
        let config = graph_config(&["--objects", "contacts"]).unwrap();
        assert_eq!(
            config.surfaces,
            Surfaces {
                mail: false,
                calendar: false,
                contacts: true,
                files: false
            }
        );
    }

    #[test]
    fn graph_objects_absent_selects_every_surface() {
        let _g = lock();
        let config = graph_config(&[]).unwrap();
        assert_eq!(config.surfaces, Surfaces::ALL);
    }

    #[test]
    fn graph_objects_rejects_jmap_type_names() {
        let _g = lock();
        match graph_config(&["--objects", "contactcard"]) {
            Err(Error::Usage(m)) => {
                assert!(m.contains("unknown surface: contactcard"), "msg was: {m}");
                assert!(
                    m.contains("valid: mail, calendar, contacts"),
                    "msg was: {m}"
                );
            }
            other => panic!("expected Usage error, got {:?}", other.map(|_| ())),
        }
    }
}
