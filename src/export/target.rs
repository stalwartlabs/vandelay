/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;
use std::fmt;

use serde_json::{Map, Value, json};

use super::Exporter;
use super::uploader::Uploader;
use crate::archive::LocalId;
use crate::error::Error;
use crate::jmap::request::{SetOutcome, SetRequest};
use crate::jmap::wire::JmapId;
use crate::sync::TypeCounts;
use crate::types::ObjectType;

#[derive(Default)]
pub(super) struct Maps(HashMap<ObjectType, HashMap<LocalId, JmapId>>);

impl Maps {
    pub(super) fn insert(&mut self, ty: ObjectType, local: LocalId, target: JmapId) {
        self.0.entry(ty).or_default().insert(local, target);
    }

    pub(super) fn target(&self, ty: ObjectType, local: LocalId) -> Option<JmapId> {
        self.0.get(&ty)?.get(&local).cloned()
    }

    pub(super) fn record_created(
        &mut self,
        ty: ObjectType,
        outcome: &SetOutcome,
        counts: &mut TypeCounts,
    ) {
        for (cid, v) in &outcome.created {
            if let Some(local) = ClientId::parse(cid)
                && let Some(id) = TargetJson(v).id()
            {
                self.insert(ty, local, JmapId(id.to_owned()));
                counts.created += 1;
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ClientId(String);

impl ClientId {
    pub(super) fn object(local: LocalId) -> ClientId {
        ClientId(format!("c{local}"))
    }

    pub(super) fn email(local: LocalId) -> ClientId {
        ClientId(format!("e{local}"))
    }

    pub(super) fn parse(cid: &str) -> Option<LocalId> {
        cid.strip_prefix('c')?
            .parse::<i64>()
            .ok()
            .map(LocalId::from_raw)
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }

    pub(super) fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy)]
pub(super) struct TargetJson<'v>(pub(super) &'v Value);

impl<'v> TargetJson<'v> {
    pub(super) fn id(self) -> Option<&'v str> {
        self.text("id")
    }

    pub(super) fn text(self, key: &str) -> Option<&'v str> {
        self.0.get(key)?.as_str()
    }

    pub(super) fn flag(self, key: &str) -> Option<bool> {
        self.0.get(key)?.as_bool()
    }

    pub(super) fn may_delete(self) -> Option<bool> {
        self.0.get("myRights")?.get("mayDelete")?.as_bool()
    }

    pub(super) fn error_type(self) -> Option<&'v str> {
        self.text("type")
    }

    pub(super) fn existing_id(self) -> Option<&'v str> {
        if self.error_type() == Some("alreadyExists") {
            self.text("existingId")
        } else {
            None
        }
    }
}

impl Exporter<'_> {
    pub(super) fn query_get(
        &self,
        ty: ObjectType,
        props: Option<&[&str]>,
    ) -> Result<Vec<Value>, Error> {
        let ids = self.account.query_all_ids(ty.jmap_name())?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self.account.get::<Value>(ty.jmap_name(), &ids, props)?.list)
    }

    pub(super) fn get_all(&self, ty: ObjectType) -> Result<Vec<Value>, Error> {
        Ok(self.account.get_all::<Value>(ty.jmap_name())?.list)
    }

    pub(super) fn create(
        &self,
        ty: ObjectType,
        creates: Vec<(ClientId, Value)>,
    ) -> Result<SetOutcome, Error> {
        if self.is_dry_run() {
            let mut outcome = SetOutcome::default();
            outcome.created.reserve(creates.len());
            for (cid, _) in creates {
                let synthetic = json!({ "id": format!("dryrun-{}-{cid}", ty.jmap_name()) });
                outcome.created.push((cid.into_string(), synthetic));
            }
            return Ok(outcome);
        }
        let map: Map<String, Value> = creates
            .into_iter()
            .map(|(cid, obj)| (cid.into_string(), obj))
            .collect();
        Ok(self.account.set(
            ty.jmap_name(),
            SetRequest {
                create: Some(Value::Object(map)),
                ..Default::default()
            },
        )?)
    }

    pub(super) fn retry_if_blob_missing(
        &self,
        ty: ObjectType,
        cid: &ClientId,
        uploader: &mut Uploader<'_>,
        outcome: SetOutcome,
        mut rebuild: impl FnMut(&mut Uploader<'_>) -> Result<Value, Error>,
    ) -> Result<SetOutcome, Error> {
        let blob_missing = outcome.not_created.iter().any(|(c, err)| {
            c == cid.as_str() && TargetJson(err).error_type() == Some("blobNotFound")
        });
        if !blob_missing {
            return Ok(outcome);
        }
        uploader.invalidate_touched();
        let wire = rebuild(uploader)?;
        uploader.clear_touched();
        self.create(ty, vec![(cid.clone(), wire)])
    }

    pub(super) fn warn_not_created(
        &self,
        ty: ObjectType,
        cid: &str,
        err: &Value,
        counts: &mut TypeCounts,
    ) {
        self.logger
            .warn(format_args!("{} {cid} not created: {err}", ty.noun()));
        counts.failed += 1;
    }
}
