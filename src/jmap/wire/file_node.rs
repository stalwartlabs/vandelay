/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use super::common::Lenient;
use super::{JmapId, UtcDate};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileNode {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<JmapId>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<JmapId>,

    pub name: String,

    #[serde(with = "time::serde::rfc3339")]
    pub created: UtcDate,

    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub modified: Option<UtcDate>,

    #[serde(flatten)]
    pub kind: NodeKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "@type")]
pub enum NodeKind {
    File(FileProps),
    Directory(DirectoryProps),
    SymLink(SymLinkProps),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileProps {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_id: Option<JmapId>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,

    #[serde(default, deserialize_with = "Lenient::bool_or_false")]
    pub executable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryProps {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,

    #[serde(
        default = "Lenient::default_true",
        deserialize_with = "Lenient::bool_or_true"
    )]
    pub is_subscribed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymLinkProps {
    pub target: Vec<String>,
}
