/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;

use rusqlite::{Connection, params};
use serde_json::Value;

use crate::db::exchange_graph_ids;
use crate::error::Error;
use crate::exchange_graph::api;
use crate::exchange_graph::calendar_map::{graph_calendar_color_to_hex, windows_or_iana_to_iana};
use crate::exchange_graph::types::MailboxKind;
use crate::logging::LEVEL_PROGRESS;
use crate::sync::TypeCounts;
use crate::types::ObjectType;

use super::coordinator::{CHUNK_SIZE, GraphCoordinator, enumerate_mail_folders};

#[derive(Debug, Clone)]
pub struct MailFolder {
    pub graph_id: String,
    pub parent_graph_id: Option<String>,
    pub display_name: String,
    pub is_hidden: bool,
    pub local_id: i64,
}

#[derive(Debug, Clone)]
pub struct CalendarFolder {
    pub graph_id: String,
    pub local_id: i64,
}

#[derive(Debug, Clone)]
pub struct ContactFolder {
    pub graph_id: String,
    pub local_id: i64,
}

pub fn reconcile_mail(
    conn: &mut Connection,
    ctx: &GraphCoordinator<'_>,
    mailbox_kind: MailboxKind,
    counts: &mut TypeCounts,
) -> Result<Vec<MailFolder>, Error> {
    let server = enumerate_mail_folders(ctx.client, ctx.endpoints, mailbox_kind, ctx.top)
        .map_err(Error::from)?;
    if ctx.logger.enabled(LEVEL_PROGRESS) {
        eprintln!("graph mailFolders enumerated: {}", server.len());
    }
    let well_known = match mailbox_kind {
        MailboxKind::Primary => resolve_well_known_roles(ctx),
        MailboxKind::Archive => HashMap::new(),
    };
    let local: HashMap<String, i64> =
        exchange_graph_ids::ids_of_type(conn, ctx.source_id, exchange_graph_ids::MAILBOX)?;

    let entries = order_by_parent(server);
    let mut by_id: HashMap<String, i64> = HashMap::new();
    let mut out: Vec<MailFolder> = Vec::new();
    let mut server_ids: Vec<String> = Vec::new();

    for chunk in entries.chunks(CHUNK_SIZE) {
        let tx = conn.unchecked_transaction()?;
        for value in chunk {
            let Some(graph_id) = value.get("id").and_then(Value::as_str) else {
                continue;
            };
            let parent_graph_id = value
                .get("parentFolderId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let display_name = value
                .get("displayName")
                .and_then(Value::as_str)
                .unwrap_or("Unnamed")
                .to_owned();
            let is_hidden = value
                .get("isHidden")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let role = well_known.get(graph_id).copied();
            let parent_local_id = parent_graph_id
                .as_deref()
                .and_then(|p| by_id.get(p))
                .copied();
            let existing = local.get(graph_id).copied();
            server_ids.push(graph_id.to_owned());

            let local_id = if let Some(id) = existing {
                let role = crate::db::roles::unique_role(&tx, role, Some(id))?;
                tx.execute(
                    "UPDATE mailboxes SET name = ?1, parent_id = ?2, role = ?3, is_subscribed = ?4
                     WHERE id = ?5",
                    params![display_name, parent_local_id, role, !is_hidden as i64, id,],
                )?;
                counts.fetched += 1;
                id
            } else {
                let role = crate::db::roles::unique_role(&tx, role, None)?;
                tx.execute(
                    "INSERT INTO mailboxes (name, parent_id, role, sort_order, is_subscribed)
                     VALUES (?1, ?2, ?3, 0, ?4)",
                    params![display_name, parent_local_id, role, !is_hidden as i64],
                )?;
                let new_id = tx.last_insert_rowid();
                exchange_graph_ids::insert(
                    &tx,
                    ctx.source_id,
                    exchange_graph_ids::MAILBOX,
                    graph_id,
                    new_id,
                )?;
                counts.created += 1;
                new_id
            };
            by_id.insert(graph_id.to_owned(), local_id);
            out.push(MailFolder {
                graph_id: graph_id.to_owned(),
                parent_graph_id,
                display_name,
                is_hidden,
                local_id,
            });
        }
        tx.commit()?;
    }

    delete_vanished_mailboxes(
        conn,
        ctx.source_id,
        &local,
        &server_ids,
        counts,
        &ctx.logger,
    )?;

    Ok(out)
}

pub fn reconcile_calendars(
    conn: &mut Connection,
    ctx: &GraphCoordinator<'_>,
    counts: &mut TypeCounts,
) -> Result<Vec<CalendarFolder>, Error> {
    let server = api::collect_all_values(ctx.client, &ctx.endpoints.calendars(ctx.top), &[])
        .map_err(Error::from)?;
    let mailbox_tz = mailbox_timezone(ctx);
    let local: HashMap<String, i64> =
        exchange_graph_ids::ids_of_type(conn, ctx.source_id, exchange_graph_ids::CALENDAR)?;
    let mut out: Vec<CalendarFolder> = Vec::new();
    let mut server_ids: Vec<String> = Vec::new();

    for chunk in server.chunks(CHUNK_SIZE) {
        let tx = conn.unchecked_transaction()?;
        for value in chunk {
            let Some(graph_id) = value.get("id").and_then(Value::as_str) else {
                continue;
            };
            server_ids.push(graph_id.to_owned());
            let name = value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("Calendar")
                .to_owned();
            let color = value
                .get("hexColor")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| {
                    value
                        .get("color")
                        .and_then(Value::as_str)
                        .and_then(graph_calendar_color_to_hex)
                        .map(str::to_owned)
                });
            let is_default = value
                .get("isDefaultCalendar")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let existing = local.get(graph_id).copied();
            let tz = mailbox_tz.clone();
            let local_id = if let Some(id) = existing {
                tx.execute(
                    "UPDATE calendars SET name = ?1, color = ?2, is_default = ?3, time_zone = ?4
                     WHERE id = ?5",
                    params![name, color, is_default as i64, tz, id],
                )?;
                counts.fetched += 1;
                id
            } else {
                tx.execute(
                    "INSERT INTO calendars (name, color, sort_order, is_subscribed, is_visible,
                                              is_default, include_in_availability, time_zone)
                     VALUES (?1, ?2, 0, 1, 1, ?3, 'all', ?4)",
                    params![name, color, is_default as i64, tz],
                )?;
                let new_id = tx.last_insert_rowid();
                exchange_graph_ids::insert(
                    &tx,
                    ctx.source_id,
                    exchange_graph_ids::CALENDAR,
                    graph_id,
                    new_id,
                )?;
                counts.created += 1;
                new_id
            };
            out.push(CalendarFolder {
                graph_id: graph_id.to_owned(),
                local_id,
            });
        }
        tx.commit()?;
    }

    delete_vanished_flat(
        conn,
        ctx.source_id,
        exchange_graph_ids::CALENDAR,
        "calendars",
        &local,
        &server_ids,
        counts,
    )?;
    Ok(out)
}

pub fn reconcile_address_books(
    conn: &mut Connection,
    ctx: &GraphCoordinator<'_>,
    counts: &mut TypeCounts,
) -> Result<Vec<ContactFolder>, Error> {
    let mut server =
        api::collect_all_values(ctx.client, &ctx.endpoints.contact_folders(ctx.top), &[])
            .map_err(Error::from)?;
    let mut seen: std::collections::HashSet<String> = server
        .iter()
        .filter_map(|f| f.get("id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    let mut default_id: Option<String> = None;
    if let Some(default) = default_contact_folder(ctx)
        && let Some(id) = default.get("id").and_then(Value::as_str)
    {
        default_id = Some(id.to_owned());
        if seen.insert(id.to_owned()) {
            server.insert(0, default);
        }
    }
    let mut frontier: Vec<String> = seen.iter().cloned().collect();
    while let Some(parent) = frontier.pop() {
        let url = ctx.endpoints.contact_folder_children(&parent, ctx.top);
        let children = api::collect_all_values(ctx.client, &url, &[]).map_err(Error::from)?;
        for child in children {
            let Some(id) = child.get("id").and_then(Value::as_str) else {
                continue;
            };
            if seen.insert(id.to_owned()) {
                frontier.push(id.to_owned());
                server.push(child);
            }
        }
    }

    let local: HashMap<String, i64> =
        exchange_graph_ids::ids_of_type(conn, ctx.source_id, exchange_graph_ids::ADDRESS_BOOK)?;
    let mut out: Vec<ContactFolder> = Vec::new();
    let mut server_ids: Vec<String> = Vec::new();
    for chunk in server.chunks(CHUNK_SIZE) {
        let tx = conn.unchecked_transaction()?;
        for value in chunk {
            let Some(graph_id) = value.get("id").and_then(Value::as_str) else {
                continue;
            };
            server_ids.push(graph_id.to_owned());
            let name = value
                .get("displayName")
                .and_then(Value::as_str)
                .unwrap_or("Contacts")
                .to_owned();
            let is_default = default_id.as_deref() == Some(graph_id);
            let existing = local.get(graph_id).copied();
            let local_id = if let Some(id) = existing {
                let is_default = crate::db::defaults::unique_default(
                    &tx,
                    ObjectType::AddressBook,
                    is_default,
                    Some(id),
                )?;
                tx.execute(
                    "UPDATE address_books SET name = ?1, is_default = ?2 WHERE id = ?3",
                    params![name, is_default as i64, id],
                )?;
                counts.fetched += 1;
                id
            } else {
                let is_default = crate::db::defaults::unique_default(
                    &tx,
                    ObjectType::AddressBook,
                    is_default,
                    None,
                )?;
                tx.execute(
                    "INSERT INTO address_books (name, sort_order, is_default, is_subscribed)
                     VALUES (?1, 0, ?2, 1)",
                    params![name, is_default as i64],
                )?;
                let new_id = tx.last_insert_rowid();
                exchange_graph_ids::insert(
                    &tx,
                    ctx.source_id,
                    exchange_graph_ids::ADDRESS_BOOK,
                    graph_id,
                    new_id,
                )?;
                counts.created += 1;
                new_id
            };
            out.push(ContactFolder {
                graph_id: graph_id.to_owned(),
                local_id,
            });
        }
        tx.commit()?;
    }

    delete_vanished_flat(
        conn,
        ctx.source_id,
        exchange_graph_ids::ADDRESS_BOOK,
        "address_books",
        &local,
        &server_ids,
        counts,
    )?;
    Ok(out)
}

fn default_contact_folder(ctx: &GraphCoordinator<'_>) -> Option<Value> {
    match ctx.client.get_json(&ctx.endpoints.default_contact_folder()) {
        Ok(value) if value.get("id").and_then(Value::as_str).is_some() => return Some(value),
        Ok(_) => {}
        Err(e) => ctx.logger.warn(&format!(
            "graph default contact folder lookup failed ({e}); \
             falling back to deriving it from an existing contact"
        )),
    }
    let probe = ctx
        .client
        .get_json(&ctx.endpoints.any_contact_parent())
        .ok()?;
    let parent = probe
        .get("value")
        .and_then(Value::as_array)?
        .first()?
        .get("parentFolderId")
        .and_then(Value::as_str)?;
    ctx.client
        .get_json(&ctx.endpoints.contact_folder(parent))
        .ok()
}

fn mailbox_timezone(ctx: &GraphCoordinator<'_>) -> Option<String> {
    let url = ctx.endpoints.mailbox_settings_timezone();
    let value = ctx.client.get_json(&url).ok()?;
    let tz = value.get("timeZone").and_then(Value::as_str)?;
    windows_or_iana_to_iana(tz)
}

fn resolve_well_known_roles(ctx: &GraphCoordinator<'_>) -> HashMap<String, &'static str> {
    let mapping: &[(&str, &str)] = &[
        ("inbox", "inbox"),
        ("drafts", "drafts"),
        ("sentitems", "sent"),
        ("deleteditems", "trash"),
        ("junkemail", "junk"),
        ("archive", "archive"),
    ];
    let mut out = HashMap::new();
    for (short_name, role) in mapping {
        let url = ctx.endpoints.well_known_folder(short_name);
        match ctx.client.get_json(&url) {
            Ok(value) => {
                if let Some(id) = value.get("id").and_then(Value::as_str) {
                    out.insert(id.to_owned(), *role);
                }
            }
            Err(_) => continue,
        }
    }
    out
}

fn order_by_parent(server: Vec<Value>) -> Vec<Value> {
    let mut parents: HashMap<String, Option<String>> = HashMap::new();
    for v in &server {
        if let Some(id) = v.get("id").and_then(Value::as_str) {
            let parent = v
                .get("parentFolderId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            parents.insert(id.to_owned(), parent);
        }
    }
    fn depth_of(
        id: &str,
        parents: &HashMap<String, Option<String>>,
        memo: &mut HashMap<String, usize>,
        seen: &mut std::collections::HashSet<String>,
    ) -> usize {
        if let Some(d) = memo.get(id) {
            return *d;
        }
        if !seen.insert(id.to_owned()) {
            return 0;
        }
        let parent = parents.get(id).and_then(|p| p.as_deref());
        let d = match parent {
            Some(p) if parents.contains_key(p) => 1 + depth_of(p, parents, memo, seen),
            _ => 0,
        };
        memo.insert(id.to_owned(), d);
        d
    }
    let mut memo: HashMap<String, usize> = HashMap::new();
    let mut entries: Vec<(usize, Value)> = server
        .into_iter()
        .map(|v| {
            let id = v.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
            let mut seen = std::collections::HashSet::new();
            let d = depth_of(&id, &parents, &mut memo, &mut seen);
            (d, v)
        })
        .collect();
    entries.sort_by_key(|(d, _)| *d);
    entries.into_iter().map(|(_, v)| v).collect()
}

fn delete_vanished_flat(
    conn: &mut Connection,
    source_id: i64,
    type_name: &str,
    table: &str,
    local: &HashMap<String, i64>,
    server_ids: &[String],
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let server_set: std::collections::HashSet<&str> =
        server_ids.iter().map(String::as_str).collect();
    let vanished: Vec<(&String, &i64)> = local
        .iter()
        .filter(|(graph_id, _)| !server_set.contains(graph_id.as_str()))
        .collect();
    for chunk in vanished.chunks(CHUNK_SIZE) {
        let tx = conn.unchecked_transaction()?;
        for (graph_id, local_id) in chunk {
            let result = tx.execute(
                &format!("DELETE FROM {table} WHERE id = ?1"),
                params![local_id],
            );
            match result {
                Ok(_) => {
                    exchange_graph_ids::delete(&tx, source_id, type_name, graph_id)?;
                    counts.deleted += 1;
                }
                Err(_) => {
                    counts.failed += 1;
                }
            }
        }
        tx.commit()?;
    }
    Ok(())
}

fn delete_vanished_mailboxes(
    conn: &mut Connection,
    source_id: i64,
    local: &HashMap<String, i64>,
    server_ids: &[String],
    counts: &mut TypeCounts,
    logger: &crate::logging::Logger,
) -> Result<(), Error> {
    let server_set: std::collections::HashSet<&str> =
        server_ids.iter().map(String::as_str).collect();
    let mut vanished: Vec<(String, i64)> = local
        .iter()
        .filter(|(id, _)| !server_set.contains(id.as_str()))
        .map(|(id, lid)| (id.clone(), *lid))
        .collect();
    let depths = mailbox_depths(conn, &vanished)?;
    vanished.sort_by_key(|(_, local_id)| std::cmp::Reverse(*depths.get(local_id).unwrap_or(&0)));

    for (graph_id, local_id) in vanished {
        let tx = conn.unchecked_transaction()?;
        let result = tx.execute("DELETE FROM mailboxes WHERE id = ?1", params![local_id]);
        match result {
            Ok(_) => {
                exchange_graph_ids::delete(&tx, source_id, exchange_graph_ids::MAILBOX, &graph_id)?;
                tx.commit()?;
                counts.deleted += 1;
            }
            Err(e) => {
                let _ = tx.rollback();
                logger.warn(&format!(
                    "mailbox {graph_id} (local id {local_id}) could not be deleted (live children?): {e}"
                ));
                counts.failed += 1;
            }
        }
    }
    Ok(())
}

fn mailbox_depths(conn: &Connection, rows: &[(String, i64)]) -> Result<HashMap<i64, usize>, Error> {
    let mut parents: HashMap<i64, Option<i64>> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT id, parent_id FROM mailboxes")?;
        let mut iter = stmt.query([])?;
        while let Some(row) = iter.next()? {
            let id: i64 = row.get(0)?;
            let parent: Option<i64> = row.get(1)?;
            parents.insert(id, parent);
        }
    }
    let mut depths: HashMap<i64, usize> = HashMap::new();
    for (_, local_id) in rows {
        depths.insert(*local_id, depth_of(*local_id, &parents));
    }
    Ok(depths)
}

fn depth_of(id: i64, parents: &HashMap<i64, Option<i64>>) -> usize {
    let mut depth = 0;
    let mut cursor = id;
    let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
    while seen.insert(cursor) {
        match parents.get(&cursor).copied().flatten() {
            Some(p) => {
                depth += 1;
                cursor = p;
            }
            None => break,
        }
    }
    depth
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn order_by_parent_returns_parents_first() {
        let input = vec![
            json!({"id": "C", "parentFolderId": "B"}),
            json!({"id": "B", "parentFolderId": "A"}),
            json!({"id": "A"}),
        ];
        let out = order_by_parent(input);
        assert_eq!(out[0]["id"], "A");
        assert_eq!(out[1]["id"], "B");
        assert_eq!(out[2]["id"], "C");
    }

    #[test]
    fn order_by_parent_tolerates_missing_parent() {
        let input = vec![
            json!({"id": "Orphan", "parentFolderId": "Missing"}),
            json!({"id": "A"}),
        ];
        let out = order_by_parent(input);
        assert_eq!(out.len(), 2);
    }
}
