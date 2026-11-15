/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod address_book;
mod calendar;
mod calendar_event;
mod contact_card;
mod email;
mod file_node;
mod identity;
mod mailbox;
mod participant_identity;
mod sieve_script;
mod vacation;

#[cfg(test)]
mod tests;

use indexmap::IndexMap;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use time::format_description::well_known::Rfc3339;

use crate::archive::{BlobId, LocalId};
use crate::jmap::blob::{BlobRefs, BlobWalkError, InlineShape};
use crate::jmap::error::JmapError;
use crate::jmap::wire::JmapId;
use crate::jmap::wire::common::UtcDate;
use crate::types::ObjectType;

pub(crate) use self::email::EmailRow;
pub(crate) use self::file_node::FileNodeRow;
pub(crate) use self::sieve_script::SieveRow;

pub(crate) struct IdRefs<'a>(&'a IndexMap<JmapId, bool>);

impl IdRefs<'_> {
    fn to_local(
        &self,
        ty: ObjectType,
        resolve: &impl Fn(ObjectType, &str) -> Option<LocalId>,
    ) -> Result<Vec<LocalId>, JmapError> {
        self.0
            .keys()
            .map(|id| {
                resolve(ty, &id.0).ok_or_else(|| {
                    JmapError::malformed(format!(
                        "unresolved {} reference {} on insert",
                        ty.jmap_name(),
                        id.0
                    ))
                })
            })
            .collect()
    }

    fn parent(
        parent: Option<&JmapId>,
        ty: ObjectType,
        resolve: &impl Fn(ObjectType, &str) -> Option<LocalId>,
    ) -> Option<LocalId> {
        parent.and_then(|p| resolve(ty, &p.0))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LocalIds(Vec<LocalId>);

impl LocalIds {
    pub(crate) fn parse(text: &str) -> Result<LocalIds, JmapError> {
        let v: Value = serde_json::from_str(text)?;
        v.as_array()
            .ok_or_else(|| JmapError::malformed("id column is not a JSON array"))?
            .iter()
            .map(|x| {
                x.as_i64()
                    .map(LocalId::from_raw)
                    .ok_or_else(|| JmapError::malformed("id array element is not an integer"))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(LocalIds)
    }

    pub(crate) fn as_slice(&self) -> &[LocalId] {
        &self.0
    }

    fn to_target(
        &self,
        ty: ObjectType,
        resolve: &impl Fn(ObjectType, LocalId) -> Option<JmapId>,
    ) -> Result<Value, JmapError> {
        let mut out = Map::new();
        for &local in &self.0 {
            let Some(target) = resolve(ty, local) else {
                return Err(JmapError::malformed(format!(
                    "unresolved local {} id {local} on export",
                    ty.jmap_name()
                )));
            };
            out.insert(target.0, Value::Bool(true));
        }
        Ok(Value::Object(out))
    }
}

struct UtcText;

impl UtcText {
    fn parse(text: &str) -> Result<UtcDate, JmapError> {
        UtcDate::parse(text, &Rfc3339)
            .map_err(|e| JmapError::malformed(format!("cannot parse date {text}: {e}")))
    }

    fn parse_opt(text: Option<String>) -> Result<Option<UtcDate>, JmapError> {
        text.as_deref().map(UtcText::parse).transpose()
    }

    fn format(value: &UtcDate) -> Result<String, JmapError> {
        value
            .format(&Rfc3339)
            .map_err(|e| JmapError::malformed(format!("cannot format date: {e}")))
    }

    fn format_opt(value: Option<&UtcDate>) -> Result<Option<String>, JmapError> {
        value.map(UtcText::format).transpose()
    }
}

struct JsonColumn;

impl JsonColumn {
    fn encode<T: Serialize>(value: Option<&T>) -> Result<Option<String>, JmapError> {
        Ok(value.map(serde_json::to_string).transpose()?)
    }

    fn decode<T: DeserializeOwned>(text: Option<String>) -> Result<Option<T>, JmapError> {
        Ok(text.as_deref().map(serde_json::from_str).transpose()?)
    }
}

struct KeywordNames<'a>(&'a IndexMap<String, bool>);

impl KeywordNames<'_> {
    fn to_json(&self) -> Result<String, JmapError> {
        Ok(serde_json::to_string(self)?)
    }
}

impl Serialize for KeywordNames<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.0.keys())
    }
}

struct JsData(Value);

impl JsData {
    fn from_rest(rest: IndexMap<String, Value>) -> JsData {
        JsData(Value::Object(rest.into_iter().collect()))
    }

    fn parse(text: &str) -> Result<JsData, JmapError> {
        Ok(JsData(serde_json::from_str(text)?))
    }

    fn take_string(&mut self, key: &str) -> Option<String> {
        match self.0.as_object_mut()?.remove(key) {
            Some(Value::String(s)) => Some(s),
            _ => None,
        }
    }

    fn remove_all(&mut self, keys: &[&str]) {
        if let Value::Object(map) = &mut self.0 {
            for key in keys {
                map.remove(*key);
            }
        }
    }

    fn insert(&mut self, key: &str, value: Value) {
        if let Value::Object(map) = &mut self.0 {
            map.insert(key.to_owned(), value);
        }
    }

    fn intern_blobs(
        &mut self,
        intern: &mut impl FnMut(&str) -> Result<BlobId, JmapError>,
    ) -> Result<(), JmapError> {
        BlobRefs(&mut self.0)
            .to_sentinels(|jmap_blob_id| {
                intern(jmap_blob_id)
                    .map(BlobId::get)
                    .map_err(BlobWalkError::resolver)
            })
            .map_err(BlobWalkError::into_source)
    }

    fn inline_blobs(
        &mut self,
        shape: InlineShape,
        bytes: &impl Fn(BlobId) -> Result<Vec<u8>, JmapError>,
    ) -> Result<(), JmapError> {
        BlobRefs(&mut self.0)
            .to_data_uris(shape, |local_id| {
                bytes(BlobId::from_raw(local_id)).map_err(BlobWalkError::resolver)
            })
            .map_err(BlobWalkError::into_source)
    }

    fn text(&self) -> String {
        self.0.to_string()
    }
}
