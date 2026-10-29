/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::time::Duration;

use encodify::base64::STANDARD as B64;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::{SyncBuilder, SyncRunner};
use testcontainers::{Container, GenericBuildableImage, GenericImage, ImageExt};

use super::dav_client::DavSeed;
use super::error::{ContainerError, ContainerResult};
use super::layouts::{self, FileSpec};
use super::{Account, Endpoint};

const HTTP_PORT: u16 = 80;

const DOCKERFILE: &str = r#"FROM debian:bookworm-20260518-slim

RUN apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        apache2 apache2-utils ca-certificates && \
    a2enmod dav dav_fs auth_basic authn_file && \
    rm -rf /var/lib/apt/lists/*

COPY apache.conf /etc/apache2/sites-available/000-default.conf
COPY htpasswd /etc/apache2/htpasswd

RUN mkdir -p /var/dav && \
    chown -R www-data:www-data /var/dav

EXPOSE 80
CMD ["apachectl", "-D", "FOREGROUND"]
"#;

const APACHE_CONF: &str = r#"DavLockDB /tmp/DavLock

<VirtualHost *:80>
    DocumentRoot /var/dav
    ErrorLog /dev/stderr
    CustomLog /dev/stdout combined

    <Directory /var/dav>
        Dav On
        AuthType Basic
        AuthName "WebDAV"
        AuthUserFile /etc/apache2/htpasswd
        Require valid-user
        Options Indexes FollowSymLinks
    </Directory>
</VirtualHost>
"#;

fn htpasswd_for_test() -> String {
    let mut out = String::new();
    for u in layouts::accounts() {
        out.push_str(u);
        out.push(':');
        out.push_str("{SHA}");
        out.push_str(&sha1_b64(layouts::PASSWORD.as_bytes()));
        out.push('\n');
    }
    out
}

fn sha1_b64(bytes: &[u8]) -> String {
    let digest = simple_sha1(bytes);
    B64.encode(digest)
}

fn simple_sha1(bytes: &[u8]) -> [u8; 20] {
    let mut h0: u32 = 0x6745_2301;
    let mut h1: u32 = 0xEFCD_AB89;
    let mut h2: u32 = 0x98BA_DCFE;
    let mut h3: u32 = 0x1032_5476;
    let mut h4: u32 = 0xC3D2_E1F0;

    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    let mut msg = bytes.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h0, h1, h2, h3, h4);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k): (u32, u32) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }
    let mut out = [0u8; 20];
    out[..4].copy_from_slice(&h0.to_be_bytes());
    out[4..8].copy_from_slice(&h1.to_be_bytes());
    out[8..12].copy_from_slice(&h2.to_be_bytes());
    out[12..16].copy_from_slice(&h3.to_be_bytes());
    out[16..20].copy_from_slice(&h4.to_be_bytes());
    out
}

pub struct WebDav {
    container: Container<GenericImage>,
    pub http: Endpoint,
    pub accounts: Vec<Account>,
}

impl WebDav {
    pub fn start() -> ContainerResult<Self> {
        let image: GenericImage = GenericBuildableImage::new("vandelay-webdav", "test")
            .with_dockerfile_string(DOCKERFILE.to_owned())
            .with_data(APACHE_CONF.as_bytes().to_vec(), "apache.conf")
            .with_data(htpasswd_for_test().into_bytes(), "htpasswd")
            .build_image()
            .map_err(|e| ContainerError::Seed(format!("webdav build: {e}")))?;

        let request = image
            .with_exposed_port(HTTP_PORT.tcp())
            .with_wait_for(WaitFor::message_on_stderr("AH00558"))
            .with_startup_timeout(Duration::from_secs(120))
            .with_labels([(super::OWNER_LABEL, "1")]);

        let container = request.start()?;
        let host = container.get_host()?.to_string();
        let http = Endpoint::new(host, container.get_host_port_ipv4(HTTP_PORT.tcp())?);

        let accounts: Vec<Account> = layouts::accounts()
            .iter()
            .map(|name| Account {
                username: (*name).to_owned(),
                password: layouts::PASSWORD.to_owned(),
                layout: layouts::layout_for(name),
            })
            .collect();

        Ok(Self {
            container,
            http,
            accounts,
        })
    }

    pub fn base_url(&self) -> String {
        self.http.http_base()
    }

    pub fn account_url(&self, account: &Account) -> String {
        format!("{}/{}/", self.base_url(), account.username)
    }

    pub fn seed_all(&self) -> ContainerResult<Vec<AccountSeed>> {
        let mut out = Vec::new();
        for acct in &self.accounts {
            let m = self.seed_account(acct)?;
            out.push(m);
        }
        Ok(out)
    }

    pub fn seed_account(&self, account: &Account) -> ContainerResult<AccountSeed> {
        let client = DavSeed::new(self.base_url(), &account.username, &account.password);
        let root = format!("/{}/", account.username);
        client.mkcol(&root, None)?;
        let mut seed = AccountSeed::new(account.username.clone());

        let specs = account.layout.files;
        for spec in specs {
            let path = build_path(&root, specs, spec.key)
                .ok_or_else(|| ContainerError::Seed(format!("missing file key: {}", spec.key)))?;
            if spec.directory {
                client.mkcol(&path, None)?;
                seed.directories += 1;
            } else {
                let payload = synth_payload(spec.name);
                client.put(&path, "application/octet-stream", payload.as_bytes())?;
                seed.files.push(SeededFile {
                    key: spec.key.to_owned(),
                    name: spec.name.to_owned(),
                    href: path,
                    payload: payload.into_bytes(),
                });
            }
        }
        Ok(seed)
    }

    pub fn delete_resource(&self, account: &Account, href: &str) -> ContainerResult<()> {
        let client = DavSeed::new(self.base_url(), &account.username, &account.password);
        client.delete(href)?;
        Ok(())
    }

    pub fn add_file(
        &self,
        account: &Account,
        parent_segments: &[&str],
        name: &str,
        payload: &[u8],
    ) -> ContainerResult<String> {
        let client = DavSeed::new(self.base_url(), &account.username, &account.password);
        let mut href = format!("/{}/", account.username);
        for seg in parent_segments {
            href.push_str(seg);
            href.push('/');
        }
        href.push_str(name);
        client.put(&href, "application/octet-stream", payload)?;
        Ok(href)
    }

    pub fn verify_seed(&self) -> ContainerResult<()> {
        for acct in &self.accounts {
            let client = DavSeed::new(self.base_url(), &acct.username, &acct.password);
            let body = client.propfind(&format!("/{}/", acct.username), 1)?;
            if !body.contains("multistatus") {
                return Err(ContainerError::Protocol(format!(
                    "webdav propfind for {} returned no multistatus",
                    acct.username
                )));
            }
        }
        Ok(())
    }

    pub fn stop(self) -> ContainerResult<()> {
        self.container.stop()?;
        Ok(())
    }
}

fn build_path(root: &str, specs: &[FileSpec], key: &str) -> Option<String> {
    let mut chain: Vec<&str> = Vec::new();
    let mut cur = key;
    let mut leaf_dir = false;
    loop {
        let spec = specs.iter().find(|s| s.key == cur)?;
        chain.push(spec.name);
        if chain.len() == 1 {
            leaf_dir = spec.directory;
        }
        match spec.parent {
            Some(p) => cur = p,
            None => break,
        }
    }
    chain.reverse();
    let mut path = String::from(root);
    for (i, name) in chain.iter().enumerate() {
        path.push_str(name);
        let is_last = i + 1 == chain.len();
        if !is_last || leaf_dir {
            path.push('/');
        }
    }
    Some(path)
}

pub fn synth_payload(name: &str) -> String {
    let mut out = String::new();
    out.push_str(name);
    out.push('\n');
    for i in 0..16 {
        out.push_str(&format!("line {i} for {name}\n"));
    }
    out
}

#[derive(Debug, Clone)]
pub struct SeededFile {
    pub key: String,
    pub name: String,
    pub href: String,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct AccountSeed {
    pub username: String,
    pub directories: usize,
    pub files: Vec<SeededFile>,
}

impl AccountSeed {
    fn new(username: String) -> Self {
        Self {
            username,
            directories: 0,
            files: Vec::new(),
        }
    }
}
