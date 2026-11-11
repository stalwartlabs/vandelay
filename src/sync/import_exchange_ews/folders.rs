/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, params};

use crate::db::exchange_ews_ids;
use crate::error::Error;
use crate::exchange_ews::EwsClient;
use crate::exchange_ews::error::EwsError;
use crate::exchange_ews::parse::{
    FolderEntry, parse_find_folder_response, parse_find_item_response, parse_folder_inner,
    parse_get_folder_response, parse_response_messages,
};
use crate::exchange_ews::types::{DistinguishedFolderId, FolderClass, MailboxKind};
use crate::exchange_ews::xml::{
    FolderRef, FolderShape, Traversal, find_folder_body, find_item_body, get_folder_body,
};
use crate::logging::{LEVEL_DEFAULT, LEVEL_PROGRESS, Logger};
use crate::sync::TypeCounts;

pub struct FolderPlan {
    pub mail: Vec<ClassifiedFolder>,
    pub calendar: Vec<ClassifiedFolder>,
    pub contacts: Vec<ClassifiedFolder>,
    pub well_known_roles: HashMap<String, &'static str>,
    pub root_folder_id: String,
}

#[derive(Debug, Clone)]
pub struct ClassifiedFolder {
    pub folder: FolderEntry,
    pub kind: FolderClass,
}

pub fn plan_folders(
    client: &EwsClient,
    url: &str,
    mailbox_kind: MailboxKind,
    mailbox: Option<&str>,
    logger: Logger,
) -> Result<FolderPlan, EwsError> {
    let mailbox = mailbox.filter(|_| !matches!(mailbox_kind, MailboxKind::PublicFolders));
    let root = mailbox_kind.distinguished_root();
    let request = find_folder_body(FolderRef::distinguished(root, mailbox), Traversal::Deep);
    let resp = client.call(url, "FindFolder", &request)?;
    let parsed = parse_find_folder_response(&resp.body)?;
    if logger.enabled(LEVEL_PROGRESS) {
        eprintln!("FindFolder returned {} folders", parsed.folders.len());
    }
    let folders = drop_hidden_subtrees(parsed.folders);
    let parents: HashMap<String, String> = folders
        .iter()
        .filter_map(|f| Some((f.folder_id.id.clone(), f.parent_id.clone()?)))
        .collect();
    let mut mail = Vec::new();
    let mut calendar = Vec::new();
    let mut contacts = Vec::new();
    for folder in folders {
        let mut class = FolderClass::from_ipf(&folder.folder_class);
        if class == FolderClass::Mail && FolderClass::is_mail_fallback(&folder.folder_class) {
            let classless = folder.folder_class.trim().is_empty();
            if classless && !matches!(mailbox_kind, MailboxKind::Archive) {
                class = kind_from_items(client, url, &folder, logger);
            }
            if logger.enabled(LEVEL_DEFAULT) {
                let shown = if classless { "no" } else { "unrecognized" };
                let kind = match class {
                    FolderClass::Calendar => "calendar",
                    FolderClass::Contacts => "contacts",
                    FolderClass::Mail | FolderClass::Skipped => "mail",
                };
                eprintln!(
                    "warning: EWS folder {:?} has {shown} FolderClass ({:?}); importing as {kind}",
                    folder.display_name, folder.folder_class
                );
            }
        }
        match (mailbox_kind, class) {
            (MailboxKind::Archive, FolderClass::Mail) => {
                mail.push(ClassifiedFolder {
                    folder,
                    kind: class,
                });
            }
            (MailboxKind::Archive, _) => {}
            (_, FolderClass::Mail) => mail.push(ClassifiedFolder {
                folder,
                kind: class,
            }),
            (_, FolderClass::Calendar) => {
                calendar.push(ClassifiedFolder {
                    folder,
                    kind: class,
                });
            }
            (_, FolderClass::Contacts) => {
                contacts.push(ClassifiedFolder {
                    folder,
                    kind: class,
                });
            }
            (_, FolderClass::Skipped) => {}
        }
    }
    reparent_under_mail_ancestors(&mut mail, &parents);
    let well_known_roles = resolve_well_known_roles(client, url, mailbox_kind, mailbox, logger)?;
    let root_folder_id = resolve_root_folder_id(client, url, mailbox_kind, mailbox)?;
    Ok(FolderPlan {
        mail,
        calendar,
        contacts,
        well_known_roles,
        root_folder_id,
    })
}

const CLASS_SAMPLE_SIZE: u32 = 10;

fn kind_from_items(
    client: &EwsClient,
    url: &str,
    folder: &FolderEntry,
    logger: Logger,
) -> FolderClass {
    let request = find_item_body(
        FolderRef::Concrete(&folder.folder_id),
        Traversal::Shallow,
        0,
        CLASS_SAMPLE_SIZE,
    );
    let sampled = client
        .call(url, "FindItem", &request)
        .and_then(|resp| parse_find_item_response(&resp.body));
    match sampled {
        Ok(found) => FolderClass::from_item_classes(
            found
                .items
                .iter()
                .filter_map(|item| item.item_class.as_deref()),
        ),
        Err(e) => {
            if logger.enabled(LEVEL_PROGRESS) {
                eprintln!(
                    "FindItem sample of {:?} failed: {e}; treating it as mail",
                    folder.display_name
                );
            }
            FolderClass::Mail
        }
    }
}

fn drop_hidden_subtrees(mut folders: Vec<FolderEntry>) -> Vec<FolderEntry> {
    if !folders.iter().any(|f| f.is_hidden) {
        return folders;
    }
    let by_id: HashMap<&str, &FolderEntry> = folders
        .iter()
        .map(|f| (f.folder_id.id.as_str(), f))
        .collect();
    let hidden: HashSet<String> = folders
        .iter()
        .filter(|f| {
            let mut current = Some(*f);
            for _ in 0..=by_id.len() {
                let Some(folder) = current else {
                    return false;
                };
                if folder.is_hidden {
                    return true;
                }
                current = folder
                    .parent_id
                    .as_deref()
                    .and_then(|p| by_id.get(p).copied());
            }
            false
        })
        .map(|f| f.folder_id.id.clone())
        .collect();
    folders.retain(|f| !hidden.contains(&f.folder_id.id));
    folders
}

fn reparent_under_mail_ancestors(mail: &mut [ClassifiedFolder], parents: &HashMap<String, String>) {
    let mail_ids: HashSet<String> = mail.iter().map(|f| f.folder.folder_id.id.clone()).collect();
    for folder in mail.iter_mut() {
        let mut parent = folder.folder.parent_id.as_deref();
        for _ in 0..=parents.len() {
            match parent {
                Some(p) if !mail_ids.contains(p) => match parents.get(p) {
                    Some(grandparent) => parent = Some(grandparent),
                    None => break,
                },
                _ => break,
            }
        }
        let parent = parent.map(str::to_owned);
        folder.folder.parent_id = parent;
    }
}

fn resolve_well_known_roles(
    client: &EwsClient,
    url: &str,
    mailbox_kind: MailboxKind,
    mailbox: Option<&str>,
    logger: Logger,
) -> Result<HashMap<String, &'static str>, EwsError> {
    if !matches!(mailbox_kind, MailboxKind::Primary) {
        return Ok(HashMap::new());
    }
    let folders: Vec<FolderRef<'_>> = WELL_KNOWN_ROLES
        .iter()
        .map(|(d, _)| FolderRef::distinguished(*d, mailbox))
        .collect();
    let request = get_folder_body(&folders, FolderShape::IdOnly);
    let resp = match client.call(url, "GetFolder", &request) {
        Ok(r) => r,
        Err(e) => {
            if logger.enabled(LEVEL_PROGRESS) {
                eprintln!("GetFolder for distinguished ids failed: {e}; roles will be empty");
            }
            return Ok(HashMap::new());
        }
    };
    let messages =
        parse_response_messages(&resp.body, "GetFolderResponseMessage").unwrap_or_default();
    let mut map: HashMap<String, &'static str> = HashMap::new();
    for (msg, (_, role)) in messages.into_iter().zip(WELL_KNOWN_ROLES.iter()) {
        if !msg.success {
            continue;
        }
        let Ok(Some(entry)) = parse_folder_inner(&msg.inner_xml) else {
            continue;
        };
        if entry.folder_id.id.is_empty() {
            continue;
        }
        if let Some(r) = role {
            map.insert(entry.folder_id.id, r);
        }
    }
    Ok(map)
}

fn resolve_root_folder_id(
    client: &EwsClient,
    url: &str,
    mailbox_kind: MailboxKind,
    mailbox: Option<&str>,
) -> Result<String, EwsError> {
    let root = mailbox_kind.distinguished_root();
    let body = get_folder_body(
        &[FolderRef::distinguished(root, mailbox)],
        FolderShape::IdOnly,
    );
    let resp = client.call(url, "GetFolder", &body)?;
    let entries = parse_get_folder_response(&resp.body)?;
    Ok(entries
        .into_iter()
        .next()
        .map(|e| e.folder_id.id)
        .unwrap_or_default())
}

const WELL_KNOWN_ROLES: &[(DistinguishedFolderId, Option<&'static str>)] = &[
    (DistinguishedFolderId::Inbox, Some("inbox")),
    (DistinguishedFolderId::SentItems, Some("sent")),
    (DistinguishedFolderId::Drafts, Some("drafts")),
    (DistinguishedFolderId::DeletedItems, Some("trash")),
    (DistinguishedFolderId::JunkEmail, Some("junk")),
    (DistinguishedFolderId::Archive, Some("archive")),
    (DistinguishedFolderId::Outbox, None),
    (DistinguishedFolderId::ConversationHistory, None),
];

struct DeleteTarget<'a> {
    type_name: &'a str,
    table: &'a str,
    counts: &'a mut TypeCounts,
}

pub struct ReconcileCounts<'a> {
    pub mailbox: &'a mut TypeCounts,
    pub calendar: &'a mut TypeCounts,
    pub addressbook: &'a mut TypeCounts,
    pub email: &'a mut TypeCounts,
    pub calendar_event: &'a mut TypeCounts,
    pub contact: &'a mut TypeCounts,
}

pub fn reconcile(
    conn: &mut Connection,
    source_id: i64,
    plan: &FolderPlan,
    counts: &mut ReconcileCounts<'_>,
    _logger: Logger,
) -> Result<(), Error> {
    let local_mailbox: HashMap<String, exchange_ews_ids::FolderRow> =
        exchange_ews_ids::folders_of_type(conn, source_id, exchange_ews_ids::MAILBOX)
            .map_err(|e| Error::Partial(e.to_string()))?;
    let local_calendar: HashMap<String, exchange_ews_ids::FolderRow> =
        exchange_ews_ids::folders_of_type(conn, source_id, exchange_ews_ids::CALENDAR)
            .map_err(|e| Error::Partial(e.to_string()))?;
    let local_addressbook: HashMap<String, exchange_ews_ids::FolderRow> =
        exchange_ews_ids::folders_of_type(conn, source_id, exchange_ews_ids::ADDRESS_BOOK)
            .map_err(|e| Error::Partial(e.to_string()))?;

    let mut mailbox_local_ids: HashMap<String, i64> = local_mailbox
        .iter()
        .map(|(id, row)| (id.clone(), row.local_id))
        .collect();

    let ordered_mail = order_by_parent(&plan.mail);
    let ordered_calendar = order_by_parent(&plan.calendar);
    let ordered_contacts = order_by_parent(&plan.contacts);

    let mut server_ids_mail: Vec<String> = Vec::new();
    let mut server_ids_calendar: Vec<String> = Vec::new();
    let mut server_ids_addressbook: Vec<String> = Vec::new();

    for folder in &ordered_mail {
        server_ids_mail.push(folder.folder.folder_id.id.clone());
        let role = plan
            .well_known_roles
            .get(&folder.folder.folder_id.id)
            .copied();
        let mut ctx = UpsertCtx {
            conn,
            source_id,
            local: &local_mailbox,
            counts: &mut *counts.mailbox,
            root_folder_id: &plan.root_folder_id,
        };
        upsert_mailbox(&mut ctx, &mut mailbox_local_ids, folder, role)?;
    }
    for folder in &ordered_calendar {
        server_ids_calendar.push(folder.folder.folder_id.id.clone());
        let mut ctx = UpsertCtx {
            conn,
            source_id,
            local: &local_calendar,
            counts: &mut *counts.calendar,
            root_folder_id: &plan.root_folder_id,
        };
        upsert_calendar(&mut ctx, folder)?;
    }
    for folder in &ordered_contacts {
        server_ids_addressbook.push(folder.folder.folder_id.id.clone());
        let mut ctx = UpsertCtx {
            conn,
            source_id,
            local: &local_addressbook,
            counts: &mut *counts.addressbook,
            root_folder_id: &plan.root_folder_id,
        };
        upsert_address_book(&mut ctx, folder)?;
    }

    delete_vanished(
        conn,
        source_id,
        &local_mailbox,
        &server_ids_mail,
        DeleteTarget {
            type_name: exchange_ews_ids::MAILBOX,
            table: "mailboxes",
            counts: counts.mailbox,
        },
        Some(DeleteTarget {
            type_name: exchange_ews_ids::EMAIL,
            table: "emails",
            counts: counts.email,
        }),
    )?;
    delete_vanished(
        conn,
        source_id,
        &local_calendar,
        &server_ids_calendar,
        DeleteTarget {
            type_name: exchange_ews_ids::CALENDAR,
            table: "calendars",
            counts: counts.calendar,
        },
        Some(DeleteTarget {
            type_name: exchange_ews_ids::CALENDAR_EVENT,
            table: "calendar_events",
            counts: counts.calendar_event,
        }),
    )?;
    delete_vanished(
        conn,
        source_id,
        &local_addressbook,
        &server_ids_addressbook,
        DeleteTarget {
            type_name: exchange_ews_ids::ADDRESS_BOOK,
            table: "address_books",
            counts: counts.addressbook,
        },
        Some(DeleteTarget {
            type_name: exchange_ews_ids::CONTACT_CARD,
            table: "contact_cards",
            counts: counts.contact,
        }),
    )?;
    Ok(())
}

fn order_by_parent(folders: &[ClassifiedFolder]) -> Vec<&ClassifiedFolder> {
    let by_id: HashMap<&str, &ClassifiedFolder> = folders
        .iter()
        .map(|f| (f.folder.folder_id.id.as_str(), f))
        .collect();
    let mut depths: HashMap<&str, usize> = HashMap::new();
    fn depth_of<'a>(
        id: &'a str,
        by_id: &HashMap<&'a str, &'a ClassifiedFolder>,
        memo: &mut HashMap<&'a str, usize>,
        seen: &mut std::collections::HashSet<&'a str>,
    ) -> usize {
        if let Some(d) = memo.get(id) {
            return *d;
        }
        if !seen.insert(id) {
            return 0;
        }
        let d = match by_id.get(id).and_then(|f| f.folder.parent_id.as_deref()) {
            Some(parent) if by_id.contains_key(parent) => 1 + depth_of(parent, by_id, memo, seen),
            _ => 0,
        };
        memo.insert(id, d);
        d
    }
    for f in folders {
        let id = f.folder.folder_id.id.as_str();
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let d = depth_of(id, &by_id, &mut depths, &mut seen);
        depths.insert(id, d);
    }
    let mut out: Vec<&ClassifiedFolder> = folders.iter().collect();
    out.sort_by_key(|f| {
        depths
            .get(f.folder.folder_id.id.as_str())
            .copied()
            .unwrap_or(0)
    });
    out
}

struct UpsertCtx<'a> {
    conn: &'a mut Connection,
    source_id: i64,
    local: &'a HashMap<String, exchange_ews_ids::FolderRow>,
    counts: &'a mut TypeCounts,
    root_folder_id: &'a str,
}

fn upsert_mailbox(
    ctx: &mut UpsertCtx<'_>,
    mailbox_local_ids: &mut HashMap<String, i64>,
    folder: &ClassifiedFolder,
    role: Option<&'static str>,
) -> Result<(), Error> {
    let parent_local = parent_local_id(folder, mailbox_local_ids, ctx.root_folder_id);
    let tx = ctx
        .conn
        .unchecked_transaction()
        .map_err(|e| Error::Partial(e.to_string()))?;
    let existing = ctx.local.get(&folder.folder.folder_id.id);
    let local_id = if let Some(row) = existing {
        let role = crate::db::roles::unique_role(&tx, role, Some(row.local_id))
            .map_err(|e| Error::Partial(e.to_string()))?;
        tx.execute(
            "UPDATE mailboxes SET name = ?1, parent_id = ?2, role = ?3 WHERE id = ?4",
            params![folder.folder.display_name, parent_local, role, row.local_id],
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
        exchange_ews_ids::update_change_key(
            &tx,
            ctx.source_id,
            exchange_ews_ids::MAILBOX,
            &folder.folder.folder_id.id,
            &folder.folder.folder_id.change_key,
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
        ctx.counts.fetched += 1;
        row.local_id
    } else {
        let role = crate::db::roles::unique_role(&tx, role, None)
            .map_err(|e| Error::Partial(e.to_string()))?;
        tx.execute(
            "INSERT INTO mailboxes (name, parent_id, role, sort_order, is_subscribed) \
             VALUES (?1, ?2, ?3, 0, 1)",
            params![folder.folder.display_name, parent_local, role],
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
        let new_id = tx.last_insert_rowid();
        exchange_ews_ids::insert(
            &tx,
            ctx.source_id,
            exchange_ews_ids::MAILBOX,
            folder.folder.parent_id.as_deref().unwrap_or(""),
            &folder.folder.folder_id.id,
            &folder.folder.folder_id.change_key,
            new_id,
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
        ctx.counts.created += 1;
        new_id
    };
    tx.commit().map_err(|e| Error::Partial(e.to_string()))?;
    mailbox_local_ids.insert(folder.folder.folder_id.id.clone(), local_id);
    Ok(())
}

fn upsert_calendar(ctx: &mut UpsertCtx<'_>, folder: &ClassifiedFolder) -> Result<(), Error> {
    let tx = ctx
        .conn
        .unchecked_transaction()
        .map_err(|e| Error::Partial(e.to_string()))?;
    let existing = ctx.local.get(&folder.folder.folder_id.id);
    if let Some(row) = existing {
        tx.execute(
            "UPDATE calendars SET name = ?1 WHERE id = ?2",
            params![folder.folder.display_name, row.local_id],
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
        exchange_ews_ids::update_change_key(
            &tx,
            ctx.source_id,
            exchange_ews_ids::CALENDAR,
            &folder.folder.folder_id.id,
            &folder.folder.folder_id.change_key,
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
        ctx.counts.fetched += 1;
    } else {
        tx.execute(
            "INSERT INTO calendars (name, sort_order, is_subscribed, is_visible) \
             VALUES (?1, 0, 1, 1)",
            params![folder.folder.display_name],
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
        let new_id = tx.last_insert_rowid();
        exchange_ews_ids::insert(
            &tx,
            ctx.source_id,
            exchange_ews_ids::CALENDAR,
            folder.folder.parent_id.as_deref().unwrap_or(""),
            &folder.folder.folder_id.id,
            &folder.folder.folder_id.change_key,
            new_id,
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
        ctx.counts.created += 1;
    }
    tx.commit().map_err(|e| Error::Partial(e.to_string()))?;
    Ok(())
}

fn upsert_address_book(ctx: &mut UpsertCtx<'_>, folder: &ClassifiedFolder) -> Result<(), Error> {
    let tx = ctx
        .conn
        .unchecked_transaction()
        .map_err(|e| Error::Partial(e.to_string()))?;
    let existing = ctx.local.get(&folder.folder.folder_id.id);
    if let Some(row) = existing {
        tx.execute(
            "UPDATE address_books SET name = ?1 WHERE id = ?2",
            params![folder.folder.display_name, row.local_id],
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
        exchange_ews_ids::update_change_key(
            &tx,
            ctx.source_id,
            exchange_ews_ids::ADDRESS_BOOK,
            &folder.folder.folder_id.id,
            &folder.folder.folder_id.change_key,
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
        ctx.counts.fetched += 1;
    } else {
        tx.execute(
            "INSERT INTO address_books (name, sort_order, is_subscribed) \
             VALUES (?1, 0, 1)",
            params![folder.folder.display_name],
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
        let new_id = tx.last_insert_rowid();
        exchange_ews_ids::insert(
            &tx,
            ctx.source_id,
            exchange_ews_ids::ADDRESS_BOOK,
            folder.folder.parent_id.as_deref().unwrap_or(""),
            &folder.folder.folder_id.id,
            &folder.folder.folder_id.change_key,
            new_id,
        )
        .map_err(|e| Error::Partial(e.to_string()))?;
        ctx.counts.created += 1;
    }
    tx.commit().map_err(|e| Error::Partial(e.to_string()))?;
    Ok(())
}

fn parent_local_id(
    folder: &ClassifiedFolder,
    mailbox_local_ids: &HashMap<String, i64>,
    root_folder_id: &str,
) -> Option<i64> {
    let parent_id = folder.folder.parent_id.as_deref()?;
    if parent_id == root_folder_id || parent_id.is_empty() {
        return None;
    }
    mailbox_local_ids.get(parent_id).copied()
}

fn delete_vanished(
    conn: &mut Connection,
    source_id: i64,
    local: &HashMap<String, exchange_ews_ids::FolderRow>,
    server_ids: &[String],
    target: DeleteTarget<'_>,
    mut child: Option<DeleteTarget<'_>>,
) -> Result<(), Error> {
    let server_set: std::collections::HashSet<&str> =
        server_ids.iter().map(String::as_str).collect();
    let mut vanished: Vec<(&str, &exchange_ews_ids::FolderRow)> = local
        .iter()
        .filter(|(id, _)| !server_set.contains(id.as_str()))
        .map(|(id, row)| (id.as_str(), row))
        .collect();
    if target.type_name == exchange_ews_ids::MAILBOX {
        vanished.sort_by_key(|(_, row)| std::cmp::Reverse(folder_depth(row, local)));
    }
    for (item_id, row) in vanished {
        if let Some(c) = child.as_mut() {
            delete_folder_children(conn, source_id, item_id, c)?;
        }
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| Error::Partial(e.to_string()))?;
        let result = tx.execute(
            &format!("DELETE FROM {} WHERE id = ?1", target.table),
            params![row.local_id],
        );
        match result {
            Ok(_) => {
                exchange_ews_ids::delete_item(&tx, source_id, target.type_name, item_id)
                    .map_err(|e| Error::Partial(e.to_string()))?;
                tx.commit().map_err(|e| Error::Partial(e.to_string()))?;
                target.counts.deleted += 1;
            }
            Err(_) => {
                let _ = tx.rollback();
                target.counts.failed += 1;
            }
        }
    }
    Ok(())
}

fn delete_folder_children(
    conn: &mut Connection,
    source_id: i64,
    folder_item_id: &str,
    child: &mut DeleteTarget<'_>,
) -> Result<(), Error> {
    let kids = exchange_ews_ids::items_in_folder(conn, source_id, child.type_name, folder_item_id)
        .map_err(|e| Error::Partial(e.to_string()))?;
    for kid in kids {
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| Error::Partial(e.to_string()))?;
        let deleted = tx.execute(
            &format!("DELETE FROM {} WHERE id = ?1", child.table),
            params![kid.local_id],
        );
        match deleted {
            Ok(_) => {
                exchange_ews_ids::delete_item(&tx, source_id, child.type_name, &kid.item_id)
                    .map_err(|e| Error::Partial(e.to_string()))?;
                tx.commit().map_err(|e| Error::Partial(e.to_string()))?;
                child.counts.deleted += 1;
            }
            Err(_) => {
                let _ = tx.rollback();
                child.counts.failed += 1;
            }
        }
    }
    Ok(())
}

fn folder_depth(
    row: &exchange_ews_ids::FolderRow,
    local: &HashMap<String, exchange_ews_ids::FolderRow>,
) -> usize {
    let mut depth = 0;
    let mut cursor = row.folder_id.as_str();
    let mut visited: std::collections::HashSet<&str> = std::collections::HashSet::new();
    while !cursor.is_empty() && visited.insert(cursor) {
        match local.get(cursor) {
            Some(parent) => {
                depth += 1;
                cursor = parent.folder_id.as_str();
            }
            None => break,
        }
    }
    depth
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vanished_folder_cascades_deletion_of_its_child_items() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::init::apply_schema(&conn).unwrap();
        let sid = crate::db::sources::upsert_source(
            &conn,
            &crate::db::sources::SourceKey {
                kind: "exchange_ews".to_owned(),
                session_url: "https://x/EWS/Exchange.asmx".to_owned(),
                account_id: "u@d".to_owned(),
            },
            Some("u"),
            "u@d",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO blobs (id, hash, data) VALUES (1, X'00', X'01')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO mailboxes (id, name) VALUES (10, 'Tasks')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO emails (id, blob_id, received_at, mailbox_ids) VALUES (20, 1, '', '[10]')",
            [],
        )
        .unwrap();
        exchange_ews_ids::insert(
            &conn,
            sid,
            exchange_ews_ids::MAILBOX,
            "",
            "FOLDER",
            "CK",
            10,
        )
        .unwrap();
        exchange_ews_ids::insert(
            &conn,
            sid,
            exchange_ews_ids::EMAIL,
            "FOLDER",
            "MAIL",
            "CK",
            20,
        )
        .unwrap();

        let mut conn = conn;
        let local =
            exchange_ews_ids::folders_of_type(&conn, sid, exchange_ews_ids::MAILBOX).unwrap();
        let mut folder_counts = TypeCounts::default();
        let mut email_counts = TypeCounts::default();
        delete_vanished(
            &mut conn,
            sid,
            &local,
            &[],
            DeleteTarget {
                type_name: exchange_ews_ids::MAILBOX,
                table: "mailboxes",
                counts: &mut folder_counts,
            },
            Some(DeleteTarget {
                type_name: exchange_ews_ids::EMAIL,
                table: "emails",
                counts: &mut email_counts,
            }),
        )
        .unwrap();

        assert_eq!(folder_counts.deleted, 1);
        assert_eq!(
            email_counts.deleted, 1,
            "child email must be cascade-deleted"
        );
        let mailboxes: i64 = conn
            .query_row("SELECT count(*) FROM mailboxes", [], |r| r.get(0))
            .unwrap();
        let emails: i64 = conn
            .query_row("SELECT count(*) FROM emails", [], |r| r.get(0))
            .unwrap();
        let orphan_idmap: i64 = conn
            .query_row("SELECT count(*) FROM sync_id_exchange_ews", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(mailboxes, 0);
        assert_eq!(emails, 0, "no orphaned email row may remain");
        assert_eq!(orphan_idmap, 0, "both idmap rows must be removed");
    }

    fn entry(id: &str, parent: &str, class: &str, hidden: bool) -> FolderEntry {
        FolderEntry {
            folder_id: crate::exchange_ews::types::FolderId {
                id: id.to_owned(),
                change_key: "CK".to_owned(),
            },
            parent_id: Some(parent.to_owned()),
            display_name: id.to_owned(),
            folder_class: class.to_owned(),
            is_hidden: hidden,
            ..FolderEntry::default()
        }
    }

    fn classified(id: &str, parent: &str, kind: FolderClass) -> ClassifiedFolder {
        ClassifiedFolder {
            folder: entry(id, parent, "", false),
            kind,
        }
    }

    #[test]
    fn hidden_folders_are_dropped_with_their_descendants() {
        let folders = vec![
            entry("CONTACTS", "ROOT", "IPF.Contact", false),
            entry("RECIPIENT_CACHE", "CONTACTS", "IPF.Contact", true),
            entry("YAMMER", "ROOT", "IPF.Note", true),
            entry("YAMMER_FEEDS", "YAMMER", "IPF.Note", false),
            entry("YAMMER_DEEP", "YAMMER_FEEDS", "IPF.Note", false),
            entry("INBOX", "ROOT", "IPF.Note", false),
        ];
        let kept: Vec<String> = drop_hidden_subtrees(folders)
            .into_iter()
            .map(|f| f.folder_id.id)
            .collect();
        assert_eq!(kept, ["CONTACTS", "INBOX"]);
    }

    #[test]
    fn mail_folder_is_filed_under_its_nearest_mail_ancestor() {
        let parents: HashMap<String, String> = [
            ("PROJECTS", "ROOT"),
            ("PCAL", "PROJECTS"),
            ("SUB", "PCAL"),
            ("CAL", "ROOT"),
            ("NOTES", "CAL"),
            ("INBOX", "ROOT"),
            ("A", "B"),
            ("B", "A"),
            ("LOOP", "A"),
        ]
        .into_iter()
        .map(|(id, parent)| (id.to_owned(), parent.to_owned()))
        .collect();
        let mut mail = vec![
            classified("PROJECTS", "ROOT", FolderClass::Mail),
            classified("SUB", "PCAL", FolderClass::Mail),
            classified("NOTES", "CAL", FolderClass::Mail),
            classified("INBOX", "ROOT", FolderClass::Mail),
            classified("LOOP", "A", FolderClass::Mail),
        ];
        reparent_under_mail_ancestors(&mut mail, &parents);
        let parent_of = |id: &str| {
            mail.iter()
                .find(|f| f.folder.folder_id.id == id)
                .and_then(|f| f.folder.parent_id.clone())
        };
        assert_eq!(parent_of("SUB").as_deref(), Some("PROJECTS"));
        assert_eq!(parent_of("NOTES").as_deref(), Some("ROOT"));
        assert_eq!(parent_of("INBOX").as_deref(), Some("ROOT"));
        assert_ne!(parent_of("LOOP").as_deref(), Some("PROJECTS"));
    }

    #[test]
    fn classless_folder_kind_comes_from_its_items() {
        let mut server = mockito::Server::new();
        let url = format!("{}/EWS/Exchange.asmx", server.url());
        let _m = server
            .mock("POST", "/EWS/Exchange.asmx")
            .with_status(200)
            .with_header("content-type", "text/xml; charset=utf-8")
            .with_body(
                "<soap:Envelope xmlns:soap=\"http://schemas.xmlsoap.org/soap/envelope/\" \
                 xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\" \
                 xmlns:m=\"http://schemas.microsoft.com/exchange/services/2006/messages\"><soap:Body>\
                 <m:FindItemResponse><m:ResponseMessages><m:FindItemResponseMessage ResponseClass=\"Success\">\
                 <m:ResponseCode>NoError</m:ResponseCode>\
                 <m:RootFolder TotalItemsInView=\"1\" IncludesLastItemInRange=\"true\"><t:Items>\
                 <t:Contact><t:ItemId Id=\"C1\"/><t:ItemClass>IPM.Contact</t:ItemClass></t:Contact>\
                 </t:Items></m:RootFolder></m:FindItemResponseMessage></m:ResponseMessages>\
                 </m:FindItemResponse></soap:Body></soap:Envelope>",
            )
            .create();
        let client = EwsClient::new(
            crate::jmap::http::Auth::Bearer {
                token: "t".to_owned(),
            },
            crate::jmap::http::RetryPolicy::new(0),
            false,
        );
        let folder = entry("LOCAL", "ROOT", "", false);
        assert_eq!(
            kind_from_items(&client, &url, &folder, Logger::new(LEVEL_DEFAULT)),
            FolderClass::Contacts
        );
    }

    #[test]
    fn rerun_does_not_take_a_calendar_local_id_as_mailbox_parent() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::init::apply_schema(&conn).unwrap();
        let sid = crate::db::sources::upsert_source(
            &conn,
            &crate::db::sources::SourceKey {
                kind: "exchange_ews".to_owned(),
                session_url: "https://x/EWS/Exchange.asmx".to_owned(),
                account_id: "u@d".to_owned(),
            },
            Some("u"),
            "u@d",
        )
        .unwrap();
        let plan = FolderPlan {
            mail: vec![
                classified("INBOX", "ROOT", FolderClass::Mail),
                classified("NOTES", "CAL", FolderClass::Mail),
            ],
            calendar: vec![
                classified("CAL", "ROOT", FolderClass::Calendar),
                classified("CAL2", "ROOT", FolderClass::Calendar),
            ],
            contacts: vec![],
            well_known_roles: HashMap::new(),
            root_folder_id: "ROOT".to_owned(),
        };
        for _ in 0..2 {
            let mut counts: [TypeCounts; 6] = Default::default();
            let [
                mailbox,
                calendar,
                addressbook,
                email,
                calendar_event,
                contact,
            ] = &mut counts;
            reconcile(
                &mut conn,
                sid,
                &plan,
                &mut ReconcileCounts {
                    mailbox,
                    calendar,
                    addressbook,
                    email,
                    calendar_event,
                    contact,
                },
                Logger::new(LEVEL_DEFAULT),
            )
            .unwrap();
            let parent: Option<i64> = conn
                .query_row(
                    "SELECT parent_id FROM mailboxes WHERE name = 'NOTES'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(parent, None);
        }
    }

    fn row(id: &str, parent_ews_id: &str, local_id: i64) -> exchange_ews_ids::FolderRow {
        exchange_ews_ids::FolderRow {
            item_id: id.to_owned(),
            folder_id: parent_ews_id.to_owned(),
            change_key: String::new(),
            local_id,
        }
    }

    #[test]
    fn folder_depth_walks_parent_chain() {
        let mut local: HashMap<String, exchange_ews_ids::FolderRow> = HashMap::new();
        local.insert("ROOT".to_owned(), row("ROOT", "", 1));
        local.insert("CHILD".to_owned(), row("CHILD", "ROOT", 2));
        local.insert("GRAND".to_owned(), row("GRAND", "CHILD", 3));
        assert_eq!(folder_depth(local.get("ROOT").unwrap(), &local), 0);
        assert_eq!(folder_depth(local.get("CHILD").unwrap(), &local), 1);
        assert_eq!(folder_depth(local.get("GRAND").unwrap(), &local), 2);
    }

    #[test]
    fn folder_depth_handles_unknown_parent() {
        let mut local: HashMap<String, exchange_ews_ids::FolderRow> = HashMap::new();
        local.insert("X".to_owned(), row("X", "ORPHAN", 1));
        assert_eq!(folder_depth(local.get("X").unwrap(), &local), 0);
    }

    #[test]
    fn folder_depth_stops_on_cycle() {
        let mut local: HashMap<String, exchange_ews_ids::FolderRow> = HashMap::new();
        local.insert("A".to_owned(), row("A", "B", 1));
        local.insert("B".to_owned(), row("B", "A", 2));
        let d = folder_depth(local.get("A").unwrap(), &local);
        assert!(d <= 2);
    }
}
