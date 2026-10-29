/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

#![allow(dead_code, unused_imports)]

pub mod baikal;
pub mod cyrus;
pub mod data;
pub mod dav_client;
pub mod dovecot;
pub mod error;
pub mod imap_client;
pub mod layouts;
pub mod radicale;
pub mod sieve_client;
pub mod stalwart;
pub mod validate;
pub mod webdav;

pub use error::{ContainerError, ContainerResult};

pub const OWNER_LABEL: &str = "art.stalw.vandelay.itest";

use std::sync::Once;

static CRYPTO_INIT: Once = Once::new();

pub fn install_crypto_provider() {
    CRYPTO_INIT.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

impl Endpoint {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }

    pub fn http_base(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }
}

#[derive(Debug, Clone)]
pub struct Account {
    pub username: String,
    pub password: String,
    pub layout: layouts::Layout,
}
