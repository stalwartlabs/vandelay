/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::error::Error;
use crate::managesieve::SieveClient;
use crate::managesieve::error::SieveError;
use crate::managesieve::response::ListedScript;
use crate::managesieve::session::SieveSession;
use crate::net::backoff::{BackoffState, Fault, RetryError};
use crate::net::http::RetryStats;

pub(super) struct Control {
    client: SieveClient,
    session: SieveSession,
    backoff: BackoffState,
    max_retries: u32,
}

impl Control {
    pub fn new(client: SieveClient, session: SieveSession, max_retries: u32) -> Control {
        Control {
            client,
            session,
            backoff: BackoffState::new(),
            max_retries,
        }
    }

    pub fn stats(&self) -> RetryStats {
        self.backoff.stats()
    }

    pub fn logout(&mut self) {
        self.client.logout();
    }

    pub fn listscripts(&mut self) -> Result<Vec<ListedScript>, Error> {
        let session = &self.session;
        let logger = session.logger();
        let block = self
            .backoff
            .run(
                self.max_retries,
                &mut self.client,
                SieveClient::listscripts,
                |client, e| {
                    *client = session.reopen()?;
                    logger.info(format_args!(
                        "Reconnected to ManageSieve after the connection dropped ({e})"
                    ));
                    Ok(())
                },
                |e| logger.progress(format_args!("LISTSCRIPTS failed ({e}); retrying")),
            )
            .map_err(|e| match e {
                RetryError::Exhausted {
                    error,
                    fault: Fault::TransportDrop,
                } => Error::Connection(format!(
                    "LISTSCRIPTS transport drop, retries exhausted: {error}"
                )),
                RetryError::Exhausted { error, .. } => {
                    Error::Connection(format!("LISTSCRIPTS retries exhausted: {error}"))
                }
                RetryError::Reconnect(error) => {
                    Error::Connection(format!("LISTSCRIPTS reconnect failed: {error}"))
                }
                RetryError::Fatal(error) if error.is_referral() => Error::Connection(format!(
                    "LISTSCRIPTS referral: {error}; vandelay does not follow referrals"
                )),
                RetryError::Fatal(error) => {
                    Error::Connection(format!("LISTSCRIPTS failed: {error}"))
                }
            })?;
        ListedScript::parse_all(&block.data)
            .map_err(|e| Error::Connection(format!("LISTSCRIPTS parse: {e}")))
    }

    pub fn getscript(&mut self, name: &str) -> Result<Vec<u8>, SieveError> {
        let session = &self.session;
        let logger = session.logger();
        self.backoff
            .run(
                self.max_retries,
                &mut self.client,
                |client| client.getscript(name),
                |client, _| {
                    *client = session.reopen()?;
                    logger.info(format_args!(
                        "Reconnected to ManageSieve after the connection dropped during GETSCRIPT \"{name}\""
                    ));
                    Ok(())
                },
                |e| logger.progress(format_args!("GETSCRIPT \"{name}\" failed ({e}); retrying")),
            )
            .map_err(RetryError::into_inner)
    }
}
