/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::io::Read;

use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::data::MboxMessage;
use super::error::{SeedError, SeedResult};
use super::jmap::Jmap;

const CORE: &str = "urn:ietf:params:jmap:core";
const MAIL: &str = "urn:ietf:params:jmap:mail";
const SUBMISSION: &str = "urn:ietf:params:jmap:submission";
const SIEVE: &str = "urn:ietf:params:jmap:sieve";
const CONTACTS: &str = "urn:ietf:params:jmap:contacts";
const CALENDARS: &str = "urn:ietf:params:jmap:calendars";
const FILENODE: &str = "urn:ietf:params:jmap:filenode";

pub struct MailboxSpec {
    pub key: &'static str,
    pub name: &'static str,
    pub parent: Option<&'static str>,
    pub role: Option<&'static str>,
}

pub struct FileSpec {
    pub key: &'static str,
    pub name: &'static str,
    pub parent: Option<&'static str>,
    pub directory: bool,
}

pub struct Layout {
    pub mailboxes: &'static [MailboxSpec],
    pub file_tree: &'static [FileSpec],
    pub email_count: usize,
    pub sieve_active: Option<bool>,
    pub identity: bool,
    pub extra_address_book: bool,
    pub extra_calendar: bool,
}

#[derive(Debug, Clone)]
pub struct SeedStats {
    pub mailboxes_created: usize,
    pub emails: usize,
    pub contacts: usize,
    pub events: usize,
    pub sieve_active: Option<bool>,
    pub identity: bool,
    pub file_nodes: usize,
    pub address_books: usize,
    pub calendars: usize,
}

pub fn seed_account(
    jmap: &Jmap,
    account_id: &str,
    account_email: &str,
    layout: &Layout,
    emails: &[MboxMessage],
    contacts: &[Value],
    events: &[Value],
) -> SeedResult<SeedStats> {
    let leaf_mailboxes = create_mailboxes(jmap, account_id, layout)?;
    let mut targets = vec![inbox_id(jmap, account_id)?];
    targets.extend(leaf_mailboxes);
    let imported = import_emails(
        jmap,
        account_id,
        &targets,
        &emails[..layout.email_count.min(emails.len())],
    )?;

    if let Some(active) = layout.sieve_active {
        seed_sieve(jmap, account_id, active)?;
    }
    if layout.identity {
        seed_identity(jmap, account_id, account_email)?;
    }
    if !layout.file_tree.is_empty() {
        seed_filenodes(jmap, account_id, layout.file_tree)?;
    }
    let (n_contacts, n_events) =
        seed_contacts_calendars(jmap, account_id, layout, contacts, events)?;

    Ok(SeedStats {
        mailboxes_created: layout.mailboxes.len(),
        emails: imported,
        contacts: n_contacts,
        events: n_events,
        sieve_active: layout.sieve_active,
        identity: layout.identity,
        file_nodes: layout.file_tree.len(),
        address_books: 1 + usize::from(layout.extra_address_book),
        calendars: 1 + usize::from(layout.extra_calendar),
    })
}

fn create_mailboxes(jmap: &Jmap, account_id: &str, layout: &Layout) -> SeedResult<Vec<String>> {
    if layout.mailboxes.is_empty() {
        return Ok(Vec::new());
    }
    let mut create = serde_json::Map::new();
    for spec in layout.mailboxes {
        let mut obj = serde_json::Map::new();
        obj.insert("name".to_owned(), Value::String(spec.name.to_owned()));
        if let Some(parent) = spec.parent {
            obj.insert("parentId".to_owned(), Value::String(format!("#{parent}")));
        }
        if let Some(role) = spec.role {
            obj.insert("role".to_owned(), Value::String(role.to_owned()));
        }
        create.insert(spec.key.to_owned(), Value::Object(obj));
    }
    let response = jmap.set_create(
        &[CORE, MAIL],
        "Mailbox/set",
        account_id,
        Value::Object(create),
        &[],
    )?;
    let created = response
        .get("created")
        .and_then(Value::as_object)
        .ok_or_else(|| SeedError::Shape(format!("Mailbox/set created missing: {response}")))?;
    let mut leaves = Vec::new();
    let parents: std::collections::HashSet<&str> =
        layout.mailboxes.iter().filter_map(|m| m.parent).collect();
    for spec in layout.mailboxes {
        if parents.contains(spec.key) {
            continue;
        }
        if let Some(id) = created
            .get(spec.key)
            .and_then(|c| c.get("id"))
            .and_then(Value::as_str)
        {
            leaves.push(id.to_owned());
        }
    }
    Ok(leaves)
}

fn inbox_id(jmap: &Jmap, account_id: &str) -> SeedResult<String> {
    let response = jmap.call(
        &[CORE, MAIL],
        "Mailbox/query",
        account_id,
        json!({ "filter": { "role": "inbox" } }),
    )?;
    response
        .get("ids")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| SeedError::Shape("no inbox mailbox".to_owned()))
}

fn import_emails(
    jmap: &Jmap,
    account_id: &str,
    targets: &[String],
    messages: &[MboxMessage],
) -> SeedResult<usize> {
    if messages.is_empty() {
        return Ok(0);
    }
    let mut imported = 0;
    for (batch_index, chunk) in messages.chunks(50).enumerate() {
        let mut emails = serde_json::Map::new();
        for (i, message) in chunk.iter().enumerate() {
            let blob_id = jmap.upload(account_id, "message/rfc822", &message.raw)?;
            let mailbox = &targets[(batch_index * 50 + i) % targets.len()];
            let received_at = OffsetDateTime::from_unix_timestamp(message.received_at)
                .unwrap_or(OffsetDateTime::UNIX_EPOCH)
                .format(&Rfc3339)
                .map_err(|e| SeedError::Resource(format!("receivedAt format: {e}")))?;
            emails.insert(
                format!("e{batch_index}_{i}"),
                json!({
                    "blobId": blob_id,
                    "mailboxIds": { mailbox.as_str(): true },
                    "keywords": { "$seen": true },
                    "receivedAt": received_at
                }),
            );
        }
        let response = jmap.call(
            &[CORE, MAIL],
            "Email/import",
            account_id,
            json!({ "emails": Value::Object(emails) }),
        )?;
        if let Some(created) = response.get("created").and_then(Value::as_object) {
            imported += created.len();
        }
        if let Some(not_created) = response.get("notCreated").and_then(Value::as_object)
            && !not_created.is_empty()
        {
            return Err(SeedError::Method {
                method: "Email/import".to_owned(),
                detail: format!("notCreated: {not_created:?}"),
            });
        }
    }
    Ok(imported)
}

fn seed_sieve(jmap: &Jmap, account_id: &str, active: bool) -> SeedResult<()> {
    let primary = "require [\"fileinto\"];\nif header :contains \"subject\" \"test\" {\n  fileinto \"INBOX\";\n}\n";
    let primary_blob = jmap.upload(account_id, "application/octet-stream", primary.as_bytes())?;
    let secondary = "require [\"vacation\"];\nvacation :days 1 \"out of office\";\n";
    let secondary_blob =
        jmap.upload(account_id, "application/octet-stream", secondary.as_bytes())?;
    let extra: Vec<(&str, Value)> = if active {
        vec![("onSuccessActivateScript", Value::String("#s0".to_owned()))]
    } else {
        Vec::new()
    };
    jmap.set_create(
        &[CORE, SIEVE],
        "SieveScript/set",
        account_id,
        json!({
            "s0": { "name": "vandelay-test-filter", "blobId": primary_blob },
            "s1": { "name": "vandelay-vacation", "blobId": secondary_blob }
        }),
        &extra,
    )?;
    Ok(())
}

fn seed_identity(jmap: &Jmap, account_id: &str, account_email: &str) -> SeedResult<()> {
    jmap.set_create(
        &[CORE, SUBMISSION],
        "Identity/set",
        account_id,
        json!({
            "i0": {
                "name": super::CUSTOM_IDENTITY_NAME,
                "email": account_email,
                "textSignature": "-- \nVandelay Industries",
                "htmlSignature": "<p>Vandelay Industries</p>"
            }
        }),
        &[],
    )?;
    Ok(())
}

fn random_bytes(len: usize) -> SeedResult<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let mut file = std::fs::File::open("/dev/urandom")?;
    file.read_exact(&mut buf)?;
    Ok(buf)
}

fn seed_filenodes(jmap: &Jmap, account_id: &str, tree: &[FileSpec]) -> SeedResult<()> {
    let mut create = serde_json::Map::new();
    for (i, spec) in tree.iter().enumerate() {
        let mut obj = serde_json::Map::new();
        obj.insert("name".to_owned(), Value::String(spec.name.to_owned()));
        if let Some(parent) = spec.parent {
            obj.insert("parentId".to_owned(), Value::String(format!("#{parent}")));
        }
        if spec.directory {
            obj.insert("@type".to_owned(), Value::String("Directory".to_owned()));
        } else {
            let bytes = random_bytes(128 + i * 17)?;
            let blob_id = jmap.upload(account_id, "application/octet-stream", &bytes)?;
            obj.insert("@type".to_owned(), Value::String("File".to_owned()));
            obj.insert("blobId".to_owned(), Value::String(blob_id));
            obj.insert(
                "mediaType".to_owned(),
                Value::String("application/octet-stream".to_owned()),
            );
        }
        create.insert(spec.key.to_owned(), Value::Object(obj));
    }
    jmap.set_create(
        &[CORE, FILENODE],
        "FileNode/set",
        account_id,
        Value::Object(create),
        &[],
    )?;
    Ok(())
}

fn default_collection(
    jmap: &Jmap,
    account_id: &str,
    method: &str,
    using: &[&str],
) -> SeedResult<String> {
    let response = jmap.call(
        using,
        method,
        account_id,
        json!({ "properties": ["id", "isDefault"] }),
    )?;
    let list = response
        .get("list")
        .and_then(Value::as_array)
        .ok_or_else(|| SeedError::Shape(format!("{method} list missing")))?;
    for entry in list {
        if entry.get("isDefault").and_then(Value::as_bool) == Some(true)
            && let Some(id) = entry.get("id").and_then(Value::as_str)
        {
            return Ok(id.to_owned());
        }
    }
    list.first()
        .and_then(|e| e.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| SeedError::Shape(format!("{method} has no entries")))
}

fn seed_contacts_calendars(
    jmap: &Jmap,
    account_id: &str,
    layout: &Layout,
    contacts: &[Value],
    events: &[Value],
) -> SeedResult<(usize, usize)> {
    let default_ab = default_collection(jmap, account_id, "AddressBook/get", &[CORE, CONTACTS])?;
    let mut address_books = vec![default_ab];
    if layout.extra_address_book {
        let response = jmap.set_create(
            &[CORE, CONTACTS],
            "AddressBook/set",
            account_id,
            json!({ "ab": { "name": "Vandelay Work Contacts" } }),
            &[],
        )?;
        if let Some(id) = response
            .get("created")
            .and_then(|c| c.get("ab"))
            .and_then(|o| o.get("id"))
            .and_then(Value::as_str)
        {
            address_books.push(id.to_owned());
        }
    }

    let default_cal = default_collection(jmap, account_id, "Calendar/get", &[CORE, CALENDARS])?;
    let mut calendars = vec![default_cal];
    if layout.extra_calendar {
        let response = jmap.set_create(
            &[CORE, CALENDARS],
            "Calendar/set",
            account_id,
            json!({ "cal": { "name": "Vandelay Team Calendar", "color": "#3366cc" } }),
            &[],
        )?;
        if let Some(id) = response
            .get("created")
            .and_then(|c| c.get("cal"))
            .and_then(|o| o.get("id"))
            .and_then(Value::as_str)
        {
            calendars.push(id.to_owned());
        }
    }

    let mut n_contacts = 0;
    for (i, card) in contacts.iter().enumerate() {
        let mut obj = card
            .as_object()
            .ok_or_else(|| SeedError::Shape("contact not an object".to_owned()))?
            .clone();
        let book = &address_books[i % address_books.len()];
        obj.insert("addressBookIds".to_owned(), json!({ book.as_str(): true }));
        jmap.set_create(
            &[CORE, CONTACTS],
            "ContactCard/set",
            account_id,
            json!({ "card": Value::Object(obj) }),
            &[],
        )?;
        n_contacts += 1;
    }

    let mut n_events = 0;
    for (i, event) in events.iter().enumerate() {
        let mut obj = event
            .as_object()
            .ok_or_else(|| SeedError::Shape("event not an object".to_owned()))?
            .clone();
        for immutable in ["method", "prodId"] {
            obj.remove(immutable);
        }
        if obj.contains_key("recurrenceRule") || obj.contains_key("recurrenceOverrides") {
            obj.remove("recurrenceId");
        }
        let calendar = &calendars[i % calendars.len()];
        obj.insert("calendarIds".to_owned(), json!({ calendar.as_str(): true }));
        jmap.set_create(
            &[CORE, CALENDARS],
            "CalendarEvent/set",
            account_id,
            json!({ "event": Value::Object(obj) }),
            &[],
        )?;
        n_events += 1;
    }

    Ok((n_contacts, n_events))
}
