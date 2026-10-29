/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

pub mod coordinator;
pub mod keywords;
pub mod messages;
pub mod tree;

pub use coordinator::{MaildirImportConfig, run, run_reporting};
