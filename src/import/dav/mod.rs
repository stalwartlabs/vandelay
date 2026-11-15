/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod collections;
pub mod coordinator;
mod items;
pub mod replace;
pub mod tree;

pub use coordinator::{DavImport, DavKindArg};

const MAX_DAV_CONNECTIONS: usize = 8;
