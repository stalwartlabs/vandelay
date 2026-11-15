/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::sync::Arc;

use crate::error::Error;
use crate::imap::client::{Capability, CollectedResponse, ImapClient};
use crate::imap::command::Command;
use crate::imap::error::ImapError;
use crate::imap::name::NameEncoding;
use crate::imap::response::{NamespaceEntry, Untagged};
use crate::imap::session::ImapSession;
use crate::logging::Logger;
use crate::net::backoff::{BackoffState, Fault, RetryError};
use crate::net::http::RetryStats;

use super::folders::ResolvedFolder;
use super::messages::UidSearch;

pub(super) struct Control {
    client: ImapClient,
    session: Arc<ImapSession>,
    max_retries: u32,
    backoff: BackoffState,
    logger: Logger,
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Selected {
    pub uidvalidity: u32,
    pub uidnext: u32,
}

#[derive(Clone, Copy)]
enum SelectCode {
    UidValidity,
    UidNext,
}

impl Selected {
    fn from_response(resp: &CollectedResponse) -> Selected {
        let mut out = Selected::default();
        for u in &resp.untagged {
            let Untagged::StatusLine(line) = u else {
                continue;
            };
            let (Some(code), Some(args)) = (&line.code, &line.code_args) else {
                continue;
            };
            let slot = match hashify::map!(code.as_bytes(), SelectCode,
                "UIDVALIDITY" => SelectCode::UidValidity,
                "UIDNEXT" => SelectCode::UidNext,
            ) {
                Some(SelectCode::UidValidity) => &mut out.uidvalidity,
                Some(SelectCode::UidNext) => &mut out.uidnext,
                None => continue,
            };
            if let Ok(n) = args.parse() {
                *slot = n;
            }
        }
        out
    }
}

impl Control {
    pub fn new(
        client: ImapClient,
        session: Arc<ImapSession>,
        max_retries: u32,
        backoff: BackoffState,
    ) -> Control {
        let logger = session.logger();
        Control {
            client,
            session,
            max_retries,
            backoff,
            logger,
        }
    }

    pub fn client(&self) -> &ImapClient {
        &self.client
    }

    pub fn name_encoding(&self) -> NameEncoding {
        self.client.name_encoding()
    }

    pub fn stats(&self) -> RetryStats {
        self.backoff.stats()
    }

    pub fn run_collect(&mut self, command: &Command<'_>) -> Result<CollectedResponse, Error> {
        let session = &self.session;
        let logger = self.logger;
        self.backoff
            .run(
                self.max_retries,
                &mut self.client,
                |client| client.run_collect(command),
                |client, _| {
                    *client = session.reopen()?;
                    logger.info(format_args!("Reconnected the IMAP control connection"));
                    Ok(())
                },
                |_| {},
            )
            .map_err(|e| match e {
                RetryError::Exhausted {
                    error,
                    fault: Fault::TransportDrop,
                } => Error::Connection(format!("control connection lost: {error}")),
                RetryError::Exhausted { error, .. } => Error::Partial(format!(
                    "{:?}: {error} (max retries exhausted)",
                    command.to_string()
                )),
                RetryError::Fatal(error) => {
                    Error::Partial(format!("{:?}: {error}", command.to_string()))
                }
                RetryError::Reconnect(error) => {
                    Error::Connection(format!("control reconnect failed: {error}"))
                }
            })
    }

    pub fn select(&mut self, folder: &ResolvedFolder) -> Result<Selected, Error> {
        self.run_collect(&Command::Select(&folder.wire_name))
            .map(|resp| Selected::from_response(&resp))
    }

    pub fn noop(&mut self) {
        let _ = self.run_collect(&Command::Noop);
    }

    pub fn uids(&mut self) -> Result<Vec<u32>, ImapError> {
        let session = &self.session;
        self.backoff
            .run(
                self.max_retries,
                &mut self.client,
                UidSearch::all,
                |client, _| {
                    *client = session.reopen()?;
                    Ok(())
                },
                |_| {},
            )
            .map_err(RetryError::into_inner)
    }

    pub fn namespace_prefix(&mut self) -> String {
        if !self.client.has(Capability::Namespace) {
            return String::new();
        }
        match self.client.run_collect(&Command::Namespace) {
            Ok(resp) => resp
                .untagged
                .into_iter()
                .find_map(|u| match u {
                    Untagged::Namespace { personal, .. } => personal
                        .into_iter()
                        .next()
                        .map(|NamespaceEntry { prefix, .. }| prefix),
                    _ => None,
                })
                .unwrap_or_default(),
            Err(e) => {
                self.logger
                    .progress(format_args!("NAMESPACE failed ({e}); using no prefix"));
                String::new()
            }
        }
    }

    pub fn logout(&mut self) {
        let _ = self.client.logout();
    }
}
