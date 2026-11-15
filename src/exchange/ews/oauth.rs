/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::time::Duration;

use crate::exchange::ews::error::EwsError;
use crate::exchange::oauth::{Authority, Dialect, OAuthClient, OAuthError, RefreshSchedule};
use crate::net::http::ClientOptions;

pub use crate::exchange::oauth::AcquiredToken;

pub const SCOPE_APP_ONLY: &str = "https://outlook.office365.com/.default";
pub const SCOPE_DELEGATED: &str =
    "https://outlook.office365.com/EWS.AccessAsUser.All offline_access";
pub const REFRESH_SCHEDULE: RefreshSchedule = RefreshSchedule {
    lead: Duration::from_secs(5 * 60),
    fallback_lifetime: Duration::from_secs(50 * 60),
};
const REFRESH_ATTEMPTS: u32 = 1;

#[derive(Debug, Clone)]
pub enum OAuthFlow {
    PreAcquired {
        token: String,
    },
    ClientCredentials {
        tenant: String,
        client_id: String,
        client_secret: String,
    },
    DeviceCode {
        tenant: String,
        client_id: String,
    },
}

impl OAuthFlow {
    pub fn acquire(&self, options: ClientOptions) -> Result<AcquiredToken, EwsError> {
        match self {
            OAuthFlow::PreAcquired { token } => Ok(AcquiredToken::from_bearer(token.clone())),
            OAuthFlow::ClientCredentials {
                tenant,
                client_id,
                client_secret,
            } => {
                let authority = Authority::for_tenant(tenant);
                Ok(
                    OAuthClient::new(options, &authority, client_id, SCOPE_APP_ONLY, Dialect::Ews)
                        .client_credentials(client_secret)?,
                )
            }
            OAuthFlow::DeviceCode { tenant, client_id } => {
                let authority = Authority::for_tenant(tenant);
                Ok(OAuthClient::new(
                    options,
                    &authority,
                    client_id,
                    SCOPE_DELEGATED,
                    Dialect::Ews,
                )
                .device_code()?)
            }
        }
    }

    pub fn renew(
        &self,
        options: ClientOptions,
        refresh_token: Option<&str>,
    ) -> Result<AcquiredToken, OAuthError> {
        match (refresh_token, self) {
            (Some(refresh_token), OAuthFlow::DeviceCode { tenant, client_id }) => {
                let authority = Authority::for_tenant(tenant);
                OAuthClient::new(
                    options,
                    &authority,
                    client_id,
                    SCOPE_DELEGATED,
                    Dialect::Ews,
                )
                .refresh(refresh_token, REFRESH_ATTEMPTS)
                .map_err(|e| OAuthError(EwsError::from(e).to_string()))
            }
            _ => self.acquire(options).map_err(|e| OAuthError(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::{Level, Logger};
    use crate::net::http::TlsPolicy;
    use crate::net::retry::RetryPolicy;

    #[test]
    fn pre_acquired_flow_keeps_the_token() {
        let options = ClientOptions {
            retry: RetryPolicy::new(0),
            tls: TlsPolicy::Verify,
            logger: Logger::new(Level::Quiet),
        };
        let acq = OAuthFlow::PreAcquired {
            token: "a.e30.c".to_owned(),
        }
        .acquire(options)
        .unwrap();
        assert_eq!(acq.access_token, "a.e30.c");
        assert_eq!(acq.upn, None);
    }
}
