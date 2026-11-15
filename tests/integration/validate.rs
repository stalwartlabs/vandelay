/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use vandelay::logging::{Level, Logger};
use vandelay::net::http::TlsPolicy;
use vandelay::sync::{RunMode, RunOptions};

pub fn tmp_archive(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "vandelay-container-{tag}-{}-{}.sqlite",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    let _ = std::fs::remove_file(&p);
    p
}

pub fn common(archive: &Path) -> RunOptions {
    RunOptions {
        archive: archive.to_path_buf(),
        threads: 2,
        mode: RunMode::Apply,
        max_retries: 3,
        tls: TlsPolicy::AcceptInvalid,
        logger: Logger::new(Level::Default),
    }
}

pub fn open_archive(archive: &Path) -> Connection {
    Connection::open(archive).expect("open archive")
}

pub fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap_or(0)
}

pub fn blob_bytes(conn: &Connection, blob_id: i64) -> Vec<u8> {
    conn.query_row("SELECT data FROM blobs WHERE id = ?1", [blob_id], |r| {
        r.get::<_, Vec<u8>>(0)
    })
    .expect("blob fetch")
}

pub fn mailbox_path(conn: &Connection, mailbox_id: i64, sep: char) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut cur = Some(mailbox_id);
    while let Some(id) = cur {
        let (name, parent): (String, Option<i64>) = conn
            .query_row(
                "SELECT name, parent_id FROM mailboxes WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("mailbox path");
        parts.push(name);
        cur = parent;
    }
    parts.reverse();
    parts.join(&sep.to_string())
}

pub fn file_path(conn: &Connection, node_id: i64) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut cur = Some(node_id);
    while let Some(id) = cur {
        let (name, parent): (String, Option<i64>) = conn
            .query_row(
                "SELECT name, parent_id FROM file_nodes WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("file path");
        parts.push(name);
        cur = parent;
    }
    parts.reverse();
    parts.join("/")
}

pub fn emails_in_mailbox(conn: &Connection, mailbox_id: i64) -> i64 {
    conn.query_row(
        "SELECT count(*) FROM emails
         WHERE EXISTS (SELECT 1 FROM json_each(emails.mailbox_ids) j WHERE j.value = ?1)",
        [mailbox_id],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

pub fn email_rows_for_blob(conn: &Connection, blob_id: i64) -> i64 {
    conn.query_row(
        "SELECT count(*) FROM emails WHERE blob_id = ?1",
        [blob_id],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

pub fn blob_id_for_hash(conn: &Connection, hash: &[u8]) -> Option<i64> {
    conn.query_row("SELECT id FROM blobs WHERE hash = ?1", [hash], |r| r.get(0))
        .ok()
}

pub fn mailbox_id_by_name(conn: &Connection, name: &str) -> Option<i64> {
    conn.query_row("SELECT id FROM mailboxes WHERE name = ?1", [name], |r| {
        r.get(0)
    })
    .ok()
}

pub fn mailbox_id_by_path(conn: &Connection, path: &str, sep: char) -> Option<i64> {
    let mut parent: Option<i64> = None;
    for segment in path.split(sep) {
        let id: i64 = match parent {
            Some(p) => conn
                .query_row(
                    "SELECT id FROM mailboxes WHERE parent_id = ?1 AND name = ?2",
                    rusqlite::params![p, segment],
                    |r| r.get(0),
                )
                .ok()?,
            None => conn
                .query_row(
                    "SELECT id FROM mailboxes WHERE parent_id IS NULL AND name = ?1",
                    [segment],
                    |r| r.get(0),
                )
                .ok()?,
        };
        parent = Some(id);
    }
    parent
}

pub fn file_node_id_by_path(conn: &Connection, segments: &[&str]) -> Option<i64> {
    let mut parent: Option<i64> = None;
    for segment in segments {
        let id: i64 = match parent {
            Some(p) => conn
                .query_row(
                    "SELECT id FROM file_nodes WHERE parent_id = ?1 AND name = ?2",
                    rusqlite::params![p, segment],
                    |r| r.get(0),
                )
                .ok()?,
            None => conn
                .query_row(
                    "SELECT id FROM file_nodes WHERE parent_id IS NULL AND name = ?1",
                    [segment],
                    |r| r.get(0),
                )
                .ok()?,
        };
        parent = Some(id);
    }
    parent
}

pub fn collection_names(conn: &Connection, table: &str) -> HashSet<String> {
    let mut stmt = conn
        .prepare(&format!("SELECT name FROM {table}"))
        .expect("prepare");
    stmt.query_map([], |r| r.get::<_, String>(0))
        .expect("query")
        .filter_map(|r| r.ok())
        .collect()
}

pub fn collection_id_by_name(conn: &Connection, table: &str, name: &str) -> Option<i64> {
    conn.query_row(
        &format!("SELECT id FROM {table} WHERE name = ?1"),
        [name],
        |r| r.get(0),
    )
    .ok()
}

pub fn calendar_event_count(conn: &Connection, calendar_id: i64) -> i64 {
    conn.query_row(
        "SELECT count(*) FROM calendar_events
         WHERE EXISTS (SELECT 1 FROM json_each(calendar_events.calendar_ids) j WHERE j.value = ?1)",
        [calendar_id],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

pub fn contact_card_count(conn: &Connection, address_book_id: i64) -> i64 {
    conn.query_row(
        "SELECT count(*) FROM contact_cards
         WHERE EXISTS (SELECT 1 FROM json_each(contact_cards.address_book_ids) j WHERE j.value = ?1)",
        [address_book_id],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

pub fn all_event_uids(conn: &Connection) -> HashSet<String> {
    let mut stmt = conn
        .prepare(
            "SELECT json_extract(data, '$.uid') FROM calendar_events
             WHERE json_extract(data, '$.uid') IS NOT NULL",
        )
        .expect("prepare");
    stmt.query_map([], |r| r.get::<_, String>(0))
        .expect("query")
        .filter_map(|r| r.ok())
        .collect()
}

pub fn all_contact_uids(conn: &Connection) -> HashSet<String> {
    let mut stmt = conn
        .prepare("SELECT uid FROM contact_cards")
        .expect("prepare");
    stmt.query_map([], |r| r.get::<_, String>(0))
        .expect("query")
        .filter_map(|r| r.ok())
        .collect()
}

pub fn keywords_for_blob(conn: &Connection, blob_id: i64) -> Vec<String> {
    let json: String = conn
        .query_row(
            "SELECT keywords FROM emails WHERE blob_id = ?1 LIMIT 1",
            [blob_id],
            |r| r.get(0),
        )
        .expect("keywords fetch");
    serde_json::from_str::<Vec<String>>(&json).expect("keywords json")
}

pub fn event_data_by_uid(conn: &Connection, uid: &str) -> Option<serde_json::Value> {
    let raw: String = conn
        .query_row(
            "SELECT data FROM calendar_events WHERE json_extract(data,'$.uid') = ?1",
            [uid],
            |r| r.get(0),
        )
        .ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn contact_data_by_uid(conn: &Connection, uid: &str) -> Option<serde_json::Value> {
    let raw: String = conn
        .query_row(
            "SELECT data FROM contact_cards WHERE uid = ?1",
            [uid],
            |r| r.get(0),
        )
        .ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn unfold_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let l = line.trim_end_matches('\r');
        if l.starts_with(' ') || l.starts_with('\t') {
            out.push_str(&l[1..]);
        } else {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(l);
        }
    }
    out
}

pub fn extract_property(text: &str, name: &str) -> Option<String> {
    component_top_level_properties(text, name)
        .into_iter()
        .next()
}

pub fn extract_property_all(text: &str, name: &str) -> Vec<String> {
    component_top_level_properties(text, name)
}

fn component_top_level_properties(text: &str, name: &str) -> Vec<String> {
    let value_colon = format!("{name}:");
    let value_semi = format!("{name};");
    let mut out = Vec::new();
    let mut depth: i32 = 0;
    let mut item_depth: Option<i32> = None;
    for line in unfold_text(text).lines() {
        let l = line.trim_end_matches('\r');
        if l.starts_with("BEGIN:") {
            depth += 1;
            if matches!(l, "BEGIN:VEVENT" | "BEGIN:VTODO" | "BEGIN:VCARD") {
                item_depth = Some(depth);
            }
            continue;
        }
        if l.starts_with("END:") {
            if Some(depth) == item_depth {
                item_depth = None;
            }
            depth -= 1;
            continue;
        }
        if item_depth.is_some_and(|d| d == depth) {
            if let Some(rest) = l.strip_prefix(&value_colon) {
                out.push(rest.to_owned());
            } else if l.starts_with(&value_semi)
                && let Some(colon) = l.find(':')
            {
                out.push(l[colon + 1..].to_owned());
            }
        }
    }
    out
}

pub fn grouped_property_values(text: &str, name: &str) -> Vec<String> {
    let unfolded = unfold_text(text);
    let mut out = Vec::new();
    for line in unfolded.lines() {
        let l = line.trim_end_matches('\r');
        let Some(colon) = l.find(':') else { continue };
        let head = &l[..colon];
        let name_part = head.split(';').next().unwrap_or(head);
        let bare = name_part.rsplit('.').next().unwrap_or(name_part);
        if bare.eq_ignore_ascii_case(name) {
            out.push(l[colon + 1..].to_owned());
        }
    }
    out
}

pub fn has_line_prefix(text: &str, prefix: &str) -> bool {
    let unfolded = unfold_text(text);
    unfolded.lines().any(|l| l.starts_with(prefix))
}

pub fn json_contains_string(value: &serde_json::Value, needle: &str) -> bool {
    let serialised = value.to_string();
    serialised.contains(needle)
}

pub fn assert_message_round_trip(conn: &Connection, raw: &[u8], target_mailbox: &str, label: &str) {
    let hash = blake3::hash(raw);
    let blob_id = blob_id_for_hash(conn, hash.as_bytes())
        .unwrap_or_else(|| panic!("{label}: blob for seeded message missing in archive"));
    let stored = blob_bytes(conn, blob_id);
    assert_eq!(
        stored, raw,
        "{label}: stored blob bytes differ from seeded raw"
    );
    let mailbox_id = mailbox_id_by_path(conn, target_mailbox, '/')
        .unwrap_or_else(|| panic!("{label}: target mailbox {target_mailbox} missing in archive"));
    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM emails
             WHERE blob_id = ?1
               AND EXISTS (SELECT 1 FROM json_each(mailbox_ids) j WHERE j.value = ?2)",
            rusqlite::params![blob_id, mailbox_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        count >= 1,
        "{label}: blob {blob_id} not linked to mailbox {target_mailbox} (id {mailbox_id})"
    );
}

pub fn assert_event_round_trip(conn: &Connection, source: &[u8], uid: &str, label: &str) {
    let text = std::str::from_utf8(source).expect("seeded ical bytes utf-8");
    let data = event_data_by_uid(conn, uid)
        .unwrap_or_else(|| panic!("{label}: event uid {uid} missing in archive"));
    let obj = data.as_object().expect("event data is object");

    let stored_uid = obj.get("uid").and_then(|v| v.as_str()).unwrap_or("");
    assert_eq!(
        stored_uid, uid,
        "{label}: event uid mismatch in stored data"
    );

    let summary = extract_property(text, "SUMMARY").unwrap_or_default();
    if !summary.is_empty() {
        let title = obj.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let expected = unescape_text(&summary);
        assert!(
            title == expected || title.contains(&expected),
            "{label}: event {uid} title mismatch: stored={title:?} expected to contain={expected:?}"
        );
    }

    if extract_property(text, "DESCRIPTION").is_some() {
        let descr = obj
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            !descr.is_empty(),
            "{label}: event {uid} description was seeded but stored data has no description"
        );
    }

    let start = obj.get("start").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        !start.is_empty(),
        "{label}: event {uid} stored data.start is empty"
    );

    if has_line_prefix(text, "RRULE:") || has_line_prefix(text, "RRULE;") {
        let has_rule = obj
            .get("recurrenceRules")
            .map(|v| !v.as_array().is_some_and(|a| a.is_empty()))
            .unwrap_or(false)
            || obj
                .get("recurrenceRule")
                .map(|v| {
                    !v.as_object().is_some_and(|o| o.is_empty())
                        && !v.as_array().is_some_and(|a| a.is_empty())
                })
                .unwrap_or(false);
        assert!(
            has_rule,
            "{label}: event {uid} seeded RRULE but no recurrenceRule/recurrenceRules in stored data: {obj:?}"
        );
    }

    if has_line_prefix(text, "BEGIN:VALARM") {
        let alerts = obj
            .get("alerts")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        assert!(
            !alerts.is_empty(),
            "{label}: event {uid} seeded VALARM but alerts empty"
        );
    }

    if has_line_prefix(text, "ORGANIZER:")
        || has_line_prefix(text, "ORGANIZER;")
        || has_line_prefix(text, "ATTENDEE:")
        || has_line_prefix(text, "ATTENDEE;")
    {
        let parts = obj
            .get("participants")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        assert!(
            !parts.is_empty(),
            "{label}: event {uid} seeded ORGANIZER/ATTENDEE but participants empty"
        );
        for addr in extract_property_all(text, "ATTENDEE") {
            if let Some(idx) = addr.find("mailto:")
                && let Some(end) = addr[idx + 7..].find([',', ';'])
            {
                let email = &addr[idx + 7..idx + 7 + end];
                assert!(
                    json_contains_string(&data, email),
                    "{label}: event {uid} attendee {email} not present in stored JSON"
                );
            } else if let Some(idx) = addr.find("mailto:") {
                let email = &addr[idx + 7..];
                assert!(
                    json_contains_string(&data, email),
                    "{label}: event {uid} attendee {email} not present in stored JSON"
                );
            }
        }
    }

    if let Some(location) = extract_property(text, "LOCATION") {
        let cleaned = unescape_text(&location);
        let probe = cleaned.split(',').next().unwrap_or(&cleaned).trim();
        if !probe.is_empty() {
            assert!(
                json_contains_string(&data, probe),
                "{label}: event {uid} location fragment {probe:?} not present in stored JSON"
            );
        }
    }

    if let Some(cats) = extract_property(text, "CATEGORIES") {
        let keywords = obj
            .get("keywords")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        assert!(
            !keywords.is_empty(),
            "{label}: event {uid} seeded CATEGORIES={cats} but keywords empty"
        );
        for cat in cats.split(',') {
            let cat = cat.trim();
            if cat.is_empty() {
                continue;
            }
            assert!(
                keywords.contains_key(cat),
                "{label}: event {uid} category {cat} not in keywords {keywords:?}"
            );
        }
    }
}

pub fn assert_contact_round_trip(conn: &Connection, source: &[u8], uid: &str, label: &str) {
    let text = std::str::from_utf8(source).expect("seeded vcard bytes utf-8");
    let data = contact_data_by_uid(conn, uid)
        .unwrap_or_else(|| panic!("{label}: contact uid {uid} missing in archive"));

    if let Some(fn_value) = extract_property(text, "FN") {
        let cleaned = unescape_text(&fn_value);
        assert!(
            json_contains_string(&data, &cleaned),
            "{label}: contact {uid} FN {cleaned:?} not present in stored JSContact"
        );
    }

    for email in extract_property_all(text, "EMAIL") {
        assert!(
            json_contains_string(&data, &email),
            "{label}: contact {uid} EMAIL {email:?} not present in stored JSContact"
        );
    }

    for tel in extract_property_all(text, "TEL") {
        assert!(
            json_contains_string(&data, &tel),
            "{label}: contact {uid} TEL {tel:?} not present in stored JSContact"
        );
    }

    if let Some(org) = extract_property(text, "ORG") {
        let primary = org.split(';').next().unwrap_or(&org).trim();
        if !primary.is_empty() {
            assert!(
                json_contains_string(&data, primary),
                "{label}: contact {uid} ORG {primary:?} not present in stored JSContact"
            );
        }
    }

    if let Some(nick) = extract_property(text, "NICKNAME") {
        assert!(
            json_contains_string(&data, &nick),
            "{label}: contact {uid} NICKNAME {nick:?} not present"
        );
    }

    for label_name in ["X-ABLABEL", "X-ABDATE"] {
        for value in grouped_property_values(text, label_name) {
            assert!(
                json_contains_string(&data, &value),
                "{label}: contact {uid} {label_name} {value:?} not preserved in stored JSContact vCard roundtrip block"
            );
        }
    }

    if let Some(bday) = extract_property(text, "BDAY") {
        let iso = format_vcard_date_as_iso(&bday);
        let present_raw = json_contains_string(&data, &bday);
        let present_iso = iso
            .as_deref()
            .map(|s| json_contains_string(&data, s))
            .unwrap_or(false);
        let obj = data.as_object().expect("contact data is object");
        let has_anniversaries = obj
            .get("anniversaries")
            .map(|v| !v.as_object().is_some_and(|o| o.is_empty()))
            .unwrap_or(false);
        assert!(
            present_raw || present_iso || has_anniversaries,
            "{label}: contact {uid} BDAY {bday:?} not present in stored JSContact (no anniversaries either)"
        );
    }
}

fn format_vcard_date_as_iso(bday: &str) -> Option<String> {
    let trimmed = bday.trim();
    if trimmed.len() == 8 && trimmed.bytes().all(|b| b.is_ascii_digit()) {
        Some(format!(
            "{}-{}-{}",
            &trimmed[..4],
            &trimmed[4..6],
            &trimmed[6..8]
        ))
    } else {
        None
    }
}

fn unescape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(&next) = chars.peek()
        {
            match next {
                ',' | ';' | '\\' => {
                    out.push(next);
                    chars.next();
                    continue;
                }
                'n' | 'N' => {
                    out.push('\n');
                    chars.next();
                    continue;
                }
                _ => {}
            }
        }
        out.push(c);
    }
    out
}

pub fn cleanup(archive: &Path) {
    let _ = std::fs::remove_file(archive);
    let mut wal = archive.as_os_str().to_owned();
    wal.push("-wal");
    let _ = std::fs::remove_file(Path::new(&wal));
    let mut shm = archive.as_os_str().to_owned();
    shm.push("-shm");
    let _ = std::fs::remove_file(Path::new(&shm));
}
