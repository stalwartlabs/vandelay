/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use super::client::{Capability, ImapClient};
use super::error::ImapError;
use super::name::NameEncoding;
use crate::error::Error;
use crate::logging::Logger;
use crate::net::sasl::{SaslAuth, SaslPayload};
use crate::net::stream::{Cleartext, ConnectMode, Connector, Endpoint, Scheme, UrlTls};

pub const IMAP: Scheme = Scheme {
    plain: "imap",
    tls: "imaps",
    plain_port: 143,
    tls_port: 993,
};

const UTF8_ACCEPT: &[&str] = &["UTF8=ACCEPT"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compress {
    Requested,
    Off,
}

pub struct ImapSession {
    connector: Connector,
    endpoint: Endpoint,
    mode: ConnectMode,
    auth: SaslAuth,
    compress: Compress,
    logger: Logger,
}

impl ImapSession {
    pub fn new(
        connector: Connector,
        endpoint: Endpoint,
        cleartext: Cleartext,
        auth: SaslAuth,
        compress: Compress,
        logger: Logger,
    ) -> ImapSession {
        let mode = match (endpoint.tls, cleartext) {
            (UrlTls::Implicit, _) => ConnectMode::ImplicitTls,
            (UrlTls::Negotiated, Cleartext::Allow) => ConnectMode::Plain,
            (UrlTls::Negotiated, Cleartext::Forbid) => ConnectMode::StartTls,
        };
        ImapSession {
            connector,
            endpoint,
            mode,
            auth,
            compress,
            logger,
        }
    }

    pub fn session_url(&self) -> String {
        self.endpoint.session_url(&IMAP)
    }

    pub fn logger(&self) -> Logger {
        self.logger
    }

    fn connect(&self) -> Result<ImapClient, ImapError> {
        ImapClient::connect(
            &self.connector,
            &self.endpoint.host,
            self.endpoint.port,
            self.mode,
            self.logger,
        )
    }

    pub fn open(&self) -> Result<(ImapClient, String), Error> {
        let logger = self.logger;
        let mut client = self.connect().map_err(Error::connection)?;
        let account_id = self.authenticate(&mut client)?;
        logger.progress(format_args!(
            "Authenticated as {account_id} on {}",
            self.session_url()
        ));
        client
            .refresh_capabilities()
            .map_err(|e| Error::Connection(format!("post-auth CAPABILITY: {e}")))?;
        if self.compress == Compress::Requested {
            if client.has(Capability::CompressDeflate) {
                client
                    .compress_deflate()
                    .map_err(|e| Error::Connection(format!("COMPRESS DEFLATE: {e}")))?;
                logger.progress(format_args!("COMPRESS DEFLATE enabled"));
            } else {
                logger.progress(format_args!(
                    "The server does not advertise COMPRESS=DEFLATE; --compress is ignored"
                ));
            }
        }
        if client.has(Capability::Enable) && client.has(Capability::Utf8Accept) {
            match client.enable(UTF8_ACCEPT) {
                Ok(_) => logger.progress(format_args!("UTF8=ACCEPT enabled")),
                Err(e) => logger.progress(format_args!(
                    "ENABLE UTF8=ACCEPT failed ({e}); using modified UTF-7"
                )),
            }
        }
        Ok((client, account_id))
    }

    pub fn reopen(&self) -> Result<ImapClient, ImapError> {
        let mut client = self.connect()?;
        self.authenticate(&mut client)
            .map_err(|e| ImapError::AuthFailed(e.to_string()))?;
        let _ = client.refresh_capabilities();
        if self.compress == Compress::Requested && client.has(Capability::CompressDeflate) {
            client.compress_deflate()?;
        }
        if client.has(Capability::Enable) && client.has(Capability::Utf8Accept) {
            let _ = client.enable(UTF8_ACCEPT);
        }
        Ok(client)
    }

    fn authenticate(&self, client: &mut ImapClient) -> Result<String, Error> {
        match &self.auth {
            SaslAuth::Basic { user, password } => {
                if client.has(Capability::AuthPlain) {
                    match client.authenticate(SaslPayload::Plain {
                        authcid: user,
                        password,
                    }) {
                        Ok(()) => return Ok(user.clone()),
                        Err(e) if e.is_auth_failed() => {
                            return Err(Error::Connection(format!("auth failed: {e}")));
                        }
                        Err(e) if e.is_negotiation_failure() => {}
                        Err(e) => return Err(Error::Connection(e.to_string())),
                    }
                }
                match client.login(user, password) {
                    Ok(()) => Ok(user.clone()),
                    Err(e) if e.is_auth_failed() => {
                        Err(Error::Connection(format!("auth failed: {e}")))
                    }
                    Err(e) => Err(Error::Connection(format!("LOGIN failed: {e}"))),
                }
            }
            SaslAuth::Bearer { user, token } => {
                let has_xoauth2 = client.has(Capability::AuthXoauth2);
                let has_oauthbearer = client.has(Capability::AuthOauthBearer);
                if !has_xoauth2 && !has_oauthbearer {
                    return Err(Error::Connection(
                        "server does not advertise AUTH=XOAUTH2 or AUTH=OAUTHBEARER".into(),
                    ));
                }
                if has_xoauth2 {
                    match client.authenticate(SaslPayload::XOauth2 { user, token }) {
                        Ok(()) => return Ok(user.clone()),
                        Err(e) if e.is_auth_failed() => {
                            return Err(Error::Connection(format!("auth failed: {e}")));
                        }
                        Err(e) if e.is_negotiation_failure() && has_oauthbearer => {}
                        Err(e) => return Err(Error::Connection(format!("XOAUTH2 failed: {e}"))),
                    }
                }
                client
                    .authenticate(SaslPayload::OAuthBearer { user, token })
                    .map_err(|e| Error::Connection(format!("OAUTHBEARER failed: {e}")))?;
                Ok(user.clone())
            }
        }
    }
}

impl ImapClient {
    pub fn name_encoding(&self) -> NameEncoding {
        if self.utf8_accept() {
            NameEncoding::Utf8
        } else {
            NameEncoding::ModifiedUtf7
        }
    }
}
