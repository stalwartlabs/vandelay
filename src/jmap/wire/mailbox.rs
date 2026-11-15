/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use serde::{Deserialize, Serialize};

use super::JmapId;
use super::common::Lenient;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mailbox {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<JmapId>,

    pub name: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<JmapId>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,

    #[serde(default)]
    pub sort_order: u32,

    #[serde(default = "Lenient::default_true")]
    pub is_subscribed: bool,
}
