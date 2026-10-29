/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod integration;
mod seeder;

use integration::stalwart::shared as shared_stalwart;
use seeder::jmap::Jmap;
use seeder::seed::SeedStats;
use serde_json::{Value, json};

const CORE: &str = "urn:ietf:params:jmap:core";
const MAIL: &str = "urn:ietf:params:jmap:mail";
const SUBMISSION: &str = "urn:ietf:params:jmap:submission";
const SIEVE: &str = "urn:ietf:params:jmap:sieve";
const CONTACTS: &str = "urn:ietf:params:jmap:contacts";
const CALENDARS: &str = "urn:ietf:params:jmap:calendars";
const FILENODE: &str = "urn:ietf:params:jmap:filenode";
const PRINCIPALS: &str = "urn:ietf:params:jmap:principals";

const DEFAULT_MAILBOXES: usize = 5;

fn list_len(v: &Value) -> usize {
    v.get("list")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(usize::MAX)
}

fn ids_len(v: &Value) -> usize {
    v.get("ids")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(usize::MAX)
}

fn bare() -> SeedStats {
    SeedStats {
        mailboxes_created: 0,
        emails: 0,
        contacts: 0,
        events: 0,
        sieve_active: None,
        identity: false,
        file_nodes: 0,
        address_books: 1,
        calendars: 1,
    }
}

fn verify_account(label: &str, user: &Jmap, account_id: &str, s: &SeedStats) {
    let mailboxes = user
        .call(
            &[CORE, MAIL],
            "Mailbox/get",
            account_id,
            json!({ "properties": ["id", "role"] }),
        )
        .expect("Mailbox/get");
    assert_eq!(
        list_len(&mailboxes),
        DEFAULT_MAILBOXES + s.mailboxes_created,
        "{label}: mailbox count"
    );

    let emails = user
        .call(
            &[CORE, MAIL],
            "Email/query",
            account_id,
            json!({ "calculateTotal": true }),
        )
        .expect("Email/query");
    let email_total = emails
        .get("total")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or_else(|| ids_len(&emails));
    assert_eq!(email_total, s.emails, "{label}: email count");

    let scripts = user
        .call(
            &[CORE, SIEVE],
            "SieveScript/get",
            account_id,
            json!({ "properties": ["id", "name", "isActive"] }),
        )
        .expect("SieveScript/get");
    match s.sieve_active {
        Some(active) => {
            let list = scripts
                .get("list")
                .and_then(Value::as_array)
                .expect("sieve list");
            assert_eq!(list.len(), 2, "{label}: sieve script count");
            let primary = list
                .iter()
                .find(|s| s.get("name").and_then(Value::as_str) == Some("vandelay-test-filter"))
                .expect("primary sieve script present");
            assert_eq!(
                primary.get("isActive").and_then(Value::as_bool),
                Some(active),
                "{label}: primary sieve isActive"
            );
        }
        None => assert_eq!(list_len(&scripts), 0, "{label}: expected no sieve scripts"),
    }

    let identities = user
        .call(
            &[CORE, SUBMISSION],
            "Identity/get",
            account_id,
            json!({ "properties": ["id", "name"] }),
        )
        .expect("Identity/get");
    let custom = identities
        .get("list")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|i| {
                    i.get("name").and_then(Value::as_str) == Some(seeder::CUSTOM_IDENTITY_NAME)
                })
                .count()
        })
        .unwrap_or(0);
    assert_eq!(
        custom,
        usize::from(s.identity),
        "{label}: custom identity presence"
    );

    let files = user
        .call(
            &[CORE, FILENODE],
            "FileNode/query",
            account_id,
            json!({ "calculateTotal": true }),
        )
        .expect("FileNode/query");
    let file_total = files
        .get("total")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or_else(|| ids_len(&files));
    assert_eq!(file_total, s.file_nodes, "{label}: file node count");

    let books = user
        .call(
            &[CORE, CONTACTS],
            "AddressBook/get",
            account_id,
            json!({ "properties": ["id"] }),
        )
        .expect("AddressBook/get");
    assert_eq!(
        list_len(&books),
        s.address_books,
        "{label}: address book count"
    );

    let calendars = user
        .call(
            &[CORE, CALENDARS],
            "Calendar/get",
            account_id,
            json!({ "properties": ["id"] }),
        )
        .expect("Calendar/get");
    assert_eq!(list_len(&calendars), s.calendars, "{label}: calendar count");

    let cards = user
        .call(
            &[CORE, CONTACTS],
            "ContactCard/get",
            account_id,
            json!({ "properties": ["id"] }),
        )
        .expect("ContactCard/get");
    assert_eq!(list_len(&cards), s.contacts, "{label}: contact card count");

    let cal_events = user
        .call(
            &[CORE, CALENDARS],
            "CalendarEvent/query",
            account_id,
            json!({ "calculateTotal": true }),
        )
        .expect("CalendarEvent/query");
    let event_total = cal_events
        .get("total")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or_else(|| ids_len(&cal_events));
    assert_eq!(event_total, s.events, "{label}: calendar event count");
}

#[test]
#[ignore = "requires Docker"]
fn provision_seed_verify_and_teardown() {
    let stalwart = shared_stalwart();
    let fixture = seeder::provision(stalwart.base_url()).expect("provision");

    assert_eq!(fixture.domain, seeder::DOMAIN);
    assert!(!fixture.domain_id.is_empty());
    assert_eq!(
        fixture.accounts.len(),
        seeder::SYNC_IN.len() + seeder::SYNC_OUT.len() + 1
    );

    for lp in seeder::SYNC_IN {
        let a = fixture.account(lp).expect("sync-in account");
        let stats = a.seeded.as_ref().expect("sync-in seeded stats");
        assert!(stats.emails > 0, "{lp}: expected seeded emails");
        let user = Jmap::connect(&fixture.base_url, &a.email, &a.password).expect("login sync-in");
        assert_eq!(user.account_id, a.account_id);
        verify_account(lp, &user, &a.account_id, stats);
    }

    let baseline = bare();
    for lp in seeder::SYNC_OUT {
        let a = fixture.account(lp).expect("sync-out account");
        assert!(a.seeded.is_none(), "{lp}: targets must not be seeded");
        let user = Jmap::connect(&fixture.base_url, &a.email, &a.password).expect("login sync-out");
        verify_account(lp, &user, &a.account_id, &baseline);
    }

    let admin_fx = fixture
        .account(seeder::ADMIN_LOCALPART)
        .expect("admin account");
    assert!(admin_fx.admin_role);
    let admin = Jmap::connect(&fixture.base_url, &admin_fx.email, &admin_fx.password)
        .expect("login admin account");
    let test1 = fixture.account("test1").expect("test1");
    let resolved = admin
        .request(
            &[CORE, PRINCIPALS],
            json!([
                ["Principal/query", { "filter": { "name": test1.email } }, "q"],
                ["Principal/get", {
                    "#ids": { "resultOf": "q", "name": "Principal/query", "path": "/ids" },
                    "properties": ["id", "name", "accounts"]
                }, "g"]
            ]),
        )
        .expect("Principal discovery");
    let principals = resolved
        .get("methodResponses")
        .and_then(Value::as_array)
        .and_then(|r| r.get(1))
        .and_then(|r| r.get(1))
        .and_then(|g| g.get("list"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let exact: Vec<&Value> = principals
        .iter()
        .filter(|p| p.get("name").and_then(Value::as_str) == Some(test1.email.as_str()))
        .collect();
    assert_eq!(exact.len(), 1, "admin Principal exact-name resolution");
    let owner = exact[0]
        .get("accounts")
        .and_then(Value::as_object)
        .and_then(|m| m.keys().next())
        .map(String::as_str)
        .expect("principal accounts key");
    assert_eq!(owner, test1.account_id, "resolved account id matches test1");

    seeder::teardown(stalwart.base_url()).expect("teardown");

    let (admin_user, admin_password) = &fixture.admin_login;
    let gone = seeder::admin::Admin::connect(&fixture.base_url, admin_user, admin_password)
        .expect("admin reconnect")
        .domain_id(seeder::DOMAIN)
        .expect("domain_id query");
    assert!(gone.is_none(), "domain should be gone after teardown");
}
