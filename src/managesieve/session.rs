/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::error::Error;
use crate::logging::Logger;
use crate::net::sasl::{SaslAuth, SaslPayload};
use crate::net::stream::{Cleartext, ConnectMode, Connector, Endpoint, Scheme, UrlTls};

use super::client::SieveClient;
use super::error::SieveError;

pub const SIEVE: Scheme = Scheme {
    plain: "sieve",
    tls: "sieves",
    plain_port: 4190,
    tls_port: 4190,
};

#[derive(Debug)]
enum AuthFailure {
    Refused(String),
    NoMechanism(String),
    Wire(SieveError),
}

pub struct SieveSession {
    connector: Connector,
    endpoint: Endpoint,
    mode: ConnectMode,
    cleartext: Cleartext,
    auth: SaslAuth,
    logger: Logger,
}

impl SieveSession {
    pub fn new(
        connector: Connector,
        endpoint: Endpoint,
        cleartext: Cleartext,
        auth: SaslAuth,
        logger: Logger,
    ) -> SieveSession {
        let mode = match endpoint.tls {
            UrlTls::Implicit => ConnectMode::ImplicitTls,
            UrlTls::Negotiated => ConnectMode::StartTls,
        };
        SieveSession {
            connector,
            endpoint,
            mode,
            cleartext,
            auth,
            logger,
        }
    }

    pub fn session_url(&self) -> String {
        self.endpoint.session_url(&SIEVE)
    }

    pub fn account_id(&self) -> &str {
        self.auth.user()
    }

    pub fn logger(&self) -> Logger {
        self.logger
    }

    fn connect(&self) -> Result<SieveClient, SieveError> {
        SieveClient::connect(
            &self.connector,
            &self.endpoint.host,
            self.endpoint.port,
            self.mode,
            self.cleartext,
            self.logger,
        )
    }

    pub fn open(&self) -> Result<SieveClient, Error> {
        let mut client = self.connect().map_err(Error::connection)?;
        self.authenticate(&mut client).map_err(|e| match e {
            AuthFailure::Refused(text) => Error::Connection(format!("auth failed: {text}")),
            AuthFailure::NoMechanism(text) => Error::Connection(text),
            AuthFailure::Wire(e) => Error::Connection(e.to_string()),
        })?;
        self.logger.progress(format_args!(
            "Authenticated as {} on {}",
            self.account_id(),
            self.session_url()
        ));
        self.refresh_capabilities(&mut client, "post-auth");
        Ok(client)
    }

    pub fn reopen(&self) -> Result<SieveClient, SieveError> {
        let mut client = self.connect()?;
        self.authenticate(&mut client).map_err(|e| match e {
            AuthFailure::Wire(e) => e,
            AuthFailure::Refused(text) => {
                SieveError::Protocol(format!("auth refused on reconnect: {text}"))
            }
            AuthFailure::NoMechanism(text) => SieveError::Unsupported(text),
        })?;
        self.refresh_capabilities(&mut client, "post-reconnect");
        Ok(client)
    }

    fn refresh_capabilities(&self, client: &mut SieveClient, when: &str) {
        if !client.had_fresh_post_auth_caps()
            && let Err(e) = client.refresh_capabilities()
        {
            self.logger.warn(format_args!(
                "{when} CAPABILITY refresh failed: {e}; continuing"
            ));
        }
    }

    fn authenticate(&self, client: &mut SieveClient) -> Result<(), AuthFailure> {
        match &self.auth {
            SaslAuth::Basic { user, password } => {
                let plain = client.capabilities().has_sasl("PLAIN");
                let login = client.capabilities().has_sasl("LOGIN");
                let mut plain_error: Option<String> = None;
                if plain {
                    match client.authenticate(SaslPayload::Plain {
                        authcid: user,
                        password,
                    }) {
                        Ok(()) => return Ok(()),
                        Err(e) if e.is_negotiation_failure() => plain_error = Some(e.to_string()),
                        Err(e) => return Err(AuthFailure::Wire(e)),
                    }
                }
                if login {
                    return match client.authenticate_login(user, password) {
                        Ok(()) => Ok(()),
                        Err(e) if e.is_negotiation_failure() => {
                            Err(AuthFailure::Refused(match plain_error {
                                Some(p) => format!("LOGIN: {e}; PLAIN: {p}"),
                                None => format!("LOGIN: {e}"),
                            }))
                        }
                        Err(e) => Err(AuthFailure::Wire(e)),
                    };
                }
                Err(match plain_error {
                    Some(p) => AuthFailure::Refused(format!("PLAIN: {p}")),
                    None => AuthFailure::NoMechanism(format!(
                        "server SASL list lacks PLAIN and LOGIN (advertised: {:?})",
                        client.capabilities().sasl
                    )),
                })
            }
            SaslAuth::Bearer { user, token } => {
                if !client.capabilities().has_sasl("OAUTHBEARER") {
                    return Err(AuthFailure::NoMechanism(format!(
                        "server does not advertise OAUTHBEARER (advertised: {:?})",
                        client.capabilities().sasl
                    )));
                }
                match client.authenticate(SaslPayload::OAuthBearer { user, token }) {
                    Ok(()) => Ok(()),
                    Err(e) if e.is_negotiation_failure() => {
                        Err(AuthFailure::Refused(format!("OAUTHBEARER: {e}")))
                    }
                    Err(e) => Err(AuthFailure::Wire(e)),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    #[test]
    fn sieve_urls_default_to_4190_and_keep_their_tls_mode() {
        let e = Endpoint::parse("sieves://mail.example.com", &SIEVE).unwrap();
        assert_eq!(
            (e.host.as_str(), e.port, e.tls),
            ("mail.example.com", 4190, UrlTls::Implicit)
        );
        let e = Endpoint::parse("sieve://mail.example.com", &SIEVE).unwrap();
        assert_eq!(
            (e.host.as_str(), e.port, e.tls),
            ("mail.example.com", 4190, UrlTls::Negotiated)
        );
        let e = Endpoint::parse("sieve://mail.example.com:14190", &SIEVE).unwrap();
        assert_eq!((e.port, e.tls), (14190, UrlTls::Negotiated));
        assert!(matches!(
            Endpoint::parse("imap://example.com", &SIEVE),
            Err(Error::Usage(_))
        ));
        assert!(matches!(
            Endpoint::parse("sieve:///path", &SIEVE),
            Err(Error::Usage(_))
        ));
    }
}
