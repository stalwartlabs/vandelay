/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::console::Initial;
use crate::error::Error;
use crate::jmap::account::AccountSelector;
use crate::jmap::http::JmapClient;
use crate::jmap::session::{Limits, Session};
use crate::net::auth::Auth;
use crate::types::ObjectType;

#[derive(Debug, Clone)]
pub struct ConnectConfig {
    pub url: String,
    pub auth: Auth,
    pub account: AccountSelector,
}

#[derive(Clone)]
pub struct JmapAccount {
    client: JmapClient,
    session: Session,
    account_id: String,
    limits: Limits,
    supported: Vec<ObjectType>,
}

impl JmapAccount {
    pub fn connect(client: JmapClient, connect: &ConnectConfig) -> Result<JmapAccount, Error> {
        let logger = client.logger();
        let session = Session::discover(&client, &connect.url)?;
        for mismatch in session.origin_mismatches(&connect.url) {
            logger.warn(format_args!("{mismatch}"));
        }
        let limits = session.core_limits()?;
        client.set_limits(&limits);
        let account_id = connect.account.resolve(&session, &client)?;

        let known = session.account(&account_id).is_some();
        if !known && !matches!(connect.account, AccountSelector::Name(_)) {
            logger.warn(format_args!(
                "account id {account_id} is not enumerated in this session; capabilities cannot be gated and every type will be attempted"
            ));
        }

        let mut supported = Vec::with_capacity(ObjectType::ALL.len());
        for ty in ObjectType::ALL {
            let urn = ty.capability_urn();
            if !known || session.supports(&account_id, urn) {
                supported.push(ty);
            } else {
                logger.warn(format_args!(
                    "the target account does not advertise {urn}; skipping {}",
                    Initial::Lower(ty.label())
                ));
            }
        }

        Ok(JmapAccount {
            client,
            session,
            account_id,
            limits,
            supported,
        })
    }

    pub fn from_parts(
        client: JmapClient,
        session: Session,
        account_id: String,
        limits: Limits,
    ) -> JmapAccount {
        JmapAccount {
            client,
            session,
            account_id,
            limits,
            supported: ObjectType::ALL.to_vec(),
        }
    }

    pub fn client(&self) -> &JmapClient {
        &self.client
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    pub fn api_url(&self) -> &str {
        &self.session.api_url
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    pub fn supports(&self, ty: ObjectType) -> bool {
        self.supported.contains(&ty)
    }
}
