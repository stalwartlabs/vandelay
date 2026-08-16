/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::net::TcpListener;
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::Value;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};
use ureq::Agent;
use ureq::config::RedirectAuthHeaders;
use ureq::tls::{TlsConfig, TlsProvider};

use super::OWNER_LABEL;
use super::error::{ContainerError, ContainerResult};

const IMAGE_NAME: &str = "stalwartlabs/stalwart";
const IMAGE_TAG: &str = "latest";

const HTTPS_PORT: u16 = 443;
const IMAP_PORT: u16 = 143;
const IMAPS_PORT: u16 = 993;
const SIEVE_PORT: u16 = 4190;

const CONFIG_PATH: &str = "/etc/stalwart/config.json";
const CONFIG_JSON: &str = r#"{"@type":"RocksDb","blobSize":16834,"bufferSize":134217728,"path":"/var/lib/stalwart","poolWorkers":null}"#;

pub const ADMIN_USER: &str = "admin";
pub const ADMIN_PASSWORD: &str = "admin";

pub struct Stalwart {
    _container: Container<GenericImage>,
    pub host: String,
    pub https_port: u16,
    pub imap_port: u16,
    pub imaps_port: u16,
    pub sieve_port: u16,
    pub public_url: String,
}

impl Stalwart {
    pub fn start() -> ContainerResult<Self> {
        sweep_abandoned_containers();
        let https_port = pick_free_port()?;
        let imap_port = pick_free_port()?;
        let imaps_port = pick_free_port()?;
        let sieve_port = pick_free_port()?;

        let host = "127.0.0.1".to_owned();
        let public_url = format!("https://{host}:{https_port}");

        let image = GenericImage::new(IMAGE_NAME, IMAGE_TAG)
            .with_exposed_port(HTTPS_PORT.tcp())
            .with_exposed_port(IMAP_PORT.tcp())
            .with_exposed_port(IMAPS_PORT.tcp())
            .with_exposed_port(SIEVE_PORT.tcp())
            .with_wait_for(WaitFor::seconds(2));

        let request = image
            .with_env_var("STALWART_PUBLIC_URL", &public_url)
            .with_env_var("STALWART_RECOVERY_ADMIN", "admin:admin")
            .with_labels([(OWNER_LABEL, "1")])
            .with_copy_to(CONFIG_PATH, CONFIG_JSON.as_bytes().to_vec())
            .with_mapped_port(https_port, HTTPS_PORT.tcp())
            .with_mapped_port(imap_port, IMAP_PORT.tcp())
            .with_mapped_port(imaps_port, IMAPS_PORT.tcp())
            .with_mapped_port(sieve_port, SIEVE_PORT.tcp())
            .with_startup_timeout(Duration::from_secs(180));

        let container = request.start()?;

        let me = Self {
            _container: container,
            host,
            https_port,
            imap_port,
            imaps_port,
            sieve_port,
            public_url,
        };

        me.wait_ready(Duration::from_secs(120))?;
        Ok(me)
    }

    pub fn base_url(&self) -> &str {
        &self.public_url
    }

    pub fn fetch_jmap_session(&self) -> ContainerResult<Value> {
        let agent = build_agent();
        let auth = basic(ADMIN_USER, ADMIN_PASSWORD);
        let url = format!("{}/.well-known/jmap", self.public_url);
        let mut resp = agent.get(&url).header("Authorization", &auth).call()?;
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| ContainerError::Protocol(format!("body read: {e}")))?;
        if status != 200 {
            return Err(ContainerError::Protocol(format!(
                "jmap session status {status}: {text}"
            )));
        }
        serde_json::from_str(&text)
            .map_err(|e| ContainerError::Protocol(format!("jmap session parse: {e}")))
    }

    fn wait_ready(&self, total: Duration) -> ContainerResult<()> {
        let deadline = Instant::now() + total;
        let mut last_err = String::from("no probe attempted");
        while Instant::now() < deadline {
            match self.fetch_jmap_session() {
                Ok(v) if v.get("apiUrl").is_some() => return Ok(()),
                Ok(v) => last_err = format!("session missing apiUrl: {v}"),
                Err(e) => last_err = e.to_string(),
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        Err(ContainerError::Protocol(format!(
            "stalwart did not become ready in {total:?}: {last_err}"
        )))
    }
}

static SHARED: OnceLock<Stalwart> = OnceLock::new();

pub fn shared() -> &'static Stalwart {
    SHARED.get_or_init(|| Stalwart::start().expect("start shared stalwart container"))
}

fn sweep_abandoned_containers() {
    let Ok(listed) = Command::new("docker")
        .args(["ps", "-aq", "--filter", &format!("label={OWNER_LABEL}")])
        .output()
    else {
        return;
    };
    let ids: Vec<String> = String::from_utf8_lossy(&listed.stdout)
        .split_whitespace()
        .map(String::from)
        .collect();
    if ids.is_empty() {
        return;
    }
    eprintln!("removing {} abandoned test container(s)", ids.len());
    let _ = Command::new("docker")
        .args(["rm", "-f"])
        .args(&ids)
        .output();
}

fn pick_free_port() -> ContainerResult<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

fn build_agent() -> Agent {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let tls = TlsConfig::builder()
        .provider(TlsProvider::Rustls)
        .unversioned_rustls_crypto_provider(provider)
        .disable_verification(true)
        .build();
    Agent::config_builder()
        .tls_config(tls)
        .http_status_as_error(false)
        .max_redirects(10)
        .redirect_auth_headers(RedirectAuthHeaders::SameHost)
        .build()
        .new_agent()
}

fn basic(user: &str, password: &str) -> String {
    let mut raw = String::with_capacity(user.len() + password.len() + 1);
    raw.push_str(user);
    raw.push(':');
    raw.push_str(password);
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(raw.as_bytes())
    )
}
