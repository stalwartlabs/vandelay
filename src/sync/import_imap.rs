/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

pub mod fetch;
pub mod folders;
pub mod internaldate;
pub mod keywords;
pub mod messages;
pub mod pool;

pub mod coordinator;

pub use coordinator::{ImapAuth, ImapImportConfig, run, run_reporting};
