/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

pub mod admin;
pub mod data;
pub mod error;
pub mod jmap;
pub mod seed;

use admin::Admin;
use error::SeedResult;
use jmap::Jmap;
use seed::{FileSpec, Layout, MailboxSpec};

pub const ADMIN_USER: &str = "admin";
pub const ADMIN_PASSWORD: &str = "admin";
pub const DOMAIN: &str = "vandelay.org";
pub const USER_PASSWORD: &str = "VandelayUser#2026";
pub const ADMIN_ACCOUNT_PASSWORD: &str = "VandelayAdmin#2026";

pub const SYNC_IN: [&str; 3] = ["test1", "test2", "test3"];
pub const SYNC_OUT: [&str; 3] = ["test4", "test5", "test6"];
pub const ADMIN_LOCALPART: &str = "vandeladmin";
pub const CUSTOM_IDENTITY_NAME: &str = "Vandelay Test (Custom)";

macro_rules! mailbox {
    ($k:expr, $n:expr, $p:expr, $r:expr) => {
        MailboxSpec {
            key: $k,
            name: $n,
            parent: $p,
            role: $r,
        }
    };
}

macro_rules! node {
    ($k:expr, $n:expr, $p:expr, $d:expr) => {
        FileSpec {
            key: $k,
            name: $n,
            parent: $p,
            directory: $d,
        }
    };
}

static LAYOUT1_MAILBOXES: &[MailboxSpec] = &[
    mailbox!("proj", "Projects", None, None),
    mailbox!("alpha", "Alpha", Some("proj"), None),
    mailbox!("sub", "Subtasks", Some("alpha"), None),
    mailbox!("done", "Done", Some("sub"), None),
    mailbox!("beta", "Beta", Some("proj"), None),
    mailbox!("backlog", "Backlog", Some("beta"), None),
    mailbox!("arch", "Archive", None, Some("archive")),
];

static LAYOUT2_MAILBOXES: &[MailboxSpec] = &[
    mailbox!("work", "Work", None, None),
    mailbox!("clients", "Clients", Some("work"), None),
    mailbox!("acme", "Acme Corp", Some("clients"), None),
    mailbox!("inv", "Invoices", Some("acme"), None),
    mailbox!("globex", "Globex", Some("clients"), None),
    mailbox!("internal", "Internal", Some("work"), None),
    mailbox!("pers", "Personal", None, None),
    mailbox!("rcpt", "Receipts", Some("pers"), None),
    mailbox!("y2025", "2025", Some("rcpt"), None),
];

static LAYOUT3_MAILBOXES: &[MailboxSpec] = &[
    mailbox!("lists", "Lists", None, None),
    mailbox!("news", "Newsletters", Some("lists"), None),
    mailbox!("tech", "Tech", Some("news"), None),
    mailbox!("notif", "Notifications", Some("lists"), None),
    mailbox!("misc", "Misc", None, None),
];

static LAYOUT1_FILES: &[FileSpec] = &[
    node!("d_docs", "Documents", None, true),
    node!("d_rep", "Reports", Some("d_docs"), true),
    node!("d_24", "2024", Some("d_rep"), true),
    node!("f_q4", "q4-results.bin", Some("d_24"), false),
    node!("d_25", "2025", Some("d_rep"), true),
    node!("f_q1", "q1-results.bin", Some("d_25"), false),
    node!("d_spec", "Specs", Some("d_docs"), true),
    node!("f_ovr", "overview.bin", Some("d_spec"), false),
    node!("f_readme", "README.bin", None, false),
];

static LAYOUT2_FILES: &[FileSpec] = &[
    node!("m_media", "Media", None, true),
    node!("m_photos", "Photos", Some("m_media"), true),
    node!("m_23", "2023", Some("m_photos"), true),
    node!("f_trip", "trip.bin", Some("m_23"), false),
    node!("m_audio", "Audio", Some("m_media"), true),
    node!("f_notes", "notes.bin", Some("m_audio"), false),
    node!("m_proj", "Projects", None, true),
    node!("m_src", "src", Some("m_proj"), true),
    node!("f_main", "main.bin", Some("m_src"), false),
];

static LAYOUT3_FILES: &[FileSpec] = &[
    node!("b_bak", "Backup", None, true),
    node!("b_db", "db", Some("b_bak"), true),
    node!("f_snap", "snapshot.bin", Some("b_db"), false),
    node!("f_scratch", "scratch.bin", None, false),
];

fn layout_for(localpart: &str) -> Layout {
    match localpart {
        "test1" => Layout {
            mailboxes: LAYOUT1_MAILBOXES,
            file_tree: LAYOUT1_FILES,
            email_count: 400,
            sieve_active: Some(true),
            identity: true,
            extra_address_book: true,
            extra_calendar: true,
        },
        "test2" => Layout {
            mailboxes: LAYOUT2_MAILBOXES,
            file_tree: LAYOUT2_FILES,
            email_count: 110,
            sieve_active: Some(false),
            identity: true,
            extra_address_book: false,
            extra_calendar: true,
        },
        _ => Layout {
            mailboxes: LAYOUT3_MAILBOXES,
            file_tree: LAYOUT3_FILES,
            email_count: 70,
            sieve_active: None,
            identity: false,
            extra_address_book: false,
            extra_calendar: false,
        },
    }
}

pub struct AccountFixture {
    pub localpart: String,
    pub email: String,
    pub password: String,
    pub admin_role: bool,
    pub account_id: String,
    pub seeded: Option<seed::SeedStats>,
}

pub struct Fixture {
    pub base_url: String,
    pub domain: String,
    pub domain_id: String,
    pub admin_login: (String, String),
    pub accounts: Vec<AccountFixture>,
}

impl Fixture {
    pub fn account(&self, localpart: &str) -> Option<&AccountFixture> {
        self.accounts.iter().find(|a| a.localpart == localpart)
    }
}

fn email_of(localpart: &str) -> String {
    format!("{localpart}@{DOMAIN}")
}

pub fn teardown(base_url: &str) -> SeedResult<()> {
    let admin = Admin::connect(base_url, ADMIN_USER, ADMIN_PASSWORD)?;
    admin.teardown_domain(DOMAIN)?;
    admin.invalidate_caches()
}

pub fn provision(base_url: &str) -> SeedResult<Fixture> {
    let admin = Admin::connect(base_url, ADMIN_USER, ADMIN_PASSWORD)?;
    admin.enable_directory_queries()?;
    admin.teardown_domain(DOMAIN)?;
    admin.invalidate_caches()?;
    let domain_id = admin.ensure_domain(DOMAIN)?;
    admin.invalidate_caches()?;

    let mut specs: Vec<(String, String, bool)> = Vec::new();
    for lp in SYNC_IN.iter().chain(SYNC_OUT.iter()) {
        specs.push(((*lp).to_owned(), USER_PASSWORD.to_owned(), false));
    }
    specs.push((
        ADMIN_LOCALPART.to_owned(),
        ADMIN_ACCOUNT_PASSWORD.to_owned(),
        true,
    ));

    let mut accounts = Vec::new();
    for (localpart, password, admin_role) in &specs {
        admin.create_account(localpart, &domain_id, password, *admin_role)?;
    }
    admin.invalidate_caches()?;
    for (localpart, password, admin_role) in &specs {
        let email = email_of(localpart);
        let user = Jmap::connect(base_url, &email, password)?;
        accounts.push(AccountFixture {
            localpart: localpart.clone(),
            email,
            password: password.clone(),
            admin_role: *admin_role,
            account_id: user.account_id.clone(),
            seeded: None,
        });
    }

    let messages = data::load_mbox(usize::MAX)?;
    let contacts = data::load_contacts()?;
    let events = data::load_events()?;

    let mut offset = 0;
    for localpart in SYNC_IN {
        let index = accounts
            .iter()
            .position(|a| a.localpart == localpart)
            .ok_or_else(|| error::SeedError::Shape(format!("missing account {localpart}")))?;
        let layout = layout_for(localpart);
        let (email, password, account_id) = {
            let a = &accounts[index];
            (a.email.clone(), a.password.clone(), a.account_id.clone())
        };
        let user = Jmap::connect(base_url, &email, &password)?;
        let end = (offset + layout.email_count).min(messages.len());
        let slice = &messages[offset..end];
        offset = end;
        let (c0, c1, e0, e1) = match localpart {
            "test1" => (0, contacts.len().min(3), 0, events.len().min(4)),
            "test2" => (
                contacts.len().min(3),
                contacts.len().min(6),
                events.len().min(4),
                events.len().min(8),
            ),
            _ => (
                contacts.len().min(6),
                contacts.len(),
                events.len().min(8),
                events.len(),
            ),
        };
        let stats = seed::seed_account(
            &user,
            &account_id,
            &email,
            &layout,
            slice,
            &contacts[c0..c1],
            &events[e0..e1],
        )?;
        accounts[index].seeded = Some(stats);
    }

    Ok(Fixture {
        base_url: base_url.to_owned(),
        domain: DOMAIN.to_owned(),
        domain_id,
        admin_login: (ADMIN_USER.to_owned(), ADMIN_PASSWORD.to_owned()),
        accounts,
    })
}
