/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashSet;
use std::time::Duration;

use rusqlite::params;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::{SyncBuilder, SyncRunner};
use testcontainers::{Container, GenericBuildableImage, GenericImage, ImageExt};

use super::data::{RawFixture, load_icals, load_vcards, malformed_ical, rewrite_uid};
use super::dav_client::DavSeed;
use super::error::{ContainerError, ContainerResult};
use super::layouts::{self, Layout};
use super::{Account, Endpoint};

const BASE_IMAGE: &str = "ckulka/baikal:0.10.1-nginx";
const HTTP_PORT: u16 = 80;
const REALM: &str = "BaikalDAV";

const ADMIN_PASSWORD_HASH: &str = "004b53ae84286ba6003b34e8ef404826";
const DIGEST_USER1: &str = "b7a47e657a4a966d0d844b1211416024";
const DIGEST_USER2: &str = "9fc67834c5a099f0a2eccea3f6918169";
const DIGEST_USER3: &str = "e1b30b108dad01b323a999b0bb1252ab";

const SCHEMA: &str = include_str!("baikal_schema.sql");

const BAIKAL_YAML: &str = "system:
    configured_version: '0.10.1'
    timezone: 'UTC'
    card_enabled: true
    cal_enabled: true
    invite_from: 'noreply@vandelay.test'
    dav_auth_type: 'Basic'
    admin_passwordhash: '004b53ae84286ba6003b34e8ef404826'
    failed_access_message: 'user %u authentication failure for Baikal'
    auth_realm: 'BaikalDAV'
    base_uri: ''
database:
    backend: 'sqlite'
    sqlite_file: '/var/www/baikal/Specific/db/db.sqlite'
";

const DOCKERFILE: &str = r#"FROM ckulka/baikal:0.10.1-nginx

COPY baikal.yaml /var/www/baikal/config/baikal.yaml
COPY db.sqlite /var/www/baikal/Specific/db/db.sqlite
RUN touch /var/www/baikal/Specific/INSTALL_DISABLED && \
    chown -R www-data:www-data /var/www/baikal/config /var/www/baikal/Specific && \
    chmod 0640 /var/www/baikal/config/baikal.yaml /var/www/baikal/Specific/db/db.sqlite
"#;

pub struct Baikal {
    container: Container<GenericImage>,
    pub http: Endpoint,
    pub accounts: Vec<Account>,
}

impl Baikal {
    pub fn start() -> ContainerResult<Self> {
        let db_bytes = build_sqlite_db()?;

        let image: GenericImage = GenericBuildableImage::new("vandelay-baikal", "test")
            .with_dockerfile_string(DOCKERFILE.to_owned())
            .with_data(BAIKAL_YAML.as_bytes().to_vec(), "baikal.yaml")
            .with_data(db_bytes, "db.sqlite")
            .build_image()
            .map_err(|e| ContainerError::Seed(format!("baikal build: {e}")))?;

        let _ = BASE_IMAGE;

        let request = image
            .with_exposed_port(HTTP_PORT.tcp())
            .with_wait_for(WaitFor::message_on_stderr("start worker process"))
            .with_startup_timeout(Duration::from_secs(180))
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

    pub fn dav_root(&self) -> String {
        format!("{}/dav.php", self.base_url())
    }

    pub fn calendar_home(&self, account: &Account) -> String {
        format!("{}/calendars/{}/", self.dav_root(), account.username)
    }

    pub fn addressbook_home(&self, account: &Account) -> String {
        format!("{}/addressbooks/{}/", self.dav_root(), account.username)
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
        let client = DavSeed::new(self.dav_root(), &account.username, &account.password);
        let layout = &account.layout;
        let mut seed = AccountSeed::new(account.username.clone());

        for (idx, cal) in layout.calendars.iter().enumerate() {
            let segment = collection_segment("cal", idx, cal);
            let path = format!("/calendars/{}/{}/", account.username, segment);
            let mkcal = format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<c:mkcalendar xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:set><d:prop>
    <d:displayname>{cal}</d:displayname>
    <c:supported-calendar-component-set>
      <c:comp name="VEVENT"/>
      <c:comp name="VTODO"/>
    </c:supported-calendar-component-set>
  </d:prop></d:set>
</c:mkcalendar>"#
            );
            client.mkcalendar(&path, Some(&mkcal))?;
            let mut plan = CollectionPlan::new((*cal).to_owned(), path.clone());

            let count = events_per_calendar(layout, idx);
            let offset = events_offset(layout, idx);
            for (i, fx) in icals.iter().cycle().skip(offset).take(count).enumerate() {
                let item = format!("{path}{}-{}.ics", fx.name, i);
                let suffix = format!("{segment}-{i}");
                let (bytes, uid) = rewrite_uid(&fx.bytes, &suffix).ok_or_else(|| {
                    ContainerError::Seed(format!("ical fixture {} has no UID", fx.name))
                })?;
                client.put(&item, "text/calendar; charset=utf-8", &bytes)?;
                plan.items.push(SeededItem {
                    uid,
                    href: item,
                    source: bytes,
                });
            }
            seed.calendars.push(plan);
        }

        for (idx, ab) in layout.address_books.iter().enumerate() {
            let segment = collection_segment("ab", idx, ab);
            let path = format!("/addressbooks/{}/{}/", account.username, segment);
            let mkbook = format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<d:mkcol xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav">
  <d:set><d:prop>
    <d:resourcetype><d:collection/><c:addressbook/></d:resourcetype>
    <d:displayname>{ab}</d:displayname>
  </d:prop></d:set>
</d:mkcol>"#
            );
            client.mkcol(&path, Some(&mkbook))?;
            let mut plan = CollectionPlan::new((*ab).to_owned(), path.clone());

            let count = contacts_per_book(layout, idx);
            let offset = contacts_offset(layout, idx);
            for (i, fx) in vcards.iter().cycle().skip(offset).take(count).enumerate() {
                let item = format!("{path}{}-{}.vcf", fx.name, i);
                let suffix = format!("{segment}-{i}");
                let (bytes, uid) = rewrite_uid(&fx.bytes, &suffix).ok_or_else(|| {
                    ContainerError::Seed(format!("vcard fixture {} has no UID", fx.name))
                })?;
                client.put(&item, "text/vcard; charset=utf-8", &bytes)?;
                plan.items.push(SeededItem {
                    uid,
                    href: item,
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
        let client = DavSeed::new(self.dav_root(), &account.username, &account.password);
        let segment = collection_segment(
            "cal",
            calendar_segment_idx,
            account.layout.calendars[calendar_segment_idx],
        );
        let path = format!(
            "/calendars/{}/{}/broken-{tag}.ics",
            account.username, segment
        );
        let bad = malformed_ical(tag);
        client.put(&path, "text/calendar; charset=utf-8", &bad.bytes)?;
        Ok(path)
    }

    pub fn delete_item(&self, account: &Account, path: &str) -> ContainerResult<()> {
        let client = DavSeed::new(self.dav_root(), &account.username, &account.password);
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
        let client = DavSeed::new(self.dav_root(), &account.username, &account.password);
        let display = account.layout.calendars[calendar_segment_idx];
        let segment = collection_segment("cal", calendar_segment_idx, display);
        let fixture = icals.first().ok_or_else(|| {
            ContainerError::Seed("no ical fixture available for add_event".to_owned())
        })?;
        let suffix = format!("added-{tag}");
        let (bytes, uid) = rewrite_uid(&fixture.bytes, &suffix)
            .ok_or_else(|| ContainerError::Seed("ical fixture has no UID".to_owned()))?;
        let path = format!("/calendars/{}/{segment}/added-{tag}.ics", account.username);
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
        let client = DavSeed::new(self.dav_root(), &account.username, &account.password);
        let display = account.layout.address_books[book_segment_idx];
        let segment = collection_segment("ab", book_segment_idx, display);
        let fixture = vcards.first().ok_or_else(|| {
            ContainerError::Seed("no vcard fixture available for add_contact".to_owned())
        })?;
        let suffix = format!("added-{tag}");
        let (bytes, uid) = rewrite_uid(&fixture.bytes, &suffix)
            .ok_or_else(|| ContainerError::Seed("vcard fixture has no UID".to_owned()))?;
        let path = format!(
            "/addressbooks/{}/{segment}/added-{tag}.vcf",
            account.username
        );
        client.put(&path, "text/vcard; charset=utf-8", &bytes)?;
        Ok((path, uid))
    }

    pub fn verify_seed(&self, seeds: &[AccountSeed]) -> ContainerResult<()> {
        for (acct, seed) in self.accounts.iter().zip(seeds) {
            let client = DavSeed::new(self.dav_root(), &acct.username, &acct.password);
            let body = client.propfind(&format!("/calendars/{}/", acct.username), 1)?;
            if !body.contains("multistatus") {
                return Err(ContainerError::Protocol(format!(
                    "baikal propfind for {} returned no multistatus",
                    acct.username
                )));
            }
            let _ = seed;
        }
        Ok(())
    }

    pub fn stop(self) -> ContainerResult<()> {
        self.container.stop()?;
        Ok(())
    }
}

fn build_sqlite_db() -> ContainerResult<Vec<u8>> {
    let tmp = tempfile::NamedTempFile::new()
        .map_err(|e| ContainerError::Seed(format!("baikal tempfile: {e}")))?;
    let path = tmp.path().to_path_buf();
    drop(tmp);

    {
        let conn = rusqlite::Connection::open(&path)
            .map_err(|e| ContainerError::Seed(format!("baikal sqlite open: {e}")))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| ContainerError::Seed(format!("baikal schema: {e}")))?;

        let users: [(&str, &str); 3] = [
            (layouts::ACCOUNT1, DIGEST_USER1),
            (layouts::ACCOUNT2, DIGEST_USER2),
            (layouts::ACCOUNT3, DIGEST_USER3),
        ];
        for (name, digest) in users {
            conn.execute(
                "INSERT INTO users (username, digesta1) VALUES (?1, ?2)",
                params![name, digest],
            )
            .map_err(|e| ContainerError::Seed(format!("baikal user {name}: {e}")))?;
            let uri = format!("principals/{name}");
            let email = format!("{name}@vandelay.test");
            let display = format!("Vandelay {name}");
            conn.execute(
                "INSERT INTO principals (uri, email, displayname) VALUES (?1, ?2, ?3)",
                params![uri, email, display],
            )
            .map_err(|e| ContainerError::Seed(format!("baikal principal {name}: {e}")))?;
            conn.execute(
                "INSERT INTO principals (uri, email, displayname) VALUES (?1, NULL, NULL)",
                params![format!("{uri}/calendar-proxy-read")],
            )
            .map_err(|e| ContainerError::Seed(format!("baikal cal-proxy-read {name}: {e}")))?;
            conn.execute(
                "INSERT INTO principals (uri, email, displayname) VALUES (?1, NULL, NULL)",
                params![format!("{uri}/calendar-proxy-write")],
            )
            .map_err(|e| ContainerError::Seed(format!("baikal cal-proxy-write {name}: {e}")))?;
        }

        let _ = REALM;
        let _ = ADMIN_PASSWORD_HASH;
    }

    let bytes =
        std::fs::read(&path).map_err(|e| ContainerError::Seed(format!("baikal read db: {e}")))?;
    let _ = std::fs::remove_file(&path);
    Ok(bytes)
}

fn collection_segment(prefix: &str, idx: usize, name: &str) -> String {
    let mut out = format!("{prefix}-{:02}-", idx);
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_owned()
}

fn events_per_calendar(layout: &Layout, idx: usize) -> usize {
    if layout.calendars.is_empty() {
        return 0;
    }
    let base = layout.event_count / layout.calendars.len().max(1);
    let extra = if idx == 0 {
        layout.event_count % layout.calendars.len()
    } else {
        0
    };
    base + extra
}

fn events_offset(layout: &Layout, idx: usize) -> usize {
    (0..idx).map(|i| events_per_calendar(layout, i)).sum()
}

fn contacts_per_book(layout: &Layout, idx: usize) -> usize {
    if layout.address_books.is_empty() {
        return 0;
    }
    let base = layout.contact_count / layout.address_books.len().max(1);
    let extra = if idx == 0 {
        layout.contact_count % layout.address_books.len()
    } else {
        0
    };
    base + extra
}

fn contacts_offset(layout: &Layout, idx: usize) -> usize {
    (0..idx).map(|i| contacts_per_book(layout, i)).sum()
}

#[derive(Debug, Clone)]
pub struct SeededItem {
    pub uid: String,
    pub href: String,
    pub source: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct CollectionPlan {
    pub display_name: String,
    pub base_href: String,
    pub items: Vec<SeededItem>,
}

impl CollectionPlan {
    fn new(display_name: String, base_href: String) -> Self {
        Self {
            display_name,
            base_href,
            items: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AccountSeed {
    pub username: String,
    pub calendars: Vec<CollectionPlan>,
    pub address_books: Vec<CollectionPlan>,
}

impl AccountSeed {
    fn new(username: String) -> Self {
        Self {
            username,
            calendars: Vec::new(),
            address_books: Vec::new(),
        }
    }

    pub fn total_events(&self) -> usize {
        self.calendars.iter().map(|c| c.items.len()).sum()
    }

    pub fn total_contacts(&self) -> usize {
        self.address_books.iter().map(|c| c.items.len()).sum()
    }

    pub fn event_uids(&self) -> HashSet<String> {
        self.calendars
            .iter()
            .flat_map(|c| c.items.iter().map(|i| i.uid.clone()))
            .collect()
    }

    pub fn contact_uids(&self) -> HashSet<String> {
        self.address_books
            .iter()
            .flat_map(|c| c.items.iter().map(|i| i.uid.clone()))
            .collect()
    }
}
