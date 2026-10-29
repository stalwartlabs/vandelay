/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use super::common::lenient_utc_date;
use super::{JmapId, UtcDate};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Email {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<JmapId>,

    pub blob_id: JmapId,

    #[serde(
        default,
        deserialize_with = "lenient_utc_date",
        serialize_with = "time::serde::rfc3339::option::serialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub received_at: Option<UtcDate>,

    pub mailbox_ids: IndexMap<JmapId, bool>,

    pub keywords: IndexMap<String, bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmailImport {
    pub blob_id: JmapId,

    pub mailbox_ids: IndexMap<JmapId, bool>,

    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub keywords: IndexMap<String, bool>,

    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub received_at: Option<UtcDate>,
}
