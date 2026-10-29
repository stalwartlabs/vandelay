/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::error::Error;
use crate::jmap::account;
use crate::jmap::session::{Limits, Session};
use crate::sync::{ConnectConfig, Context};
use crate::types::ObjectType;

pub struct Connected {
    pub session: Session,
    pub limits: Limits,
    pub account_id: String,
    supported: Vec<ObjectType>,
}

impl Connected {
    pub fn supports(&self, ty: ObjectType) -> bool {
        self.supported.contains(&ty)
    }

    pub fn supported_types(&self) -> &[ObjectType] {
        &self.supported
    }
}

pub fn prepare(ctx: &Context, connect: &ConnectConfig) -> Result<Connected, Error> {
    ctx.client.set_logger(ctx.common.logger);

    let session = Session::discover(&ctx.client, &connect.url)?;
    for mismatch in session.origin_mismatches(&connect.url) {
        ctx.common.logger.warn(&mismatch.to_string());
    }
    let limits = session.core_limits()?;
    ctx.client.set_limits(&limits);
    let account_id = account::resolve(&connect.account, &session, &ctx.client)?;

    if session.account(&account_id).is_none()
        && !matches!(connect.account, account::AccountSelector::Name(_))
    {
        ctx.common.logger.warn(&format!(
            "account id {account_id} is not enumerated in this session; capabilities cannot be gated and every type will be attempted"
        ));
    }

    let mut supported = Vec::new();
    for ty in ObjectType::ALL {
        let urn = ty.capability_urn();
        let known = session.account(&account_id).is_some();
        if !known || session.supports(&account_id, urn) {
            supported.push(ty);
        } else {
            ctx.common.logger.warn(&format!(
                "target account does not advertise {urn}; skipping type {}",
                ty.jmap_name()
            ));
        }
    }

    Ok(Connected {
        session,
        limits,
        account_id,
        supported,
    })
}
