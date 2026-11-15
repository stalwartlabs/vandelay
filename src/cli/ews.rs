/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::env;
use std::path::PathBuf;

use clap::Args;

use super::Action;
use super::global::GlobalArgs;
use crate::archive::sources::SourceChange;
use crate::error::Error;
use crate::exchange::ews::oauth::OAuthFlow;
use crate::exchange::ews::types::MailboxKind;
use crate::import::ews::items::MAX_EWS_CONNECTIONS;
use crate::import::ews::{EwsAuth, EwsImport, ItemEnumeration};
use crate::secret::Secret;

#[derive(Args)]
#[command(group(clap::ArgGroup::new("ews_auth").required(true).args([
    "auth_basic", "auth_bearer",
])))]
pub struct ExchangeEwsImportArgs {
    #[arg(long, value_name = "URL", help = "Fully-qualified EWS endpoint URL")]
    url: Option<String>,

    #[arg(
        long,
        value_name = "EMAIL",
        help = "Target mailbox SMTP address (required for autodiscover and app-only OAuth)"
    )]
    mailbox: Option<String>,

    #[arg(
        long,
        value_name = "KIND",
        default_value = "primary",
        help = "Mailbox surface: primary | archive | public-folders"
    )]
    mailbox_kind: String,

    #[arg(long, value_name = "USER", help = "Basic auth user (on-prem only)")]
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
        help = "Bearer token (value optional: $VANDELAY_TOKEN or device-code flow)"
    )]
    auth_bearer: Option<Option<String>>,

    #[arg(
        long,
        value_name = "TENANT",
        help = "Entra tenant id or .onmicrosoft.com short name"
    )]
    ews_tenant: Option<String>,

    #[arg(long, value_name = "ID", help = "Entra app (client) id")]
    ews_client_id: Option<String>,

    #[arg(
        long,
        value_name = "SECRET",
        help = "OAuth client secret (app-only flow; prefer $VANDELAY_EWS_CLIENT_SECRET)"
    )]
    ews_client_secret: Option<String>,

    #[arg(long, help = "Use the OAuth device-code interactive flow")]
    ews_device_code: bool,

    #[arg(
        long,
        value_name = "N",
        default_value_t = 4,
        help = "Parallel EWS worker connections (hard cap 8)"
    )]
    ews_connections: usize,

    #[arg(
        long,
        value_name = "N",
        default_value_t = 20,
        help = "ItemIds per GetItem batch"
    )]
    ews_getitem_batch: usize,

    #[arg(
        long,
        value_name = "N",
        default_value_t = 5,
        help = "AttachmentIds per GetAttachment batch"
    )]
    ews_attachment_batch: usize,

    #[arg(long, help = "Force FindItem even when SyncFolderItems would work")]
    ews_no_syncfolderitems: bool,

    #[arg(
        long,
        help = "Permit importing when the archive records a different EWS source"
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

impl ExchangeEwsImportArgs {
    pub(super) fn resolve(self) -> Result<Action, Error> {
        let archive = self.archive.clone();
        let common = self.global.common_config(archive);
        let mailbox_kind = MailboxKind::parse(&self.mailbox_kind).ok_or_else(|| {
            Error::Usage(format!(
                "--mailbox-kind must be primary | archive | public-folders, got {:?}",
                self.mailbox_kind
            ))
        })?;
        let auth = self.auth()?;
        let ews_connections = self
            .ews_connections
            .clamp(1, MAX_EWS_CONNECTIONS)
            .min(common.threads.max(1));
        let config = EwsImport {
            url: self.url,
            mailbox: self.mailbox,
            mailbox_kind,
            auth,
            ews_connections,
            getitem_batch: self.ews_getitem_batch.max(1),
            attachment_batch: self.ews_attachment_batch.max(1),
            enumeration: if self.ews_no_syncfolderitems {
                ItemEnumeration::FindItem
            } else {
                ItemEnumeration::SyncFolderItems
            },
            source_change: if self.allow_source_change {
                SourceChange::Allow
            } else {
                SourceChange::Reject
            },
        };
        Ok(Action::import(common, config))
    }

    fn auth(&self) -> Result<EwsAuth, Error> {
        if let Some(user) = self.auth_basic.as_deref() {
            let password = Secret::resolve(
                self.auth_password.as_deref(),
                "VANDELAY_PASSWORD",
                "password",
            )?
            .into_inner();
            return Ok(EwsAuth::Basic {
                user: user.to_owned(),
                password,
            });
        }
        if self.auth_password.is_some() {
            return Err(Error::Usage(
                "--auth-password is only valid with --auth-basic".to_owned(),
            ));
        }
        let Some(bearer) = self.auth_bearer.as_ref() else {
            return Err(Error::Usage(
                "exactly one of --auth-basic / --auth-bearer is required".to_owned(),
            ));
        };
        if let Some(token) = bearer.clone() {
            return Ok(EwsAuth::Bearer { token });
        }
        let tenant = || {
            self.ews_tenant
                .clone()
                .unwrap_or_else(|| "common".to_owned())
        };
        let required_client_id = || {
            self.ews_client_id.clone().ok_or_else(|| {
                Error::Usage(
                    "--ews-client-id is required with --ews-device-code or --ews-client-secret"
                        .to_owned(),
                )
            })
        };
        if self.ews_device_code {
            return Ok(EwsAuth::OAuth(OAuthFlow::DeviceCode {
                tenant: tenant(),
                client_id: required_client_id()?,
            }));
        }
        if let Some(secret) = self.ews_client_secret.as_deref() {
            return Ok(EwsAuth::OAuth(OAuthFlow::ClientCredentials {
                tenant: tenant(),
                client_id: required_client_id()?,
                client_secret: secret.to_owned(),
            }));
        }
        if let Ok(token) = env::var("VANDELAY_TOKEN")
            && !token.is_empty()
        {
            return Ok(EwsAuth::Bearer { token });
        }
        if let Some(client_id) = self.ews_client_id.clone()
            && let Ok(client_secret) = env::var("VANDELAY_EWS_CLIENT_SECRET")
            && !client_secret.is_empty()
        {
            return Ok(EwsAuth::OAuth(OAuthFlow::ClientCredentials {
                tenant: tenant(),
                client_id,
                client_secret,
            }));
        }
        let token = Secret::resolve(None, "VANDELAY_TOKEN", "bearer token")?.into_inner();
        Ok(EwsAuth::Bearer { token })
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::cli::{Cli, env_lock as lock};
    use crate::import::Import;

    fn ews_auth(extra: &[&str]) -> Result<EwsAuth, Error> {
        let mut argv = vec!["vandelay", "import", "exchange-ews", "archive.sqlite"];
        argv.extend_from_slice(extra);
        let cli = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("clap rejected args: {e}"));
        match cli.resolve()? {
            Action::Import(_, Import::Ews(config)) => Ok(config.auth),
            _ => panic!("expected ImportExchangeEws"),
        }
    }

    fn set_ews_env(token: Option<&str>, client_secret: Option<&str>) {
        unsafe {
            match token {
                Some(t) => env::set_var("VANDELAY_TOKEN", t),
                None => env::remove_var("VANDELAY_TOKEN"),
            }
            match client_secret {
                Some(s) => env::set_var("VANDELAY_EWS_CLIENT_SECRET", s),
                None => env::remove_var("VANDELAY_EWS_CLIENT_SECRET"),
            }
        }
    }

    // A bare --auth-bearer takes VANDELAY_TOKEN without needing --ews-client-id.
    #[test]
    fn ews_bare_bearer_uses_env_token_without_client_id() {
        let _g = lock();
        set_ews_env(Some("envtok"), None);
        let auth = ews_auth(&["--auth-bearer"]);
        set_ews_env(None, None);
        assert!(matches!(auth, Ok(EwsAuth::Bearer { token }) if token == "envtok"));
    }

    // VANDELAY_TOKEN wins over client credentials taken from the environment.
    #[test]
    fn ews_env_token_beats_env_client_secret() {
        let _g = lock();
        set_ews_env(Some("envtok"), Some("sec"));
        let auth = ews_auth(&["--auth-bearer", "--ews-client-id", "cid"]);
        set_ews_env(None, None);
        assert!(matches!(auth, Ok(EwsAuth::Bearer { token }) if token == "envtok"));
    }

    // The env client secret is used when no token is exported.
    #[test]
    fn ews_env_client_secret_without_token_uses_client_credentials() {
        let _g = lock();
        set_ews_env(None, Some("sec"));
        let auth = ews_auth(&["--auth-bearer", "--ews-client-id", "cid"]);
        set_ews_env(None, None);
        assert!(matches!(
            auth,
            Ok(EwsAuth::OAuth(OAuthFlow::ClientCredentials { client_id, client_secret, .. }))
                if client_id == "cid" && client_secret == "sec"
        ));
    }

    // An explicit --ews-device-code is not overridden by an exported token.
    #[test]
    fn ews_device_code_flag_beats_env_token() {
        let _g = lock();
        set_ews_env(Some("envtok"), None);
        let auth = ews_auth(&[
            "--auth-bearer",
            "--ews-client-id",
            "cid",
            "--ews-device-code",
        ]);
        set_ews_env(None, None);
        assert!(matches!(
            auth,
            Ok(EwsAuth::OAuth(OAuthFlow::DeviceCode { tenant, client_id }))
                if tenant == "common" && client_id == "cid"
        ));
    }

    // --ews-device-code still needs a client id.
    #[test]
    fn ews_device_code_without_client_id_is_usage_error() {
        let _g = lock();
        set_ews_env(Some("envtok"), None);
        let auth = ews_auth(&["--auth-bearer", "--ews-device-code"]);
        set_ews_env(None, None);
        match auth {
            Err(Error::Usage(m)) => assert!(m.contains("--ews-client-id"), "msg was: {m}"),
            other => panic!("expected Usage error, got {other:?}"),
        }
    }

    // An inline token wins over everything else.
    #[test]
    fn ews_inline_token_beats_env_and_flows() {
        let _g = lock();
        set_ews_env(Some("envtok"), None);
        let auth = ews_auth(&[
            "--auth-bearer=inline",
            "--ews-client-id",
            "cid",
            "--ews-device-code",
        ]);
        set_ews_env(None, None);
        assert!(matches!(auth, Ok(EwsAuth::Bearer { token }) if token == "inline"));
    }
}
