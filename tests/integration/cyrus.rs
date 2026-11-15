/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::time::Duration;

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::{SyncBuilder, SyncRunner};
use testcontainers::{Container, GenericBuildableImage, GenericImage, ImageExt};

use super::data::load_mbox;
use super::error::{ContainerError, ContainerResult};
use super::imap_client::{ImapSeed, MailboxSeed, SeedProbes};
use super::layouts::{self};
use super::{Account, Endpoint};

const IMAP_PORT: u16 = 143;

const DOCKERFILE: &str = r#"FROM debian:bookworm-20260518-slim

RUN apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        cyrus-imapd cyrus-admin cyrus-clients sasl2-bin \
        libsasl2-modules libsasl2-modules-db \
        ca-certificates && \
    rm -rf /var/lib/apt/lists/* && \
    mkdir -p /var/lib/cyrus /var/spool/cyrus /run/cyrus && \
    chown -R cyrus:mail /var/lib/cyrus /var/spool/cyrus /run/cyrus

COPY imapd.conf /etc/imapd.conf
COPY cyrus.conf /etc/cyrus.conf
COPY sasldb.txt /tmp/sasldb.txt
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

EXPOSE 143
CMD ["/entrypoint.sh"]
"#;

const IMAPD_CONF: &str = r#"configdirectory: /var/lib/cyrus
partition-default: /var/spool/cyrus
admins: cyrus
allowanonymouslogin: no
allowplaintext: yes
sasl_mech_list: PLAIN LOGIN
sasl_pwcheck_method: auxprop
sasl_auxprop_plugin: sasldb
sasl_sasldb_path: /etc/sasldb2
unixhierarchysep: yes
altnamespace: yes
sasl_minimum_layer: 0
"#;

const CYRUS_CONF: &str = r#"START {
  recover cmd="ctl_cyrusdb -r"
}

SERVICES {
  imap cmd="imapd" listen="143" prefork=0
}

EVENTS {
  checkpoint cmd="ctl_cyrusdb -c" period=30
  delprune cmd="cyr_expire -E 3" at=0400
}
"#;

fn sasldb_entries() -> String {
    let mut out = String::new();
    for u in layouts::accounts() {
        out.push_str(u);
        out.push(' ');
        out.push_str(layouts::PASSWORD);
        out.push('\n');
    }
    out.push_str("cyrus admin-secret\n");
    out
}

const ENTRYPOINT: &str = r#"#!/bin/bash
set -e

while IFS=' ' read -r user pw; do
  [ -z "$user" ] && continue
  echo "$pw" | saslpasswd2 -p -c -f /etc/sasldb2 "$user"
done < /tmp/sasldb.txt
chown cyrus:mail /etc/sasldb2
chmod 0640 /etc/sasldb2

mkdir -p /var/lib/cyrus /var/spool/cyrus /run/cyrus
chown -R cyrus:mail /var/lib/cyrus /var/spool/cyrus /run/cyrus
su cyrus -s /bin/bash -c '/usr/sbin/cyrus makedirs' || true

/usr/sbin/cyrmaster -C /etc/imapd.conf -M /etc/cyrus.conf &
master_pid=$!

for _ in $(seq 1 120); do
  if (exec 3<>/dev/tcp/127.0.0.1/143) 2>/dev/null; then
    exec 3<&- 3>&-
    break
  fi
  sleep 0.5
done

provision() {
  exec 3<>/dev/tcp/127.0.0.1/143
  read -r _greeting <&3
  printf 'a1 LOGIN cyrus admin-secret\r\n' >&3
  read -r _ <&3
  local tag=1
  for u in "$@"; do
    tag=$((tag + 1))
    printf 'a%d CREATE user/%s\r\n' "$tag" "$u" >&3
    read -r _ <&3
  done
  tag=$((tag + 1))
  printf 'a%d LOGOUT\r\n' "$tag" >&3
  read -r _ <&3
  exec 3<&- 3>&-
}

users=()
while IFS=' ' read -r user _pw; do
  [ -z "$user" ] && continue
  [ "$user" = "cyrus" ] && continue
  users+=("$user")
done < /tmp/sasldb.txt
provision "${users[@]}"

echo "cyrus provisioned"
wait "$master_pid"
"#;

pub struct Cyrus {
    container: Container<GenericImage>,
    pub imap: Endpoint,
    pub accounts: Vec<Account>,
}

impl Cyrus {
    pub fn start() -> ContainerResult<Self> {
        let image: GenericImage = GenericBuildableImage::new("vandelay-cyrus", "test")
            .with_dockerfile_string(DOCKERFILE.to_owned())
            .with_data(IMAPD_CONF.as_bytes().to_vec(), "imapd.conf")
            .with_data(CYRUS_CONF.as_bytes().to_vec(), "cyrus.conf")
            .with_data(sasldb_entries().into_bytes(), "sasldb.txt")
            .with_data(ENTRYPOINT.as_bytes().to_vec(), "entrypoint.sh")
            .build_image()
            .map_err(|e| ContainerError::Seed(format!("cyrus build: {e}")))?;

        let request = image
            .with_exposed_port(IMAP_PORT.tcp())
            .with_wait_for(WaitFor::message_on_stdout("cyrus provisioned"))
            .with_startup_timeout(Duration::from_secs(180))
            .with_labels([(super::OWNER_LABEL, "1")]);

        let container = request.start()?;
        let host = container.get_host()?.to_string();
        let imap = Endpoint::new(host, container.get_host_port_ipv4(IMAP_PORT.tcp())?);

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
            imap,
            accounts,
        })
    }

    pub fn seed_all(&self) -> ContainerResult<Vec<AccountSeed>> {
        let limit = self
            .accounts
            .iter()
            .map(|a| a.layout.email_count)
            .max()
            .unwrap_or(0)
            .max(1);
        let messages = load_mbox(limit)?;
        let probes = SeedProbes {
            flag: flag_probe_message(),
            no_message_id: None,
            shared_message_id: None,
        };
        let mut out = Vec::new();
        for acct in &self.accounts {
            let mut client = ImapSeed::open(&self.imap, acct)?;
            let mailbox = client.seed_layout(acct, &messages, &probes)?;
            client.logout()?;
            out.push(AccountSeed {
                username: acct.username.clone(),
                mailbox,
            });
        }
        Ok(out)
    }

    pub fn verify_seed(&self, seeds: &[AccountSeed]) -> ContainerResult<()> {
        for (acct, seed) in self.accounts.iter().zip(seeds) {
            let mut client = ImapSeed::open(&self.imap, acct)?;
            client.verify_layout(acct, &seed.mailbox, &format!("cyrus {}", acct.username))?;
            client.logout()?;
        }
        Ok(())
    }

    pub fn stop(self) -> ContainerResult<()> {
        self.container.stop()?;
        Ok(())
    }
}

pub fn flag_probe_message() -> Vec<u8> {
    let body = "From: probe-flags@vandelay.test\r\n\
                To: user@vandelay.test\r\n\
                Subject: Cyrus flag probe\r\n\
                Message-ID: <cyrus-flag-probe@vandelay.test>\r\n\
                Date: Wed, 01 Jan 2025 12:00:00 +0000\r\n\
                \r\n\
                Cyrus flag probe body.\r\n";
    body.as_bytes().to_vec()
}

#[derive(Debug, Clone)]
pub struct AccountSeed {
    pub username: String,
    pub mailbox: MailboxSeed,
}
