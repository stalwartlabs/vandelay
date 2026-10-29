/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use serde_json::json;

use super::Net;
use crate::db;
use crate::error::Error;
use crate::jmap::request::{SetRequest, set_call};
use crate::jmap::wire::vacation_response::{SINGLETON_ID, TYPE_NAME};
use crate::logging::Logger;
use crate::sync::{Context, TypeCounts};

pub fn reconcile(
    ctx: &Context,
    net: &Net,
    counts: &mut TypeCounts,
    logger: &Logger,
) -> Result<bool, Error> {
    let Some(vacation) =
        db::vacation_response::load(&ctx.conn).map_err(|e| Error::Partial(e.to_string()))?
    else {
        return Ok(false);
    };
    if net.dry_run {
        counts.updated += 1;
        return Ok(true);
    }
    let outcome = set_call(
        &net.client,
        &net.api,
        &net.account,
        TYPE_NAME,
        SetRequest {
            update: Some(json!({ SINGLETON_ID: vacation })),
            ..Default::default()
        },
        &net.limits,
    )
    .map_err(Error::from)?;
    counts.updated += outcome.updated.len() as u64;
    for (id, err) in &outcome.not_updated {
        logger.warn(&format!("{TYPE_NAME} {id} not updated: {err}"));
        counts.failed += 1;
    }
    Ok(true)
}
