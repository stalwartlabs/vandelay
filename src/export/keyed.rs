/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashSet;

use serde_json::Value;

use super::target::{ClientId, TargetJson};
use super::{Exporter, Plan};
use crate::archive::LocalId;
use crate::error::Error;
use crate::jmap::wire::identity::Identity;
use crate::sync::TypeCounts;
use crate::sync::keys::Key32;
use crate::types::ObjectType;

impl Exporter<'_> {
    pub(super) fn reconcile_identities(&self, counts: &mut TypeCounts) -> Result<Plan, Error> {
        let ty = ObjectType::Identity;
        let locals = self.identity_locals()?;
        let targets = self.get_all(ty)?;
        let target_keys: HashSet<Key32> = targets
            .iter()
            .map(|t| {
                let t = TargetJson(t);
                Key32::identity(
                    t.text("name").unwrap_or_default(),
                    t.text("email").unwrap_or_default(),
                )
            })
            .collect();

        let mut seen: HashSet<Key32> = HashSet::with_capacity(locals.len());
        let mut batch = Vec::new();
        for (local, key, wire) in locals {
            if target_keys.contains(&key) || !seen.insert(key) {
                counts.skipped += 1;
                continue;
            }
            batch.push((ClientId::object(local), wire));
        }
        if !batch.is_empty() {
            let outcome = self.create(ty, batch)?;
            counts.created += outcome.created.len() as u64;
            for (cid, err) in &outcome.not_created {
                self.logger.warn(format_args!(
                    "{} {cid} not created (expected for addresses the account does not own): {err}",
                    ty.noun()
                ));
                counts.skipped += 1;
            }
        }
        Ok(Plan::default())
    }

    fn identity_locals(&self) -> Result<Vec<(LocalId, Key32, Value)>, Error> {
        let mut stmt = self
            .store
            .connection()
            .prepare(Identity::SELECT)
            .map_err(Error::partial)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, LocalId>(0)?, Identity::from_row(row)))
            })
            .map_err(Error::partial)?;
        let rows: Vec<_> = rows.collect::<Result<_, _>>().map_err(Error::partial)?;
        let mut out = Vec::with_capacity(rows.len());
        for (id, wire) in rows {
            let wire = wire?;
            let key = Key32::identity(&wire.name, &wire.email);
            out.push((
                id,
                key,
                serde_json::to_value(&wire).map_err(Error::partial)?,
            ));
        }
        Ok(out)
    }
}
