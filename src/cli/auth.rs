/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use clap::{ArgGroup, Args};

use crate::error::Error;
use crate::jmap::account::AccountSelector;
use crate::net::auth::Auth;
use crate::secret::Secret;

#[derive(Args)]
#[command(group(ArgGroup::new("auth").required(true).args(["auth_basic", "auth_bearer"])))]
pub(super) struct AuthArgs {
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
        help = "Bearer token (value optional: falls back to $VANDELAY_TOKEN or prompt)"
    )]
    auth_bearer: Option<Option<String>>,
}

impl AuthArgs {
    pub(super) fn resolve(&self) -> Result<Auth, Error> {
        let flags = AuthFlags {
            basic: self.auth_basic.as_deref(),
            password: self.auth_password.as_deref(),
            bearer: self.auth_bearer.as_ref().map(Option::as_deref),
        };
        flags.resolve()
    }
}

#[derive(Clone, Copy)]
pub(super) struct AuthFlags<'a> {
    pub(super) basic: Option<&'a str>,
    pub(super) password: Option<&'a str>,
    pub(super) bearer: Option<Option<&'a str>>,
}

impl AuthFlags<'_> {
    pub(super) fn resolve(self) -> Result<Auth, Error> {
        if let Some(user) = self.basic {
            let password = Secret::resolve(self.password, "VANDELAY_PASSWORD", "password")?;
            return Ok(Auth::Basic {
                user: user.to_owned(),
                password: password.into_inner(),
            });
        }
        if self.password.is_some() {
            return Err(Error::Usage(
                "--auth-password is only valid together with --auth-basic".to_owned(),
            ));
        }
        let bearer = self.bearer.ok_or_else(|| {
            Error::Usage("exactly one of --auth-basic / --auth-bearer is required".to_owned())
        })?;
        let token = Secret::resolve(bearer, "VANDELAY_TOKEN", "bearer token")?;
        Ok(Auth::Bearer {
            token: token.into_inner(),
        })
    }
}

pub(super) struct AccountFlags {
    pub(super) id: Option<String>,
    pub(super) name: Option<String>,
}

impl AccountFlags {
    pub(super) fn resolve(self) -> Result<AccountSelector, Error> {
        match (self.id, self.name) {
            (Some(id), None) => Ok(AccountSelector::Id(id)),
            (None, Some(name)) => Ok(AccountSelector::Name(name)),
            _ => Err(Error::Usage(
                "exactly one of --account-id / --account-name is required".to_owned(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::env;

    use super::*;
    use crate::cli::env_lock as lock;

    #[test]
    fn auth_password_without_basic_is_usage_error() {
        let _g = lock();
        unsafe {
            env::remove_var("VANDELAY_TOKEN");
            env::remove_var("VANDELAY_PASSWORD");
        }
        let bearer: Option<String> = Some("t".to_owned());
        let r = AuthFlags {
            basic: None,
            password: Some("hunter2"),
            bearer: Some(bearer.as_deref()),
        }
        .resolve();
        match r {
            Err(Error::Usage(m)) => {
                assert!(m.contains("auth-password"), "msg was: {m}");
            }
            other => panic!("expected Usage error, got {other:?}"),
        }
    }

    #[test]
    fn auth_basic_with_password_resolves() {
        let _g = lock();
        unsafe {
            env::remove_var("VANDELAY_TOKEN");
            env::remove_var("VANDELAY_PASSWORD");
        }
        let auth = AuthFlags {
            basic: Some("alice"),
            password: Some("pw"),
            bearer: None,
        }
        .resolve()
        .unwrap();
        match auth {
            Auth::Basic { user, password } => {
                assert_eq!(user, "alice");
                assert_eq!(password, "pw");
            }
            _ => panic!("expected Basic"),
        }
    }

    #[test]
    fn auth_bearer_inline_token_resolves() {
        let _g = lock();
        unsafe {
            env::remove_var("VANDELAY_TOKEN");
            env::remove_var("VANDELAY_PASSWORD");
        }
        let bearer: Option<String> = Some("t".to_owned());
        let auth = AuthFlags {
            basic: None,
            password: None,
            bearer: Some(bearer.as_deref()),
        }
        .resolve()
        .unwrap();
        assert!(matches!(auth, Auth::Bearer { token } if token == "t"));
    }
}
