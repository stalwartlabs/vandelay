/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use indexmap::IndexMap;
use rusqlite::{Connection, Row, params};
use serde_json::{Map, Value};

use crate::db::defaults::unique_default;
use crate::jmap::blob::{BlobWalkError, InlineShape, import_blob_ids, inline_blob_data_uris};
use crate::jmap::error::JmapError;
use crate::jmap::wire::JmapId;
use crate::jmap::wire::address_book::AddressBook;
use crate::jmap::wire::calendar::Calendar;
use crate::jmap::wire::calendar_event::CalendarEvent;
use crate::jmap::wire::contact_card::ContactCard;
use crate::jmap::wire::email::Email;
use crate::jmap::wire::file_node::FileNode;
use crate::jmap::wire::identity::Identity;
use crate::jmap::wire::mailbox::Mailbox;
use crate::jmap::wire::participant_identity::ParticipantIdentity;
use crate::jmap::wire::sieve_script::SieveScript;
use crate::types::ObjectType;

pub trait LocalResolver {
    fn local(&self, ty: ObjectType, jmap_id: &str) -> Option<i64>;
}

pub trait TargetResolver {
    fn target(&self, ty: ObjectType, local_id: i64) -> Option<JmapId>;
}

pub trait BlobIntern {
    fn intern(&mut self, jmap_blob_id: &str) -> Result<i64, JmapError>;
}

pub trait BlobBytes {
    fn bytes(&self, local_id: i64) -> Result<Vec<u8>, JmapError>;
}

fn translate_in(
    ids: &IndexMap<JmapId, bool>,
    ty: ObjectType,
    resolver: &impl LocalResolver,
) -> Result<Vec<i64>, JmapError> {
    let mut out = Vec::with_capacity(ids.len());
    for (id, _) in ids {
        match resolver.local(ty, &id.0) {
            Some(local) => out.push(local),
            None => {
                return Err(JmapError::malformed(format!(
                    "unresolved {} reference {} on insert",
                    ty.jmap_name(),
                    id.0
                )));
            }
        }
    }
    Ok(out)
}

fn translate_out(
    locals: &[i64],
    ty: ObjectType,
    resolver: &impl TargetResolver,
) -> Result<IndexMap<JmapId, bool>, JmapError> {
    let mut out = IndexMap::new();
    for local in locals {
        match resolver.target(ty, *local) {
            Some(target) => {
                out.insert(target, true);
            }
            None => {
                return Err(JmapError::malformed(format!(
                    "unresolved local {} id {local} on export",
                    ty.jmap_name()
                )));
            }
        }
    }
    Ok(out)
}

fn id_array_json(ids: &[i64]) -> String {
    let vals: Vec<Value> = ids.iter().map(|i| Value::from(*i)).collect();
    Value::Array(vals).to_string()
}

fn parse_local_id_array(text: &str) -> Result<Vec<i64>, JmapError> {
    let v: Value = serde_json::from_str(text)?;
    v.as_array()
        .ok_or_else(|| JmapError::malformed("id column is not a JSON array"))?
        .iter()
        .map(|x| {
            x.as_i64()
                .ok_or_else(|| JmapError::malformed("id array element is not an integer"))
        })
        .collect()
}

fn opt_parent(
    resolver: &impl LocalResolver,
    ty: ObjectType,
    parent: &Option<JmapId>,
) -> Option<i64> {
    parent.as_ref().and_then(|p| resolver.local(ty, &p.0))
}

pub fn insert_mailbox(
    conn: &Connection,
    wire: &Mailbox,
    resolver: &impl LocalResolver,
) -> Result<i64, JmapError> {
    let parent = opt_parent(resolver, ObjectType::Mailbox, &wire.parent_id);
    let role = crate::db::roles::unique_role(conn, wire.role.as_deref(), None)?;
    conn.execute(
        "INSERT INTO mailboxes (name, parent_id, role, sort_order, is_subscribed)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            wire.name,
            parent,
            role,
            wire.sort_order,
            wire.is_subscribed as i64
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub const MAILBOX_SELECT: &str =
    "SELECT id, name, parent_id, role, sort_order, is_subscribed FROM mailboxes";

pub fn row_to_mailbox(row: &Row, resolver: &impl TargetResolver) -> Result<Mailbox, JmapError> {
    let parent_local: Option<i64> = row.get(2)?;
    let parent_id = match parent_local {
        Some(p) => Some(
            resolver
                .target(ObjectType::Mailbox, p)
                .ok_or_else(|| JmapError::malformed("unresolved mailbox parent on export"))?,
        ),
        None => None,
    };
    Ok(Mailbox {
        id: None,
        name: row.get(1)?,
        parent_id,
        role: row.get(3)?,
        sort_order: row.get::<_, i64>(4)? as u32,
        is_subscribed: row.get::<_, i64>(5)? != 0,
    })
}

pub fn insert_email(
    conn: &Connection,
    wire: &Email,
    blob_local_id: i64,
    message_match: &str,
    resolver: &impl LocalResolver,
) -> Result<i64, JmapError> {
    let mailbox_locals = translate_in(&wire.mailbox_ids, ObjectType::Mailbox, resolver)?;
    if mailbox_locals.is_empty() {
        return Err(JmapError::malformed("email has no resolvable mailbox"));
    }
    let keywords: Vec<String> = wire.keywords.keys().cloned().collect();
    conn.execute(
        "INSERT INTO emails (blob_id, received_at, mailbox_ids, keywords, message_match)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            blob_local_id,
            format_utc(&wire.received_at.unwrap_or(time::OffsetDateTime::UNIX_EPOCH))?,
            id_array_json(&mailbox_locals),
            Value::Array(keywords.iter().map(|k| Value::from(k.as_str())).collect()).to_string(),
            message_match
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

#[derive(Debug, Clone)]
pub struct EmailRow {
    pub blob_local_id: i64,
    pub received_at: String,
    pub mailbox_locals: Vec<i64>,
    pub keywords: Vec<String>,
    pub message_match: String,
}

pub const EMAIL_SELECT: &str =
    "SELECT id, blob_id, received_at, mailbox_ids, keywords, message_match FROM emails";

pub fn row_to_email(row: &Row) -> Result<EmailRow, JmapError> {
    let mailbox_text: String = row.get(3)?;
    let keyword_text: String = row.get(4)?;
    let keywords: Vec<String> = serde_json::from_str(&keyword_text)?;
    Ok(EmailRow {
        blob_local_id: row.get(1)?,
        received_at: row.get(2)?,
        mailbox_locals: parse_local_id_array(&mailbox_text)?,
        keywords,
        message_match: row.get(5)?,
    })
}

pub fn insert_identity(conn: &Connection, wire: &Identity) -> Result<i64, JmapError> {
    conn.execute(
        "INSERT INTO identities (name, email, reply_to, bcc, text_signature, html_signature)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            wire.name,
            wire.email,
            opt_json(&wire.reply_to)?,
            opt_json(&wire.bcc)?,
            wire.text_signature,
            wire.html_signature
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub const IDENTITY_SELECT: &str =
    "SELECT id, name, email, reply_to, bcc, text_signature, html_signature FROM identities";

pub fn row_to_identity(row: &Row) -> Result<Identity, JmapError> {
    Ok(Identity {
        id: None,
        name: row.get(1)?,
        email: row.get(2)?,
        reply_to: from_opt_json(row.get::<_, Option<String>>(3)?)?,
        bcc: from_opt_json(row.get::<_, Option<String>>(4)?)?,
        text_signature: row.get(5)?,
        html_signature: row.get(6)?,
    })
}

pub fn insert_sieve_script(
    conn: &Connection,
    wire: &SieveScript,
    blob_local_id: i64,
) -> Result<i64, JmapError> {
    conn.execute(
        "INSERT INTO sieve_scripts (name, is_active, blob_id) VALUES (?1, ?2, ?3)",
        params![wire.name, wire.is_active as i64, blob_local_id],
    )?;
    Ok(conn.last_insert_rowid())
}

pub const SIEVE_SELECT: &str = "SELECT id, name, is_active, blob_id FROM sieve_scripts";

pub fn insert_address_book(conn: &Connection, wire: &AddressBook) -> Result<i64, JmapError> {
    let is_default = unique_default(conn, ObjectType::AddressBook, wire.is_default, None)?;
    conn.execute(
        "INSERT INTO address_books (name, description, sort_order, is_default, is_subscribed)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            wire.name,
            wire.description,
            wire.sort_order,
            is_default as i64,
            wire.is_subscribed as i64
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub const ADDRESS_BOOK_SELECT: &str =
    "SELECT id, name, description, sort_order, is_default, is_subscribed FROM address_books";

pub fn insert_calendar(conn: &Connection, wire: &Calendar) -> Result<i64, JmapError> {
    let is_default = unique_default(conn, ObjectType::Calendar, wire.is_default, None)?;
    conn.execute(
        "INSERT INTO calendars (name, description, color, sort_order, is_subscribed, is_visible,
            is_default, include_in_availability, default_alerts_with_time,
            default_alerts_without_time, time_zone)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            wire.name,
            wire.description,
            wire.color,
            wire.sort_order,
            wire.is_subscribed as i64,
            wire.is_visible as i64,
            is_default as i64,
            wire.include_in_availability,
            opt_json(&wire.default_alerts_with_time)?,
            opt_json(&wire.default_alerts_without_time)?,
            wire.time_zone
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub const CALENDAR_SELECT: &str = "SELECT id, name, description, color, sort_order, is_subscribed, \
     is_visible, is_default, include_in_availability, default_alerts_with_time, \
     default_alerts_without_time, time_zone FROM calendars";

pub fn insert_participant_identity(
    conn: &Connection,
    wire: &ParticipantIdentity,
) -> Result<i64, JmapError> {
    let is_default = unique_default(conn, ObjectType::ParticipantIdentity, wire.is_default, None)?;
    conn.execute(
        "INSERT INTO participant_identities (name, calendar_address, is_default)
         VALUES (?1, ?2, ?3)",
        params![wire.name, wire.calendar_address, is_default as i64],
    )?;
    Ok(conn.last_insert_rowid())
}

pub const PARTICIPANT_IDENTITY_SELECT: &str =
    "SELECT id, name, calendar_address, is_default FROM participant_identities";

pub fn insert_file_node(
    conn: &Connection,
    wire: &FileNode,
    blob_local_id: Option<i64>,
    resolver: &impl LocalResolver,
) -> Result<i64, JmapError> {
    let parent = opt_parent(resolver, ObjectType::FileNode, &wire.parent_id);
    let node_type = serde_json::to_value(wire.effective_node_type())?
        .as_str()
        .unwrap_or("file")
        .to_owned();
    let target = match &wire.target {
        Some(t) => Some(serde_json::to_string(t)?),
        None => None,
    };
    conn.execute(
        "INSERT INTO file_nodes (parent_id, node_type, blob_id, target, name, media_type,
            created, modified, is_subscribed, role)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            parent,
            node_type,
            blob_local_id,
            target,
            wire.name,
            wire.media_type,
            format_utc(&wire.created)?,
            opt_format_utc(&wire.modified)?,
            wire.is_subscribed as i64,
            wire.role
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub const FILE_NODE_SELECT: &str = "SELECT id, parent_id, node_type, blob_id, target, name, \
     media_type, created, modified, is_subscribed, role FROM file_nodes";

pub fn insert_contact_card(
    conn: &Connection,
    wire: &ContactCard,
    resolver: &impl LocalResolver,
    blobs: &mut impl BlobIntern,
) -> Result<i64, JmapError> {
    let address_books = translate_in(&wire.address_book_ids, ObjectType::AddressBook, resolver)?;
    let mut data = Value::Object(value_map(&wire.rest));
    let uid = take_string(&mut data, "uid")
        .ok_or_else(|| JmapError::malformed("ContactCard has no uid"))?;
    rewrite_blobs_in(&mut data, blobs)?;
    conn.execute(
        "INSERT INTO contact_cards (uid, address_book_ids, data) VALUES (?1, ?2, ?3)",
        params![uid, id_array_json(&address_books), data.to_string()],
    )?;
    Ok(conn.last_insert_rowid())
}

pub const CONTACT_CARD_SELECT: &str = "SELECT id, uid, address_book_ids, data FROM contact_cards";

pub fn insert_calendar_event(
    conn: &Connection,
    wire: &CalendarEvent,
    resolver: &impl LocalResolver,
    blobs: &mut impl BlobIntern,
) -> Result<i64, JmapError> {
    let calendars = translate_in(&wire.calendar_ids, ObjectType::Calendar, resolver)?;
    let mut data = Value::Object(value_map(&wire.rest));
    for drop_key in ["method", "utcStart", "utcEnd", "isOrigin", "baseEventId"] {
        if let Value::Object(m) = &mut data {
            m.remove(drop_key);
        }
    }
    rewrite_blobs_in(&mut data, blobs)?;
    conn.execute(
        "INSERT INTO calendar_events (calendar_ids, is_draft, use_default_alerts, data)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            id_array_json(&calendars),
            wire.is_draft as i64,
            wire.use_default_alerts as i64,
            data.to_string()
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub const CALENDAR_EVENT_SELECT: &str = "SELECT id, calendar_ids, is_draft, use_default_alerts, data FROM calendar_events \
     WHERE data_type = 'Event'";

pub fn update_mailbox(
    conn: &Connection,
    local_id: i64,
    wire: &Mailbox,
    resolver: &impl LocalResolver,
) -> Result<bool, JmapError> {
    let parent = opt_parent(resolver, ObjectType::Mailbox, &wire.parent_id);
    let role = crate::db::roles::unique_role(conn, wire.role.as_deref(), Some(local_id))?;
    let n = conn.execute(
        "UPDATE mailboxes SET name = ?1, parent_id = ?2, role = ?3, sort_order = ?4,
            is_subscribed = ?5
         WHERE id = ?6 AND (name IS NOT ?1 OR parent_id IS NOT ?2 OR role IS NOT ?3
            OR sort_order IS NOT ?4 OR is_subscribed IS NOT ?5)",
        params![
            wire.name,
            parent,
            role,
            wire.sort_order,
            wire.is_subscribed as i64,
            local_id
        ],
    )?;
    Ok(n > 0)
}

pub fn update_email(
    conn: &Connection,
    local_id: i64,
    obj: &Value,
    resolver: &impl LocalResolver,
) -> Result<bool, JmapError> {
    let mailbox_ids: IndexMap<JmapId, bool> = match obj.get("mailboxIds") {
        Some(v) => serde_json::from_value(v.clone())?,
        None => IndexMap::new(),
    };
    let mailbox_locals = translate_in(&mailbox_ids, ObjectType::Mailbox, resolver)?;
    if mailbox_locals.is_empty() {
        return Err(JmapError::malformed(
            "email update has no resolvable mailbox",
        ));
    }
    let keywords: IndexMap<String, bool> = match obj.get("keywords") {
        Some(v) => serde_json::from_value(v.clone())?,
        None => IndexMap::new(),
    };
    let kw: Vec<String> = keywords.keys().cloned().collect();
    let n = conn.execute(
        "UPDATE emails SET mailbox_ids = ?1, keywords = ?2
         WHERE id = ?3 AND (mailbox_ids IS NOT ?1 OR keywords IS NOT ?2)",
        params![
            id_array_json(&mailbox_locals),
            Value::Array(kw.iter().map(|k| Value::from(k.as_str())).collect()).to_string(),
            local_id
        ],
    )?;
    Ok(n > 0)
}

pub fn update_identity(
    conn: &Connection,
    local_id: i64,
    wire: &Identity,
) -> Result<bool, JmapError> {
    let n = conn.execute(
        "UPDATE identities SET name = ?1, email = ?2, reply_to = ?3, bcc = ?4,
            text_signature = ?5, html_signature = ?6
         WHERE id = ?7 AND (name IS NOT ?1 OR email IS NOT ?2 OR reply_to IS NOT ?3
            OR bcc IS NOT ?4 OR text_signature IS NOT ?5 OR html_signature IS NOT ?6)",
        params![
            wire.name,
            wire.email,
            opt_json(&wire.reply_to)?,
            opt_json(&wire.bcc)?,
            wire.text_signature,
            wire.html_signature,
            local_id
        ],
    )?;
    Ok(n > 0)
}

pub fn update_sieve_script(
    conn: &Connection,
    local_id: i64,
    wire: &SieveScript,
    blob_local_id: i64,
) -> Result<bool, JmapError> {
    let n = conn.execute(
        "UPDATE sieve_scripts SET name = ?1, is_active = ?2, blob_id = ?3
         WHERE id = ?4 AND (name IS NOT ?1 OR is_active IS NOT ?2 OR blob_id IS NOT ?3)",
        params![wire.name, wire.is_active as i64, blob_local_id, local_id],
    )?;
    Ok(n > 0)
}

pub fn update_address_book(
    conn: &Connection,
    local_id: i64,
    wire: &AddressBook,
) -> Result<bool, JmapError> {
    let is_default = unique_default(
        conn,
        ObjectType::AddressBook,
        wire.is_default,
        Some(local_id),
    )?;
    let n = conn.execute(
        "UPDATE address_books SET name = ?1, description = ?2, sort_order = ?3, is_default = ?4,
            is_subscribed = ?5
         WHERE id = ?6 AND (name IS NOT ?1 OR description IS NOT ?2 OR sort_order IS NOT ?3
            OR is_default IS NOT ?4 OR is_subscribed IS NOT ?5)",
        params![
            wire.name,
            wire.description,
            wire.sort_order,
            is_default as i64,
            wire.is_subscribed as i64,
            local_id
        ],
    )?;
    Ok(n > 0)
}

pub fn update_calendar(
    conn: &Connection,
    local_id: i64,
    wire: &Calendar,
) -> Result<bool, JmapError> {
    let is_default = unique_default(conn, ObjectType::Calendar, wire.is_default, Some(local_id))?;
    let n = conn.execute(
        "UPDATE calendars SET name = ?1, description = ?2, color = ?3, sort_order = ?4,
            is_subscribed = ?5, is_visible = ?6, is_default = ?7, include_in_availability = ?8,
            default_alerts_with_time = ?9, default_alerts_without_time = ?10, time_zone = ?11
         WHERE id = ?12 AND (name IS NOT ?1 OR description IS NOT ?2 OR color IS NOT ?3
            OR sort_order IS NOT ?4 OR is_subscribed IS NOT ?5 OR is_visible IS NOT ?6
            OR is_default IS NOT ?7 OR include_in_availability IS NOT ?8
            OR default_alerts_with_time IS NOT ?9 OR default_alerts_without_time IS NOT ?10
            OR time_zone IS NOT ?11)",
        params![
            wire.name,
            wire.description,
            wire.color,
            wire.sort_order,
            wire.is_subscribed as i64,
            wire.is_visible as i64,
            is_default as i64,
            wire.include_in_availability,
            opt_json(&wire.default_alerts_with_time)?,
            opt_json(&wire.default_alerts_without_time)?,
            wire.time_zone,
            local_id
        ],
    )?;
    Ok(n > 0)
}

pub fn update_participant_identity(
    conn: &Connection,
    local_id: i64,
    wire: &ParticipantIdentity,
) -> Result<bool, JmapError> {
    let is_default = unique_default(
        conn,
        ObjectType::ParticipantIdentity,
        wire.is_default,
        Some(local_id),
    )?;
    let n = conn.execute(
        "UPDATE participant_identities SET name = ?1, calendar_address = ?2, is_default = ?3
         WHERE id = ?4 AND (name IS NOT ?1 OR calendar_address IS NOT ?2 OR is_default IS NOT ?3)",
        params![
            wire.name,
            wire.calendar_address,
            is_default as i64,
            local_id
        ],
    )?;
    Ok(n > 0)
}

pub fn update_file_node(
    conn: &Connection,
    local_id: i64,
    wire: &FileNode,
    blob_local_id: Option<i64>,
    resolver: &impl LocalResolver,
) -> Result<bool, JmapError> {
    let parent = opt_parent(resolver, ObjectType::FileNode, &wire.parent_id);
    let node_type = serde_json::to_value(wire.effective_node_type())?
        .as_str()
        .unwrap_or("file")
        .to_owned();
    let target = match &wire.target {
        Some(t) => Some(serde_json::to_string(t)?),
        None => None,
    };
    let n = conn.execute(
        "UPDATE file_nodes SET parent_id = ?1, node_type = ?2, blob_id = ?3, target = ?4,
            name = ?5, media_type = ?6, created = ?7, modified = ?8, is_subscribed = ?9, role = ?10
         WHERE id = ?11 AND (parent_id IS NOT ?1 OR node_type IS NOT ?2 OR blob_id IS NOT ?3
            OR target IS NOT ?4 OR name IS NOT ?5 OR media_type IS NOT ?6 OR created IS NOT ?7
            OR modified IS NOT ?8 OR is_subscribed IS NOT ?9 OR role IS NOT ?10)",
        params![
            parent,
            node_type,
            blob_local_id,
            target,
            wire.name,
            wire.media_type,
            format_utc(&wire.created)?,
            opt_format_utc(&wire.modified)?,
            wire.is_subscribed as i64,
            wire.role,
            local_id
        ],
    )?;
    Ok(n > 0)
}

pub fn update_contact_card(
    conn: &Connection,
    local_id: i64,
    wire: &ContactCard,
    resolver: &impl LocalResolver,
    blobs: &mut impl BlobIntern,
) -> Result<bool, JmapError> {
    let address_books = translate_in(&wire.address_book_ids, ObjectType::AddressBook, resolver)?;
    let mut data = Value::Object(value_map(&wire.rest));
    let uid = take_string(&mut data, "uid")
        .ok_or_else(|| JmapError::malformed("ContactCard has no uid"))?;
    rewrite_blobs_in(&mut data, blobs)?;
    let n = conn.execute(
        "UPDATE contact_cards SET uid = ?1, address_book_ids = ?2, data = ?3
         WHERE id = ?4 AND (uid IS NOT ?1 OR address_book_ids IS NOT ?2 OR data IS NOT ?3)",
        params![
            uid,
            id_array_json(&address_books),
            data.to_string(),
            local_id
        ],
    )?;
    Ok(n > 0)
}

pub fn update_calendar_event(
    conn: &Connection,
    local_id: i64,
    wire: &CalendarEvent,
    resolver: &impl LocalResolver,
    blobs: &mut impl BlobIntern,
) -> Result<bool, JmapError> {
    let calendars = translate_in(&wire.calendar_ids, ObjectType::Calendar, resolver)?;
    let mut data = Value::Object(value_map(&wire.rest));
    for drop_key in ["method", "utcStart", "utcEnd", "isOrigin", "baseEventId"] {
        if let Value::Object(m) = &mut data {
            m.remove(drop_key);
        }
    }
    rewrite_blobs_in(&mut data, blobs)?;
    let n = conn.execute(
        "UPDATE calendar_events SET calendar_ids = ?1, is_draft = ?2, use_default_alerts = ?3,
            data = ?4
         WHERE id = ?5 AND (calendar_ids IS NOT ?1 OR is_draft IS NOT ?2
            OR use_default_alerts IS NOT ?3 OR data IS NOT ?4)",
        params![
            id_array_json(&calendars),
            wire.is_draft as i64,
            wire.use_default_alerts as i64,
            data.to_string(),
            local_id
        ],
    )?;
    Ok(n > 0)
}

pub fn contact_card_to_wire(
    uid: &str,
    address_book_ids: &str,
    data: &str,
    resolver: &impl TargetResolver,
    blobs: &impl BlobBytes,
) -> Result<Value, JmapError> {
    let mut value: Value = serde_json::from_str(data)?;
    inline_blobs_in(&mut value, InlineShape::JsContactResource, blobs)?;
    let locals = parse_local_id_array(address_book_ids)?;
    let abids = translate_out(&locals, ObjectType::AddressBook, resolver)?;
    if let Value::Object(map) = &mut value {
        map.insert("uid".to_owned(), Value::from(uid));
        map.insert("addressBookIds".to_owned(), id_bool_map(&abids));
    }
    Ok(value)
}

pub fn calendar_event_to_wire(
    calendar_ids: &str,
    is_draft: bool,
    use_default_alerts: bool,
    data: &str,
    resolver: &impl TargetResolver,
    blobs: &impl BlobBytes,
) -> Result<Value, JmapError> {
    let mut value: Value = serde_json::from_str(data)?;
    inline_blobs_in(&mut value, InlineShape::JsCalendarLink, blobs)?;
    let locals = parse_local_id_array(calendar_ids)?;
    let calids = translate_out(&locals, ObjectType::Calendar, resolver)?;
    if let Value::Object(map) = &mut value {
        map.insert("calendarIds".to_owned(), id_bool_map(&calids));
        map.insert("isDraft".to_owned(), Value::Bool(is_draft));
        map.insert(
            "useDefaultAlerts".to_owned(),
            Value::Bool(use_default_alerts),
        );
    }
    Ok(value)
}

fn id_bool_map(ids: &IndexMap<JmapId, bool>) -> Value {
    let mut m = Map::new();
    for (id, _) in ids {
        m.insert(id.0.clone(), Value::Bool(true));
    }
    Value::Object(m)
}

fn value_map(rest: &IndexMap<String, Value>) -> Map<String, Value> {
    let mut m = Map::new();
    for (k, v) in rest {
        m.insert(k.clone(), v.clone());
    }
    m
}

fn take_string(value: &mut Value, key: &str) -> Option<String> {
    if let Value::Object(map) = value
        && let Some(Value::String(s)) = map.remove(key)
    {
        return Some(s);
    }
    None
}

fn rewrite_blobs_in(data: &mut Value, blobs: &mut impl BlobIntern) -> Result<(), JmapError> {
    import_blob_ids(data, |jmap_blob_id| {
        blobs.intern(jmap_blob_id).map_err(BlobWalkError::resolver)
    })
    .map_err(BlobWalkError::into_source)
}

fn inline_blobs_in(
    data: &mut Value,
    shape: InlineShape,
    blobs: &impl BlobBytes,
) -> Result<(), JmapError> {
    inline_blob_data_uris(data, shape, |local_id| {
        blobs.bytes(local_id).map_err(BlobWalkError::resolver)
    })
    .map_err(BlobWalkError::into_source)
}

fn opt_json<T: serde::Serialize>(value: &Option<T>) -> Result<Option<String>, JmapError> {
    match value {
        Some(v) => Ok(Some(serde_json::to_string(v)?)),
        None => Ok(None),
    }
}

fn from_opt_json<T: serde::de::DeserializeOwned>(
    text: Option<String>,
) -> Result<Option<T>, JmapError> {
    match text {
        Some(t) => Ok(Some(serde_json::from_str(&t)?)),
        None => Ok(None),
    }
}

pub fn row_to_address_book(row: &Row) -> Result<AddressBook, JmapError> {
    Ok(AddressBook {
        id: None,
        name: row.get(1)?,
        description: row.get(2)?,
        sort_order: row.get::<_, i64>(3)? as u32,
        is_default: row.get::<_, i64>(4)? != 0,
        is_subscribed: row.get::<_, i64>(5)? != 0,
    })
}

pub fn row_to_calendar(row: &Row) -> Result<Calendar, JmapError> {
    Ok(Calendar {
        id: None,
        name: row.get(1)?,
        description: row.get(2)?,
        color: row.get(3)?,
        sort_order: row.get::<_, i64>(4)? as u32,
        is_subscribed: row.get::<_, i64>(5)? != 0,
        is_visible: row.get::<_, i64>(6)? != 0,
        is_default: row.get::<_, i64>(7)? != 0,
        include_in_availability: row.get(8)?,
        default_alerts_with_time: from_opt_json(row.get::<_, Option<String>>(9)?)?,
        default_alerts_without_time: from_opt_json(row.get::<_, Option<String>>(10)?)?,
        time_zone: row.get(11)?,
    })
}

pub fn row_to_participant_identity(row: &Row) -> Result<ParticipantIdentity, JmapError> {
    Ok(ParticipantIdentity {
        id: None,
        name: row.get(1)?,
        calendar_address: row.get(2)?,
        is_default: row.get::<_, i64>(3)? != 0,
    })
}

#[derive(Debug, Clone)]
pub struct SieveRow {
    pub name: Option<String>,
    pub is_active: bool,
    pub blob_local_id: i64,
}

pub fn row_to_sieve_script(row: &Row) -> Result<SieveRow, JmapError> {
    Ok(SieveRow {
        name: row.get(1)?,
        is_active: row.get::<_, i64>(2)? != 0,
        blob_local_id: row.get(3)?,
    })
}

#[derive(Debug, Clone)]
pub struct FileNodeRow {
    pub wire: FileNode,
    pub blob_local_id: Option<i64>,
}

pub fn row_to_file_node(
    row: &Row,
    resolver: &impl TargetResolver,
) -> Result<FileNodeRow, JmapError> {
    let parent_local: Option<i64> = row.get(1)?;
    let parent_id = match parent_local {
        Some(p) => Some(
            resolver
                .target(ObjectType::FileNode, p)
                .ok_or_else(|| JmapError::malformed("unresolved file_node parent on export"))?,
        ),
        None => None,
    };
    let node_type = Some(serde_json::from_value(Value::from(
        row.get::<_, String>(2)?,
    ))?);
    let target: Option<Vec<String>> = from_opt_json(row.get::<_, Option<String>>(4)?)?;
    let blob_local_id: Option<i64> = row.get(3)?;
    Ok(FileNodeRow {
        wire: FileNode {
            id: None,
            parent_id,
            node_type,
            blob_id: None,
            target,
            name: row.get(5)?,
            media_type: row.get(6)?,
            created: parse_utc(&row.get::<_, String>(7)?)?,
            modified: opt_parse_utc(row.get::<_, Option<String>>(8)?)?,
            is_subscribed: row.get::<_, i64>(9)? != 0,
            role: row.get(10)?,
        },
        blob_local_id,
    })
}

fn parse_utc(text: &str) -> Result<time::OffsetDateTime, JmapError> {
    time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
        .map_err(|e| JmapError::malformed(format!("cannot parse date {text}: {e}")))
}

fn opt_parse_utc(text: Option<String>) -> Result<Option<time::OffsetDateTime>, JmapError> {
    match text {
        Some(t) => Ok(Some(parse_utc(&t)?)),
        None => Ok(None),
    }
}

fn format_utc(value: &time::OffsetDateTime) -> Result<String, JmapError> {
    value
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|e| JmapError::malformed(format!("cannot format date: {e}")))
}

fn opt_format_utc(value: &Option<time::OffsetDateTime>) -> Result<Option<String>, JmapError> {
    match value {
        Some(v) => Ok(Some(format_utc(v)?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init;
    use std::collections::HashMap;

    struct MapResolver {
        to_local: HashMap<(ObjectType, String), i64>,
        to_target: HashMap<(ObjectType, i64), String>,
    }
    impl LocalResolver for MapResolver {
        fn local(&self, ty: ObjectType, jmap_id: &str) -> Option<i64> {
            self.to_local.get(&(ty, jmap_id.to_owned())).copied()
        }
    }
    impl TargetResolver for MapResolver {
        fn target(&self, ty: ObjectType, local_id: i64) -> Option<JmapId> {
            self.to_target
                .get(&(ty, local_id))
                .map(|s| JmapId(s.clone()))
        }
    }

    struct FakeBlobs;
    impl BlobIntern for FakeBlobs {
        fn intern(&mut self, _jmap_blob_id: &str) -> Result<i64, JmapError> {
            Ok(42)
        }
    }
    impl BlobBytes for FakeBlobs {
        fn bytes(&self, local_id: i64) -> Result<Vec<u8>, JmapError> {
            Ok(format!("payload-{local_id}").into_bytes())
        }
    }

    fn decode_data_uri(value: &Value, media_type: &str) -> Vec<u8> {
        use base64::Engine;
        let uri = value.as_str().unwrap_or_else(|| panic!("{value} is a URI"));
        let prefix = format!("data:{media_type};base64,");
        let payload = uri
            .strip_prefix(&prefix)
            .unwrap_or_else(|| panic!("{uri} does not start with {prefix}"));
        base64::engine::general_purpose::STANDARD
            .decode(payload)
            .expect("base64 payload")
    }

    fn assert_no_blob_id(value: &Value) {
        match value {
            Value::Object(map) => {
                assert!(map.get("blobId").is_none(), "blobId present in {value}");
                assert!(map.get("@blob").is_none(), "@blob present in {value}");
                for child in map.values() {
                    assert_no_blob_id(child);
                }
            }
            Value::Array(items) => items.iter().for_each(assert_no_blob_id),
            _ => {}
        }
    }

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        init::apply_schema(&c).unwrap();
        c
    }

    #[test]
    fn mailbox_roundtrips_with_parent_translation() {
        let c = mem();
        let root: Mailbox = serde_json::from_value(serde_json::json!({
            "id": "M1", "name": "Root", "role": "archive", "sortOrder": 3
        }))
        .unwrap();
        let empty = MapResolver {
            to_local: HashMap::new(),
            to_target: HashMap::new(),
        };
        let root_local = insert_mailbox(&c, &root, &empty).unwrap();

        let child: Mailbox = serde_json::from_value(serde_json::json!({
            "id": "M2", "name": "Child", "parentId": "M1"
        }))
        .unwrap();
        let mut res = MapResolver {
            to_local: HashMap::new(),
            to_target: HashMap::new(),
        };
        res.to_local
            .insert((ObjectType::Mailbox, "M1".to_owned()), root_local);
        let child_local = insert_mailbox(&c, &child, &res).unwrap();

        res.to_target
            .insert((ObjectType::Mailbox, root_local), "TGT1".to_owned());
        let wire = c
            .query_row(
                &format!("{MAILBOX_SELECT} WHERE id = ?1"),
                params![child_local],
                |row| Ok(row_to_mailbox(row, &res)),
            )
            .unwrap()
            .unwrap();
        assert_eq!(wire.name, "Child");
        assert_eq!(wire.parent_id, Some(JmapId("TGT1".to_owned())));
    }

    #[test]
    fn email_keywords_keep_verbatim_case_and_mailbox_translation() {
        let c = mem();
        let mut res = MapResolver {
            to_local: HashMap::new(),
            to_target: HashMap::new(),
        };
        res.to_local
            .insert((ObjectType::Mailbox, "MB".to_owned()), 7);
        let email: Email = serde_json::from_value(serde_json::json!({
            "id": "E1",
            "blobId": "B1",
            "receivedAt": "2021-05-04T10:00:00Z",
            "mailboxIds": { "MB": true },
            "keywords": { "$Seen": true, "MyTag": true }
        }))
        .unwrap();
        let blob_id = crate::db::blobs::intern_blob(&c, b"rfc5322 bytes").unwrap();
        let local = insert_email(&c, &email, blob_id, "{}", &res).unwrap();
        let got = c
            .query_row(
                &format!("{EMAIL_SELECT} WHERE id = ?1"),
                params![local],
                |row| Ok(row_to_email(row)),
            )
            .unwrap()
            .unwrap();
        assert_eq!(got.blob_local_id, blob_id);
        assert_eq!(got.mailbox_locals, vec![7]);
        assert_eq!(got.keywords, vec!["$Seen".to_owned(), "MyTag".to_owned()]);
    }

    #[test]
    fn unresolved_foreign_key_on_insert_is_malformed() {
        let c = mem();
        let empty = MapResolver {
            to_local: HashMap::new(),
            to_target: HashMap::new(),
        };
        let email: Email = serde_json::from_value(serde_json::json!({
            "id": "E1",
            "blobId": "B1",
            "receivedAt": "2021-05-04T10:00:00Z",
            "mailboxIds": { "UNKNOWN": true },
            "keywords": {}
        }))
        .unwrap();
        let err = insert_email(&c, &email, 1, "{}", &empty).unwrap_err();
        assert!(matches!(err, JmapError::Malformed(_)), "got {err:?}");
    }

    #[test]
    fn unresolved_target_on_export_is_malformed() {
        let c = mem();
        let root: Mailbox = serde_json::from_value(serde_json::json!({
            "id": "M1", "name": "Root", "parentId": "P9"
        }))
        .unwrap();
        let mut res = MapResolver {
            to_local: HashMap::new(),
            to_target: HashMap::new(),
        };
        res.to_local
            .insert((ObjectType::Mailbox, "P9".to_owned()), 1);
        let local = insert_mailbox(&c, &root, &res).unwrap();
        let err = c
            .query_row(
                &format!("{MAILBOX_SELECT} WHERE id = ?1"),
                params![local],
                |row| Ok(row_to_mailbox(row, &res)),
            )
            .unwrap()
            .unwrap_err();
        assert!(matches!(err, JmapError::Malformed(_)), "got {err:?}");
    }

    #[test]
    fn contact_card_strips_uid_and_inlines_media_as_data_uri() {
        let c = mem();
        let res = MapResolver {
            to_local: HashMap::from([((ObjectType::AddressBook, "AB".to_owned()), 1)]),
            to_target: HashMap::from([((ObjectType::AddressBook, 1), "TAB".to_owned())]),
        };
        let card: ContactCard = serde_json::from_value(serde_json::json!({
            "id": "C1",
            "addressBookIds": { "AB": true },
            "uid": "urn:uuid:42",
            "name": { "full": "Jane" },
            "media": { "photo": {
                "@type": "Media", "kind": "photo",
                "blobId": "PB", "mediaType": "image/png"
            } }
        }))
        .unwrap();
        let mut blobs = FakeBlobs;
        let local = insert_contact_card(&c, &card, &res, &mut blobs).unwrap();
        let (uid, abids, data): (String, String, String) = c
            .query_row(
                &format!("{CONTACT_CARD_SELECT} WHERE id = ?1"),
                params![local],
                |r| Ok((r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(uid, "urn:uuid:42");
        let stored: Value = serde_json::from_str(&data).unwrap();
        assert!(stored.get("uid").is_none());
        assert_eq!(stored["media"]["photo"]["@blob"], Value::from(42));

        let up = FakeBlobs;
        let wire = contact_card_to_wire(&uid, &abids, &data, &res, &up).unwrap();
        assert_eq!(wire["uid"], Value::from("urn:uuid:42"));
        assert_eq!(wire["addressBookIds"]["TAB"], Value::Bool(true));
        assert_eq!(
            decode_data_uri(&wire["media"]["photo"]["uri"], "image/png"),
            b"payload-42"
        );
        assert_eq!(
            wire["media"]["photo"]["mediaType"],
            Value::from("image/png")
        );
        assert!(wire["media"]["photo"].get("href").is_none());
        assert_no_blob_id(&wire);
    }

    #[test]
    fn calendar_event_inlines_link_enclosure_as_data_uri() {
        let c = mem();
        let res = MapResolver {
            to_local: HashMap::from([((ObjectType::Calendar, "CAL".to_owned()), 5)]),
            to_target: HashMap::from([((ObjectType::Calendar, 5), "TCAL".to_owned())]),
        };
        let ev: CalendarEvent = serde_json::from_value(serde_json::json!({
            "id": "EV1",
            "calendarIds": { "CAL": true },
            "uid": "ev-with-enclosure",
            "title": "Review",
            "@type": "Event",
            "links": { "1": {
                "@type": "Link", "rel": "enclosure",
                "blobId": "AB", "contentType": "text/plain", "title": "agenda.txt"
            } }
        }))
        .unwrap();
        let mut blobs = FakeBlobs;
        let local = insert_calendar_event(&c, &ev, &res, &mut blobs).unwrap();
        let (cal, dr, ud, data): (String, i64, i64, String) = c
            .query_row(
                &format!("{CALENDAR_EVENT_SELECT} AND id = ?1"),
                params![local],
                |r| Ok((r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        let stored: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(stored["links"]["1"]["@blob"], Value::from(42));

        let up = FakeBlobs;
        let wire = calendar_event_to_wire(&cal, dr != 0, ud != 0, &data, &res, &up).unwrap();
        assert_eq!(
            decode_data_uri(&wire["links"]["1"]["href"], "text/plain"),
            b"payload-42"
        );
        assert_eq!(wire["links"]["1"]["contentType"], Value::from("text/plain"));
        assert_eq!(wire["links"]["1"]["rel"], Value::from("enclosure"));
        assert!(wire["links"]["1"].get("uri").is_none());
        assert_no_blob_id(&wire);
    }

    #[test]
    fn contact_card_media_without_media_type_defaults_to_octet_stream() {
        let c = mem();
        let res = MapResolver {
            to_local: HashMap::from([((ObjectType::AddressBook, "AB".to_owned()), 1)]),
            to_target: HashMap::from([((ObjectType::AddressBook, 1), "TAB".to_owned())]),
        };
        c.execute(
            "INSERT INTO contact_cards (id,uid,address_book_ids,data)
             VALUES (1,'u-1','[1]',?1)",
            params![
                serde_json::json!({ "@type": "Card", "media": { "photo": { "@blob": 9 } } })
                    .to_string()
            ],
        )
        .unwrap();
        let (uid, abids, data): (String, String, String) = c
            .query_row(&format!("{CONTACT_CARD_SELECT} WHERE id = 1"), [], |r| {
                Ok((r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .unwrap();
        let up = FakeBlobs;
        let wire = contact_card_to_wire(&uid, &abids, &data, &res, &up).unwrap();
        assert_eq!(
            decode_data_uri(&wire["media"]["photo"]["uri"], "application/octet-stream"),
            b"payload-9"
        );
        assert_no_blob_id(&wire);
    }

    #[test]
    fn an_archive_read_failure_while_inlining_is_an_archive_error_not_a_unit_failure() {
        struct BrokenBlobs;
        impl BlobBytes for BrokenBlobs {
            fn bytes(&self, _local_id: i64) -> Result<Vec<u8>, JmapError> {
                Err(JmapError::Sqlite(rusqlite::Error::QueryReturnedNoRows))
            }
        }
        let res = MapResolver {
            to_local: HashMap::new(),
            to_target: HashMap::from([
                ((ObjectType::AddressBook, 1), "TAB".to_owned()),
                ((ObjectType::Calendar, 1), "TCAL".to_owned()),
            ]),
        };
        let card = serde_json::json!({ "@type": "Card", "media": { "photo": { "@blob": 9 } } })
            .to_string();
        let event =
            serde_json::json!({ "@type": "Event", "links": { "1": { "@blob": 9 } } }).to_string();
        let failures = [
            contact_card_to_wire("u-1", "[1]", &card, &res, &BrokenBlobs).expect_err("card"),
            calendar_event_to_wire("[1]", false, false, &event, &res, &BrokenBlobs)
                .expect_err("event"),
        ];
        for err in failures {
            assert!(matches!(err, JmapError::Sqlite(_)), "{err:?}");
            let mapped = crate::error::Error::from(err);
            assert!(mapped.aborts_run(), "{mapped} must abort the run");
            assert_eq!(mapped.exit_code(), 7);
        }
    }

    #[test]
    fn calendar_event_extracts_columns_and_drops_method() {
        let c = mem();
        let res = MapResolver {
            to_local: HashMap::from([((ObjectType::Calendar, "CAL".to_owned()), 5)]),
            to_target: HashMap::from([((ObjectType::Calendar, 5), "TCAL".to_owned())]),
        };
        let ev: CalendarEvent = serde_json::from_value(serde_json::json!({
            "id": "EV1",
            "calendarIds": { "CAL": true },
            "isDraft": true,
            "useDefaultAlerts": false,
            "method": "request",
            "utcStart": "2020-01-01T00:00:00Z",
            "title": "Sprint",
            "@type": "Event"
        }))
        .unwrap();
        let mut blobs = FakeBlobs;
        let local = insert_calendar_event(&c, &ev, &res, &mut blobs).unwrap();
        let (cal, dr, ud, data): (String, i64, i64, String) = c
            .query_row(
                &format!("{CALENDAR_EVENT_SELECT} AND id = ?1"),
                params![local],
                |r| Ok((r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(dr, 1);
        assert_eq!(ud, 0);
        let stored: Value = serde_json::from_str(&data).unwrap();
        assert!(stored.get("method").is_none());
        assert!(stored.get("utcStart").is_none());
        assert!(stored.get("calendarIds").is_none());
        assert_eq!(stored["title"], Value::from("Sprint"));

        let up = FakeBlobs;
        let wire = calendar_event_to_wire(&cal, dr != 0, ud != 0, &data, &res, &up).unwrap();
        assert_eq!(wire["calendarIds"]["TCAL"], Value::Bool(true));
        assert_eq!(wire["isDraft"], Value::Bool(true));
        assert_eq!(wire["title"], Value::from("Sprint"));
    }

    #[test]
    fn identity_json_columns_roundtrip() {
        let c = mem();
        let id: Identity = serde_json::from_value(serde_json::json!({
            "id": "I1",
            "name": "Alice",
            "email": "a@x.test",
            "replyTo": [ { "name": "A", "email": "a@x.test" } ]
        }))
        .unwrap();
        let local = insert_identity(&c, &id).unwrap();
        let wire = c
            .query_row(
                &format!("{IDENTITY_SELECT} WHERE id = ?1"),
                params![local],
                |row| Ok(row_to_identity(row)),
            )
            .unwrap()
            .unwrap();
        assert_eq!(wire.email, "a@x.test");
        assert_eq!(wire.reply_to.unwrap()[0].email, "a@x.test");
        assert!(wire.bcc.is_none());
    }

    #[test]
    fn file_node_enum_and_target_roundtrip() {
        let c = mem();
        let empty = MapResolver {
            to_local: HashMap::new(),
            to_target: HashMap::new(),
        };
        let node: FileNode = serde_json::from_value(serde_json::json!({
            "id": "F1",
            "nodeType": "symlink",
            "target": ["..", "etc", "hosts"],
            "name": "link",
            "created": "2022-02-02T02:02:02Z"
        }))
        .unwrap();
        let local = insert_file_node(&c, &node, None, &empty).unwrap();
        let (nt, tgt): (String, Option<String>) = c
            .query_row(
                &format!("{FILE_NODE_SELECT} WHERE id = ?1"),
                params![local],
                |r| Ok((r.get(2)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(nt, "symlink");
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&tgt.unwrap()).unwrap(),
            vec!["..", "etc", "hosts"]
        );
    }

    #[test]
    fn address_book_and_calendar_row_to_wire_roundtrip() {
        let c = mem();
        let ab: AddressBook = serde_json::from_value(serde_json::json!({
            "id": "A1", "name": "Personal", "description": "d", "sortOrder": 2,
            "isDefault": true, "isSubscribed": false
        }))
        .unwrap();
        let abl = insert_address_book(&c, &ab).unwrap();
        let got = c
            .query_row(
                &format!("{ADDRESS_BOOK_SELECT} WHERE id = ?1"),
                params![abl],
                |r| Ok(row_to_address_book(r)),
            )
            .unwrap()
            .unwrap();
        assert_eq!(got.name, "Personal");
        assert!(got.is_default);
        assert!(!got.is_subscribed);

        let cal: Calendar = serde_json::from_value(serde_json::json!({
            "id": "C1", "name": "Work", "color": "#ff0000", "timeZone": "Europe/Rome",
            "isVisible": false
        }))
        .unwrap();
        let cl = insert_calendar(&c, &cal).unwrap();
        let gc = c
            .query_row(
                &format!("{CALENDAR_SELECT} WHERE id = ?1"),
                params![cl],
                |r| Ok(row_to_calendar(r)),
            )
            .unwrap()
            .unwrap();
        assert_eq!(gc.name, "Work");
        assert_eq!(gc.color.as_deref(), Some("#ff0000"));
        assert_eq!(gc.time_zone.as_deref(), Some("Europe/Rome"));
        assert!(!gc.is_visible);
    }

    #[test]
    fn participant_identity_and_sieve_row_to_wire_roundtrip() {
        let c = mem();
        let pi: ParticipantIdentity = serde_json::from_value(serde_json::json!({
            "id": "P1", "name": "Me", "calendarAddress": "mailto:me@x.test",
            "isDefault": true
        }))
        .unwrap();
        let pil = insert_participant_identity(&c, &pi).unwrap();
        let gp = c
            .query_row(
                &format!("{PARTICIPANT_IDENTITY_SELECT} WHERE id = ?1"),
                params![pil],
                |r| Ok(row_to_participant_identity(r)),
            )
            .unwrap()
            .unwrap();
        assert_eq!(gp.calendar_address, "mailto:me@x.test");
        assert!(gp.is_default);

        let ss: SieveScript = serde_json::from_value(serde_json::json!({
            "id": "S1", "name": "main", "isActive": true, "blobId": "B1"
        }))
        .unwrap();
        let blob = crate::db::blobs::intern_blob(&c, b"keep;").unwrap();
        let ssl = insert_sieve_script(&c, &ss, blob).unwrap();
        let gs = c
            .query_row(
                &format!("{SIEVE_SELECT} WHERE id = ?1"),
                params![ssl],
                |r| Ok(row_to_sieve_script(r)),
            )
            .unwrap()
            .unwrap();
        assert_eq!(gs.name.as_deref(), Some("main"));
        assert!(gs.is_active);
        assert_eq!(gs.blob_local_id, blob);
    }

    #[test]
    fn file_node_row_to_wire_translates_parent() {
        let c = mem();
        let mut res = MapResolver {
            to_local: HashMap::new(),
            to_target: HashMap::new(),
        };
        let dir: FileNode = serde_json::from_value(serde_json::json!({
            "id": "D1", "nodeType": "directory", "name": "Docs",
            "created": "2022-02-02T02:02:02Z"
        }))
        .unwrap();
        let dirl = insert_file_node(&c, &dir, None, &res).unwrap();
        res.to_local
            .insert((ObjectType::FileNode, "D1".to_owned()), dirl);
        let file: FileNode = serde_json::from_value(serde_json::json!({
            "id": "F1", "nodeType": "file", "parentId": "D1", "name": "a.bin",
            "type": "application/octet-stream", "created": "2022-02-02T02:02:02Z"
        }))
        .unwrap();
        let blob = crate::db::blobs::intern_blob(&c, b"bytes").unwrap();
        let fl = insert_file_node(&c, &file, Some(blob), &res).unwrap();
        res.to_target
            .insert((ObjectType::FileNode, dirl), "TGTD".to_owned());
        let got = c
            .query_row(
                &format!("{FILE_NODE_SELECT} WHERE id = ?1"),
                params![fl],
                |r| Ok(row_to_file_node(r, &res)),
            )
            .unwrap()
            .unwrap();
        assert_eq!(got.wire.name, "a.bin");
        assert_eq!(got.wire.parent_id, Some(JmapId("TGTD".to_owned())));
        assert_eq!(got.blob_local_id, Some(blob));
    }

    fn one_row(c: &Connection, table: &str) -> i64 {
        c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn delta_update_mailbox_in_place_preserves_id() {
        let c = mem();
        let empty = MapResolver {
            to_local: HashMap::new(),
            to_target: HashMap::new(),
        };
        let m: Mailbox =
            serde_json::from_value(serde_json::json!({"id":"M1","name":"Personal"})).unwrap();
        let local = insert_mailbox(&c, &m, &empty).unwrap();
        let changed: Mailbox = serde_json::from_value(
            serde_json::json!({"id":"M1","name":"PersonalRenamed","role":"archive","sortOrder":4}),
        )
        .unwrap();
        assert!(
            update_mailbox(&c, local, &changed, &empty).unwrap(),
            "a real change reports changed=true"
        );
        assert!(
            !update_mailbox(&c, local, &changed, &empty).unwrap(),
            "re-applying identical values is a no-op (changed=false), so re-runs converge"
        );
        assert_eq!(one_row(&c, "mailboxes"), 1);
        let (name, role, sort): (String, Option<String>, i64) = c
            .query_row(
                "SELECT name, role, sort_order FROM mailboxes WHERE id=?1",
                params![local],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(name, "PersonalRenamed");
        assert_eq!(role.as_deref(), Some("archive"));
        assert_eq!(sort, 4);
    }

    #[test]
    fn delta_update_email_changes_keywords_and_mailboxes_keeps_blob() {
        let c = mem();
        let mut res = MapResolver {
            to_local: HashMap::new(),
            to_target: HashMap::new(),
        };
        res.to_local
            .insert((ObjectType::Mailbox, "MB".to_owned()), 7);
        res.to_local
            .insert((ObjectType::Mailbox, "MB2".to_owned()), 8);
        let email: Email = serde_json::from_value(serde_json::json!({
            "id":"E1","blobId":"B1","receivedAt":"2021-05-04T10:00:00Z",
            "mailboxIds":{"MB":true},"keywords":{"$seen":true}
        }))
        .unwrap();
        let blob = crate::db::blobs::intern_blob(&c, b"rfc5322").unwrap();
        let local = insert_email(&c, &email, blob, "{}", &res).unwrap();

        let changed = serde_json::json!({
            "id":"E1","mailboxIds":{"MB2":true},"keywords":{"$seen":true,"$flagged":true}
        });
        update_email(&c, local, &changed, &res).unwrap();
        assert_eq!(one_row(&c, "emails"), 1);
        let got = c
            .query_row(
                &format!("{EMAIL_SELECT} WHERE id=?1"),
                params![local],
                |row| Ok(row_to_email(row)),
            )
            .unwrap()
            .unwrap();
        assert_eq!(got.blob_local_id, blob, "immutable body blob is untouched");
        assert_eq!(got.mailbox_locals, vec![8], "mailbox membership moved");
        assert!(got.keywords.contains(&"$flagged".to_owned()));
        assert!(got.keywords.contains(&"$seen".to_owned()));
    }

    #[test]
    fn delta_update_identity_in_place() {
        let c = mem();
        let id: Identity =
            serde_json::from_value(serde_json::json!({"id":"I1","name":"Old","email":"a@x.test"}))
                .unwrap();
        let local = insert_identity(&c, &id).unwrap();
        let changed: Identity = serde_json::from_value(
            serde_json::json!({"id":"I1","name":"New Name","email":"a@x.test","textSignature":"sig"}),
        )
        .unwrap();
        update_identity(&c, local, &changed).unwrap();
        assert_eq!(one_row(&c, "identities"), 1);
        let got = c
            .query_row(
                &format!("{IDENTITY_SELECT} WHERE id=?1"),
                params![local],
                |row| Ok(row_to_identity(row)),
            )
            .unwrap()
            .unwrap();
        assert_eq!(got.name, "New Name");
        assert_eq!(got.text_signature, "sig");
    }

    #[test]
    fn delta_update_address_book_in_place() {
        let c = mem();
        let ab: AddressBook =
            serde_json::from_value(serde_json::json!({"id":"A1","name":"Old"})).unwrap();
        let local = insert_address_book(&c, &ab).unwrap();
        let changed: AddressBook = serde_json::from_value(
            serde_json::json!({"id":"A1","name":"Renamed","description":"d2","sortOrder":3}),
        )
        .unwrap();
        update_address_book(&c, local, &changed).unwrap();
        assert_eq!(one_row(&c, "address_books"), 1);
        let got = c
            .query_row(
                &format!("{ADDRESS_BOOK_SELECT} WHERE id=?1"),
                params![local],
                |r| Ok(row_to_address_book(r)),
            )
            .unwrap()
            .unwrap();
        assert_eq!(got.name, "Renamed");
        assert_eq!(got.description.as_deref(), Some("d2"));
    }

    #[test]
    fn delta_update_calendar_in_place() {
        let c = mem();
        let cal: Calendar =
            serde_json::from_value(serde_json::json!({"id":"C1","name":"Old","color":"#000"}))
                .unwrap();
        let local = insert_calendar(&c, &cal).unwrap();
        let changed: Calendar = serde_json::from_value(
            serde_json::json!({"id":"C1","name":"Renamed","color":"#abcdef","timeZone":"Europe/Rome"}),
        )
        .unwrap();
        update_calendar(&c, local, &changed).unwrap();
        assert_eq!(one_row(&c, "calendars"), 1);
        let got = c
            .query_row(
                &format!("{CALENDAR_SELECT} WHERE id=?1"),
                params![local],
                |r| Ok(row_to_calendar(r)),
            )
            .unwrap()
            .unwrap();
        assert_eq!(got.name, "Renamed");
        assert_eq!(got.color.as_deref(), Some("#abcdef"));
        assert_eq!(got.time_zone.as_deref(), Some("Europe/Rome"));
    }

    #[test]
    fn delta_update_participant_identity_in_place() {
        let c = mem();
        let pi: ParticipantIdentity = serde_json::from_value(
            serde_json::json!({"id":"P1","name":"Old","calendarAddress":"mailto:me@x.test"}),
        )
        .unwrap();
        let local = insert_participant_identity(&c, &pi).unwrap();
        let changed: ParticipantIdentity = serde_json::from_value(
            serde_json::json!({"id":"P1","name":"New","calendarAddress":"mailto:me@x.test"}),
        )
        .unwrap();
        update_participant_identity(&c, local, &changed).unwrap();
        assert_eq!(one_row(&c, "participant_identities"), 1);
        let got = c
            .query_row(
                &format!("{PARTICIPANT_IDENTITY_SELECT} WHERE id=?1"),
                params![local],
                |r| Ok(row_to_participant_identity(r)),
            )
            .unwrap()
            .unwrap();
        assert_eq!(got.name, "New");
    }

    #[test]
    fn delta_update_sieve_script_swaps_blob_and_active() {
        let c = mem();
        let ss: SieveScript = serde_json::from_value(
            serde_json::json!({"id":"S1","name":"main","isActive":false,"blobId":"B1"}),
        )
        .unwrap();
        let blob1 = crate::db::blobs::intern_blob(&c, b"keep;").unwrap();
        let local = insert_sieve_script(&c, &ss, blob1).unwrap();
        let blob2 = crate::db::blobs::intern_blob(&c, b"discard;").unwrap();
        let changed: SieveScript = serde_json::from_value(
            serde_json::json!({"id":"S1","name":"main","isActive":true,"blobId":"B2"}),
        )
        .unwrap();
        update_sieve_script(&c, local, &changed, blob2).unwrap();
        assert_eq!(one_row(&c, "sieve_scripts"), 1);
        let got = c
            .query_row(
                &format!("{SIEVE_SELECT} WHERE id=?1"),
                params![local],
                |r| Ok(row_to_sieve_script(r)),
            )
            .unwrap()
            .unwrap();
        assert!(got.is_active);
        assert_eq!(got.blob_local_id, blob2, "content blob swapped");
    }

    #[test]
    fn delta_update_file_node_changes_blob_and_name() {
        let c = mem();
        let res = MapResolver {
            to_local: HashMap::new(),
            to_target: HashMap::new(),
        };
        let file: FileNode = serde_json::from_value(serde_json::json!({
            "id":"F1","nodeType":"file","name":"a.bin",
            "type":"application/octet-stream","created":"2022-02-02T02:02:02Z"
        }))
        .unwrap();
        let blob1 = crate::db::blobs::intern_blob(&c, b"v1").unwrap();
        let local = insert_file_node(&c, &file, Some(blob1), &res).unwrap();
        let blob2 = crate::db::blobs::intern_blob(&c, b"v2").unwrap();
        let changed: FileNode = serde_json::from_value(serde_json::json!({
            "id":"F1","nodeType":"file","name":"renamed.bin",
            "type":"text/plain","created":"2022-02-02T02:02:02Z"
        }))
        .unwrap();
        update_file_node(&c, local, &changed, Some(blob2), &res).unwrap();
        assert_eq!(one_row(&c, "file_nodes"), 1);
        let (name, mt, b): (String, Option<String>, Option<i64>) = c
            .query_row(
                "SELECT name, media_type, blob_id FROM file_nodes WHERE id=?1",
                params![local],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(name, "renamed.bin");
        assert_eq!(mt.as_deref(), Some("text/plain"));
        assert_eq!(b, Some(blob2));
    }

    #[test]
    fn delta_update_contact_card_rewrites_data_and_columns() {
        let c = mem();
        let res = MapResolver {
            to_local: HashMap::from([((ObjectType::AddressBook, "AB".to_owned()), 1)]),
            to_target: HashMap::new(),
        };
        let card: ContactCard = serde_json::from_value(serde_json::json!({
            "id":"C1","addressBookIds":{"AB":true},"uid":"u-1","name":{"full":"Old"}
        }))
        .unwrap();
        let mut blobs = FakeBlobs;
        let local = insert_contact_card(&c, &card, &res, &mut blobs).unwrap();
        let changed: ContactCard = serde_json::from_value(serde_json::json!({
            "id":"C1","addressBookIds":{"AB":true},"uid":"u-1","name":{"full":"New Name"}
        }))
        .unwrap();
        update_contact_card(&c, local, &changed, &res, &mut blobs).unwrap();
        assert_eq!(one_row(&c, "contact_cards"), 1);
        let (uid, data): (String, String) = c
            .query_row(
                &format!("{CONTACT_CARD_SELECT} WHERE id=?1"),
                params![local],
                |r| Ok((r.get(1)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(uid, "u-1");
        let stored: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(stored["name"]["full"], Value::from("New Name"));
    }

    #[test]
    fn delta_update_calendar_event_rewrites_data_and_columns() {
        let c = mem();
        let res = MapResolver {
            to_local: HashMap::from([((ObjectType::Calendar, "CAL".to_owned()), 5)]),
            to_target: HashMap::new(),
        };
        let ev: CalendarEvent = serde_json::from_value(serde_json::json!({
            "id":"EV1","calendarIds":{"CAL":true},"isDraft":false,
            "useDefaultAlerts":false,"title":"Old","@type":"Event"
        }))
        .unwrap();
        let mut blobs = FakeBlobs;
        let local = insert_calendar_event(&c, &ev, &res, &mut blobs).unwrap();
        let changed: CalendarEvent = serde_json::from_value(serde_json::json!({
            "id":"EV1","calendarIds":{"CAL":true},"isDraft":true,
            "useDefaultAlerts":false,"title":"Rescheduled","@type":"Event"
        }))
        .unwrap();
        update_calendar_event(&c, local, &changed, &res, &mut blobs).unwrap();
        assert_eq!(one_row(&c, "calendar_events"), 1);
        let (dr, data): (i64, String) = c
            .query_row(
                &format!("{CALENDAR_EVENT_SELECT} AND id=?1"),
                params![local],
                |r| Ok((r.get(2)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(dr, 1, "isDraft column updated");
        let stored: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(stored["title"], Value::from("Rescheduled"));
    }
}
