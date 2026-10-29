/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

pub mod calendar;
pub mod contacts;
pub mod coordinator;
pub mod files;
pub mod folders;
pub mod messages;

pub use coordinator::{GraphAuth, GraphImportConfig, run};
