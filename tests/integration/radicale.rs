/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashSet;
use std::time::{Duration, Instant};

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};

use super::data::{RawFixture, load_icals, load_vcards, malformed_ical, rewrite_uid};
use super::dav_client::DavSeed;
use super::error::{ContainerError, ContainerResult};
use super::layouts::{self, Layout};
pub use super::layouts::{AccountSeed, CollectionPlan, SeededItem};
use super::layouts::{
    collection_segment, contacts_offset, contacts_per_book, events_offset, events_per_calendar,
};

use super::{Account, Endpoint};

const IMAGE_NAME: &str = "tomsquest/docker-radicale";
const IMAGE_TAG: &str = "3.7.3.0";
const RADICALE_PORT: u16 = 5232;

const CONFIG: &str = "[server]
hosts = 0.0.0.0:5232

[auth]
type = htpasswd
htpasswd_filename = /config/users
htpasswd_encryption = plain

[storage]
filesystem_folder = /data/collections
";

pub struct Radicale {
    container: Container<GenericImage>,
    pub endpoint: Endpoint,
    pub accounts: Vec<Account>,
}

impl Radicale {
    pub fn start() -> ContainerResult<Self> {
        let mut users = String::new();
        for u in layouts::accounts() {
            users.push_str(u);
            users.push(':');
            users.push_str(layouts::PASSWORD);
            users.push('\n');
        }

        let image = GenericImage::new(IMAGE_NAME, IMAGE_TAG)
            .with_exposed_port(RADICALE_PORT.tcp())
            .with_wait_for(WaitFor::message_on_stderr("Radicale server ready"));

        let request = image
            .with_copy_to("/config/config", CONFIG.as_bytes().to_vec())
            .with_copy_to("/config/users", users.into_bytes())
            .with_startup_timeout(Duration::from_secs(90))
            .with_labels([(super::OWNER_LABEL, "1")]);

        let container = request.start()?;
        let host = container.get_host()?.to_string();
        let port = container.get_host_port_ipv4(RADICALE_PORT.tcp())?;

        wait_ready(&format!("http://{host}:{port}/"), Duration::from_secs(30))?;

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
            endpoint: Endpoint::new(host, port),
            accounts,
        })
    }

    pub fn base_url(&self) -> String {
        self.endpoint.http_base()
    }

    pub fn account_url(&self, account: &Account) -> String {
        format!("{}/{}/", self.base_url(), account.username)
    }

    pub fn seed_all(&self) -> ContainerResult<Vec<AccountSeed>> {
        let icals = load_icals()?;
        let vcards = load_vcards()?;
        let mut out = Vec::new();
        for acct in &self.accounts {
            let seed = self.seed_account(acct, &icals, &vcards)?;
            out.push(seed);
        }
        Ok(out)
    }

    pub fn seed_account(
        &self,
        account: &Account,
        icals: &[RawFixture],
        vcards: &[RawFixture],
    ) -> ContainerResult<AccountSeed> {
        let client = DavSeed::new(self.base_url(), &account.username, &account.password);
        let layout = &account.layout;
        let mut seed = AccountSeed::new(account.username.clone());

        for (idx, cal) in layout.calendars.iter().enumerate() {
            let collection = collection_segment("cal", idx, cal);
            let base = format!("/{}/{}/", account.username, collection);
            let body = format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<mkcol xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <set><prop>
    <resourcetype><collection/><c:calendar/></resourcetype>
    <displayname>{cal}</displayname>
  </prop></set>
</mkcol>"#
            );
            client.mkcol(&base, Some(&body))?;
            let mut plan = CollectionPlan::new((*cal).to_owned(), base.clone());

            let count = events_per_calendar(layout, idx);
            let offset = events_offset(layout, idx);
            for (i, fixture) in icals.iter().cycle().skip(offset).take(count).enumerate() {
                let path = format!("{base}{}-{}.ics", fixture.name, i);
                let suffix = format!("{collection}-{i}");
                let (bytes, uid) = rewrite_uid(&fixture.bytes, &suffix).ok_or_else(|| {
                    ContainerError::Seed(format!("ical fixture {} has no UID", fixture.name))
                })?;
                client.put(&path, "text/calendar; charset=utf-8", &bytes)?;
                plan.items.push(SeededItem {
                    uid,
                    href: path,
                    source: bytes,
                });
            }
            seed.calendars.push(plan);
        }

        for (idx, ab) in layout.address_books.iter().enumerate() {
            let collection = collection_segment("ab", idx, ab);
            let base = format!("/{}/{}/", account.username, collection);
            let body = format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<mkcol xmlns="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <set><prop>
    <resourcetype><collection/><card:addressbook/></resourcetype>
    <displayname>{ab}</displayname>
  </prop></set>
</mkcol>"#
            );
            client.mkcol(&base, Some(&body))?;
            let mut plan = CollectionPlan::new((*ab).to_owned(), base.clone());

            let count = contacts_per_book(layout, idx);
            let offset = contacts_offset(layout, idx);
            for (i, fixture) in vcards.iter().cycle().skip(offset).take(count).enumerate() {
                let path = format!("{base}{}-{}.vcf", fixture.name, i);
                let suffix = format!("{collection}-{i}");
                let (bytes, uid) = rewrite_uid(&fixture.bytes, &suffix).ok_or_else(|| {
                    ContainerError::Seed(format!("vcard fixture {} has no UID", fixture.name))
                })?;
                client.put(&path, "text/vcard; charset=utf-8", &bytes)?;
                plan.items.push(SeededItem {
                    uid,
                    href: path,
                    source: bytes,
                });
            }
            seed.address_books.push(plan);
        }

        Ok(seed)
    }

    pub fn seed_malformed_event(
        &self,
        account: &Account,
        calendar_segment_idx: usize,
        tag: &str,
    ) -> ContainerResult<String> {
        let client = DavSeed::new(self.base_url(), &account.username, &account.password);
        let collection = collection_segment(
            "cal",
            calendar_segment_idx,
            account.layout.calendars[calendar_segment_idx],
        );
        let path = format!("/{}/{}/broken-{tag}.ics", account.username, collection);
        let bad = malformed_ical(tag);
        client.put(&path, "text/calendar; charset=utf-8", &bad.bytes)?;
        Ok(path)
    }

    pub fn delete_item(&self, account: &Account, path: &str) -> ContainerResult<()> {
        let client = DavSeed::new(self.base_url(), &account.username, &account.password);
        client.delete(path)?;
        Ok(())
    }

    pub fn add_event(
        &self,
        account: &Account,
        calendar_segment_idx: usize,
        tag: &str,
        icals: &[RawFixture],
    ) -> ContainerResult<(String, String)> {
        let client = DavSeed::new(self.base_url(), &account.username, &account.password);
        let display = account.layout.calendars[calendar_segment_idx];
        let collection = collection_segment("cal", calendar_segment_idx, display);
        let fixture = icals.first().ok_or_else(|| {
            ContainerError::Seed("no ical fixture available for add_event".to_owned())
        })?;
        let suffix = format!("added-{tag}");
        let (bytes, uid) = rewrite_uid(&fixture.bytes, &suffix)
            .ok_or_else(|| ContainerError::Seed("ical fixture has no UID".to_owned()))?;
        let path = format!("/{}/{collection}/added-{tag}.ics", account.username);
        client.put(&path, "text/calendar; charset=utf-8", &bytes)?;
        Ok((path, uid))
    }

    pub fn add_contact(
        &self,
        account: &Account,
        book_segment_idx: usize,
        tag: &str,
        vcards: &[RawFixture],
    ) -> ContainerResult<(String, String)> {
        let client = DavSeed::new(self.base_url(), &account.username, &account.password);
        let display = account.layout.address_books[book_segment_idx];
        let collection = collection_segment("ab", book_segment_idx, display);
        let fixture = vcards.first().ok_or_else(|| {
            ContainerError::Seed("no vcard fixture available for add_contact".to_owned())
        })?;
        let suffix = format!("added-{tag}");
        let (bytes, uid) = rewrite_uid(&fixture.bytes, &suffix)
            .ok_or_else(|| ContainerError::Seed("vcard fixture has no UID".to_owned()))?;
        let path = format!("/{}/{collection}/added-{tag}.vcf", account.username);
        client.put(&path, "text/vcard; charset=utf-8", &bytes)?;
        Ok((path, uid))
    }

    pub fn verify_seed(&self, seeds: &[AccountSeed]) -> ContainerResult<()> {
        for (acct, seed) in self.accounts.iter().zip(seeds) {
            let client = DavSeed::new(self.base_url(), &acct.username, &acct.password);
            let body = client.propfind(&format!("/{}/", acct.username), 1)?;
            if !body.contains("multistatus") {
                return Err(ContainerError::Protocol(format!(
                    "radicale propfind for {} returned no multistatus",
                    acct.username
                )));
            }
            let expected_cal_names: HashSet<&str> = seed
                .calendars
                .iter()
                .map(|c| c.display_name.as_str())
                .collect();
            let expected_ab_names: HashSet<&str> = seed
                .address_books
                .iter()
                .map(|c| c.display_name.as_str())
                .collect();
            if expected_cal_names.is_empty() && expected_ab_names.is_empty() {
                continue;
            }
        }
        Ok(())
    }

    pub fn stop(self) -> ContainerResult<()> {
        self.container.stop()?;
        Ok(())
    }
}

fn wait_ready(base: &str, total: Duration) -> ContainerResult<()> {
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .new_agent();
    let deadline = Instant::now() + total;
    let mut last_err = String::from("no probe attempted");
    while Instant::now() < deadline {
        match agent.get(base).call() {
            Ok(_) => return Ok(()),
            Err(e) => last_err = e.to_string(),
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(ContainerError::Protocol(format!(
        "radicale did not become ready in {total:?}: {last_err}"
    )))
}
