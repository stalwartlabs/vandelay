/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

pub mod automap;
pub mod client;
pub mod command;
pub mod error;
pub mod name;
pub mod response;
pub mod retry;
pub mod session;

pub use client::{Capabilities, Capability, CollectedResponse, ImapClient};
pub use error::ImapError;
