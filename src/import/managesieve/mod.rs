/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod control;
mod plan;
mod run;

use crate::archive::sources::SourceChange;
use crate::net::stream::Cleartext;

pub use crate::net::sasl::SaslAuth;

pub struct ManageSieveImport {
    pub url: String,
    pub auth: SaslAuth,
    pub cleartext: Cleartext,
    pub source_change: SourceChange,
}
