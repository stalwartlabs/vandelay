/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;
use std::time::Duration;

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::{SyncBuilder, SyncRunner};
use testcontainers::{Container, GenericBuildableImage, GenericImage, ImageExt};

use super::data::{MboxMessage, load_mbox};
use super::error::{ContainerError, ContainerResult};
use super::imap_client::{ImapSeed, full_path};
use super::layouts::{self};
use super::sieve_client::SieveSeed as SieveClient;
use super::{Account, Endpoint};

const IMAP_PORT: u16 = 143;
const SIEVE_PORT: u16 = 4190;

const DOCKERFILE: &str = r#"FROM debian:bookworm-20260518-slim

RUN apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        dovecot-imapd dovecot-managesieved dovecot-pop3d dovecot-sieve \
        openssl ca-certificates && \
    rm -rf /var/lib/apt/lists/* && \
    addgroup --gid 5000 vmail && \
    adduser --system --no-create-home --uid 5000 --gid 5000 vmail && \
    mkdir -p /var/vmail && chown -R vmail:vmail /var/vmail && \
    openssl req -x509 -nodes -days 365 -newkey rsa:2048 \
        -keyout /etc/dovecot/dovecot.key \
        -out /etc/dovecot/dovecot.crt \
        -subj /CN=localhost && \
    chmod 600 /etc/dovecot/dovecot.key

COPY dovecot.conf /etc/dovecot/dovecot.conf
COPY users /etc/dovecot/users

EXPOSE 143 4190
CMD ["dovecot", "-F"]
"#;

const DOVECOT_CONF: &str = r#"protocols = imap sieve
listen = *

mail_location = maildir:/var/vmail/%u/Maildir
mail_uid = 5000
mail_gid = 5000
mail_privileged_group = vmail

namespace inbox {
  inbox = yes
  separator = /
  prefix =
}

auth_mechanisms = plain login
disable_plaintext_auth = no

passdb {
  driver = passwd-file
  args = /etc/dovecot/users
}
userdb {
  driver = static
  args = uid=5000 gid=5000 home=/var/vmail/%u
}

service imap-login {
  inet_listener imap {
    port = 143
  }
}

service managesieve-login {
  inet_listener sieve {
    port = 4190
  }
}

protocol sieve {
  managesieve_max_line_length = 1M
  managesieve_max_compile_errors = 5
}

ssl = yes
ssl_cert = </etc/dovecot/dovecot.crt
ssl_key = </etc/dovecot/dovecot.key
log_path = /dev/stderr
info_log_path = /dev/stderr

plugin {
  sieve = file:~/sieve;active=~/.dovecot.sieve
  sieve_max_script_size = 1M
}
"#;

fn users_file() -> String {
    let mut out = String::new();
    for u in layouts::accounts() {
        out.push_str(u);
        out.push_str(":{plain}");
        out.push_str(layouts::PASSWORD);
        out.push_str(":5000:5000::/var/vmail/");
        out.push_str(u);
        out.push_str("::\n");
    }
    out
}

pub struct Dovecot {
    container: Container<GenericImage>,
    pub imap: Endpoint,
    pub sieve: Endpoint,
    pub accounts: Vec<Account>,
}

impl Dovecot {
    pub fn start() -> ContainerResult<Self> {
        let image: GenericImage = GenericBuildableImage::new("vandelay-dovecot", "test")
            .with_dockerfile_string(DOCKERFILE.to_owned())
            .with_data(DOVECOT_CONF.as_bytes().to_vec(), "dovecot.conf")
            .with_data(users_file().into_bytes(), "users")
            .build_image()
            .map_err(|e| ContainerError::Seed(format!("dovecot build: {e}")))?;

        let request = image
            .with_exposed_port(IMAP_PORT.tcp())
            .with_exposed_port(SIEVE_PORT.tcp())
            .with_wait_for(WaitFor::message_on_stderr("starting up"))
            .with_startup_timeout(Duration::from_secs(120))
            .with_labels([(super::OWNER_LABEL, "1")]);

        let container = request.start()?;
        let host = container.get_host()?.to_string();
        let imap = Endpoint::new(host.clone(), container.get_host_port_ipv4(IMAP_PORT.tcp())?);
        let sieve = Endpoint::new(host, container.get_host_port_ipv4(SIEVE_PORT.tcp())?);

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
            sieve,
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
        let mut out = Vec::new();
        for acct in &self.accounts {
            let mailbox = self.seed_imap(acct, &messages)?;
            let sieve = self.seed_sieve(acct)?;
            out.push(AccountSeed {
                username: acct.username.clone(),
                mailbox,
                sieve,
            });
        }
        Ok(out)
    }

    fn seed_imap(
        &self,
        account: &Account,
        messages: &[MboxMessage],
    ) -> ContainerResult<MailboxSeed> {
        let mut client = ImapSeed::connect(&self.imap.host, self.imap.port)?;
        client.login(&account.username, &account.password)?;
        let sep = client.discover_separator().unwrap_or('/');

        let mut paths = Vec::new();
        for spec in account.layout.mailboxes {
            let path = full_path(account.layout.mailboxes, spec.key, sep).ok_or_else(|| {
                ContainerError::Seed(format!("missing mailbox key: {}", spec.key))
            })?;
            client.create(&path)?;
            client.subscribe(&path)?;
            paths.push(path);
        }

        let mut targets: Vec<String> = vec!["INBOX".to_owned()];
        targets.extend(paths.iter().cloned());
        let mut histogram: HashMap<String, usize> =
            targets.iter().map(|t| (t.clone(), 0)).collect();
        let mut appends: Vec<SeededAppend> = Vec::new();

        let total = account.layout.email_count.min(messages.len());
        if total < account.layout.email_count {
            return Err(ContainerError::Seed(format!(
                "{} only has {} messages but layout requires {}",
                account.username,
                messages.len(),
                account.layout.email_count
            )));
        }
        for (i, msg) in messages.iter().take(total).enumerate() {
            let target = targets[i % targets.len()].clone();
            client.append_with_flags(&target, &[], &msg.raw)?;
            *histogram.entry(target.clone()).or_insert(0) += 1;
            appends.push(SeededAppend {
                raw: msg.raw.clone(),
                target,
                flags: Vec::new(),
                tag: SeedTag::Bulk(i),
            });
        }

        let mut dedup_target: Option<String> = None;
        if targets.len() > 1 && !messages.is_empty() {
            let target = targets[1].clone();
            client.append_with_flags(&target, &[], &messages[0].raw)?;
            *histogram.entry(target.clone()).or_insert(0) += 1;
            appends.push(SeededAppend {
                raw: messages[0].raw.clone(),
                target: target.clone(),
                flags: Vec::new(),
                tag: SeedTag::Dedup,
            });
            dedup_target = Some(target);
        }

        let mut flagged_target: Option<String> = None;
        if !messages.is_empty() {
            let target = "INBOX".to_owned();
            let probe = flag_probe_message();
            client.append_with_flags(&target, &["\\Seen", "\\Flagged"], &probe)?;
            *histogram.entry(target.clone()).or_insert(0) += 1;
            appends.push(SeededAppend {
                raw: probe,
                target: target.clone(),
                flags: vec!["$seen".to_owned(), "$flagged".to_owned()],
                tag: SeedTag::FlagProbe,
            });
            flagged_target = Some(target);
        }

        let mut nomid_target: Option<String> = None;
        if !messages.is_empty() {
            let target = "INBOX".to_owned();
            let probe = no_message_id_probe();
            client.append_with_flags(&target, &[], &probe)?;
            *histogram.entry(target.clone()).or_insert(0) += 1;
            appends.push(SeededAppend {
                raw: probe,
                target: target.clone(),
                flags: Vec::new(),
                tag: SeedTag::NoMessageId,
            });
            nomid_target = Some(target);
        }

        let mut mid_dedup_targets: Option<(String, String)> = None;
        if targets.len() >= 3 {
            let body = shared_message_id_probe();
            let a = targets[1].clone();
            let b = targets[2].clone();
            client.append_with_flags(&a, &[], &body)?;
            client.append_with_flags(&b, &[], &body)?;
            *histogram.entry(a.clone()).or_insert(0) += 1;
            *histogram.entry(b.clone()).or_insert(0) += 1;
            appends.push(SeededAppend {
                raw: body.clone(),
                target: a.clone(),
                flags: Vec::new(),
                tag: SeedTag::SharedMid,
            });
            appends.push(SeededAppend {
                raw: body,
                target: b.clone(),
                flags: Vec::new(),
                tag: SeedTag::SharedMid,
            });
            mid_dedup_targets = Some((a, b));
        }

        client.logout()?;

        let extras = ExtraAppends {
            dedup_target,
            flagged_target,
            nomid_target,
            mid_dedup_targets,
        };

        let total_appends = histogram_total(&histogram);
        Ok(MailboxSeed {
            paths,
            histogram,
            total_appends,
            extras,
            appends,
        })
    }

    fn seed_sieve(&self, account: &Account) -> ContainerResult<SieveSeed> {
        if account.layout.sieve_scripts.is_empty() {
            return Ok(SieveSeed::default());
        }
        let mut client = SieveClient::connect_seed(&self.sieve.host, self.sieve.port)?;
        client.authenticate(&account.username, &account.password)?;
        let mut active_name: Option<&'static str> = None;
        let mut names = Vec::new();
        for script in account.layout.sieve_scripts {
            client.putscript(script.name, script.body)?;
            names.push(script.name.to_owned());
            if script.active {
                active_name = Some(script.name);
            }
        }
        if let Some(name) = active_name {
            client.setactive(name)?;
        }
        client.logout()?;
        Ok(SieveSeed {
            names,
            active: active_name.map(str::to_owned),
        })
    }

    pub fn create_non_ascii_mailbox(
        &self,
        account: &Account,
        wire_name: &str,
        message: &[u8],
    ) -> ContainerResult<()> {
        let mut client = ImapSeed::connect(&self.imap.host, self.imap.port)?;
        client.login(&account.username, &account.password)?;
        client.create(wire_name)?;
        client.subscribe(wire_name)?;
        client.append_with_flags(wire_name, &[], message)?;
        client.logout()?;
        Ok(())
    }

    pub fn delete_first_inbox_message(&self, account: &Account) -> ContainerResult<()> {
        let mut client = ImapSeed::connect(&self.imap.host, self.imap.port)?;
        client.login(&account.username, &account.password)?;
        client.delete_and_expunge_first("INBOX")?;
        client.logout()?;
        Ok(())
    }

    pub fn append_new_message(
        &self,
        account: &Account,
        mailbox: &str,
        tag: &str,
    ) -> ContainerResult<(Vec<u8>, String)> {
        let mut client = ImapSeed::connect(&self.imap.host, self.imap.port)?;
        client.login(&account.username, &account.password)?;
        let message_id = format!("<added-{tag}-{}@vandelay.test>", account.username);
        let body = format!(
            "From: added-{tag}@vandelay.test\r\n\
             To: {}@vandelay.test\r\n\
             Subject: Added probe {tag}\r\n\
             Message-ID: {message_id}\r\n\
             Date: Wed, 01 Jan 2025 12:00:00 +0000\r\n\
             \r\n\
             Added probe body {tag}.\r\n",
            account.username
        );
        let raw = body.into_bytes();
        client.append_with_flags(mailbox, &[], &raw)?;
        client.logout()?;
        Ok((raw, message_id))
    }

    pub fn install_broken_sieve(&self, account: &Account) -> ContainerResult<String> {
        let mut client = SieveClient::connect_seed(&self.sieve.host, self.sieve.port)?;
        client.authenticate(&account.username, &account.password)?;
        let name = "broken-script";
        client.putscript_raw(name, "INVALID:::not_a_sieve_program;;;")?;
        client.logout()?;
        Ok(name.to_owned())
    }

    pub fn verify_seed(&self, seeds: &[AccountSeed]) -> ContainerResult<()> {
        for (acct, seed) in self.accounts.iter().zip(seeds) {
            let mut client = ImapSeed::connect(&self.imap.host, self.imap.port)?;
            client.login(&acct.username, &acct.password)?;
            let names = client.list_all()?;
            let expected = acct.layout.mailboxes.len() + 1;
            if names.len() < expected {
                return Err(ContainerError::Seed(format!(
                    "{}: expected >= {expected} mailboxes, got {}",
                    acct.username,
                    names.len()
                )));
            }
            let inbox_n = client.select("INBOX")?;
            if acct.layout.email_count > 0 && inbox_n == 0 {
                return Err(ContainerError::Seed(format!(
                    "{}: INBOX EXISTS = 0 after seed",
                    acct.username
                )));
            }
            for path in &seed.mailbox.paths {
                let n = client.select(path)?;
                let want = seed.mailbox.histogram.get(path).copied().unwrap_or(0);
                if n < want {
                    return Err(ContainerError::Seed(format!(
                        "{}: mailbox {path} EXISTS={n} but {want} were appended",
                        acct.username
                    )));
                }
            }
            client.logout()?;
        }
        Ok(())
    }

    pub fn stop(self) -> ContainerResult<()> {
        self.container.stop()?;
        Ok(())
    }
}

fn histogram_total(h: &HashMap<String, usize>) -> usize {
    h.values().sum()
}

pub fn flag_probe_message() -> Vec<u8> {
    let body = "From: probe-flags@vandelay.test\r\n\
                To: user@vandelay.test\r\n\
                Subject: Flag probe\r\n\
                Message-ID: <flag-probe@vandelay.test>\r\n\
                Date: Wed, 01 Jan 2025 12:00:00 +0000\r\n\
                \r\n\
                Flag probe body.\r\n";
    body.as_bytes().to_vec()
}

pub fn no_message_id_probe() -> Vec<u8> {
    let body = "From: nomid@vandelay.test\r\n\
                To: user@vandelay.test\r\n\
                Subject: No Message-ID probe\r\n\
                Date: Wed, 01 Jan 2025 12:00:00 +0000\r\n\
                \r\n\
                No Message-ID body.\r\n";
    body.as_bytes().to_vec()
}

pub fn shared_message_id_probe() -> Vec<u8> {
    let body = "From: shared@vandelay.test\r\n\
                To: user@vandelay.test\r\n\
                Subject: Shared MID probe\r\n\
                Message-ID: <shared-mid@vandelay.test>\r\n\
                Date: Wed, 01 Jan 2025 12:00:00 +0000\r\n\
                \r\n\
                Shared MID body.\r\n";
    body.as_bytes().to_vec()
}

#[derive(Debug, Clone)]
pub struct MailboxSeed {
    pub paths: Vec<String>,
    pub histogram: HashMap<String, usize>,
    pub total_appends: usize,
    pub extras: ExtraAppends,
    pub appends: Vec<SeededAppend>,
}

#[derive(Debug, Clone)]
pub struct SeededAppend {
    pub raw: Vec<u8>,
    pub target: String,
    pub flags: Vec<String>,
    pub tag: SeedTag,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedTag {
    Bulk(usize),
    Dedup,
    FlagProbe,
    NoMessageId,
    SharedMid,
}

#[derive(Debug, Clone)]
pub struct ExtraAppends {
    pub dedup_target: Option<String>,
    pub flagged_target: Option<String>,
    pub nomid_target: Option<String>,
    pub mid_dedup_targets: Option<(String, String)>,
}

#[derive(Debug, Default, Clone)]
pub struct SieveSeed {
    pub names: Vec<String>,
    pub active: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AccountSeed {
    pub username: String,
    pub mailbox: MailboxSeed,
    pub sieve: SieveSeed,
}
