/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use serde::{Deserialize, Serialize};

pub const TYPE_NAME: &str = "VacationResponse";

pub const SINGLETON_ID: &str = "singleton";

pub const RESERVED_SCRIPT_NAME: &str = "vacation";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VacationResponse {
    #[serde(default)]
    pub is_enabled: bool,

    #[serde(default)]
    pub from_date: Option<String>,

    #[serde(default)]
    pub to_date: Option<String>,

    #[serde(default)]
    pub subject: Option<String>,

    #[serde(default)]
    pub text_body: Option<String>,

    #[serde(default)]
    pub html_body: Option<String>,
}

impl VacationResponse {
    pub fn is_unset(&self) -> bool {
        !self.is_enabled
            && self.from_date.is_none()
            && self.to_date.is_none()
            && self.subject.is_none()
            && self.text_body.is_none()
            && self.html_body.is_none()
    }

    pub fn is_reserved_script_name(name: &str) -> bool {
        name.eq_ignore_ascii_case(RESERVED_SCRIPT_NAME)
    }
}
