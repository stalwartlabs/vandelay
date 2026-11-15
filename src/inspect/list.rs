/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::io::Write;

use rusqlite::Row;
use serde_json::Value;

use super::{Page, View};
use crate::archive::BlobId;
use crate::console::{ByteSize, Column as TableColumn, Grouped, Initial, Table, Tone};
use crate::error::Error;
use crate::types::ObjectType;

const NULL_TEXT: &str = "-";
const DASH: &str = "-";
const MISSING: &str = "missing";
const EMAIL_COLUMNS: [TableColumn; 6] = [
    TableColumn::right("ID"),
    TableColumn::right("Size"),
    TableColumn::left("Received"),
    TableColumn::left("Mailboxes"),
    TableColumn::left("Keywords"),
    TableColumn::left("Message-ID"),
];
const EMAIL_BATCH_ROWS: usize = 256;
const BLOB_KEY: &str = "@blob";
const MESSAGE_IDS_KEY: &str = "m";

struct LimitClause(Page);

impl fmt::Display for LimitClause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.limit {
            Some(n) => write!(f, " LIMIT {n} OFFSET {}", self.0.offset),
            None if self.0.offset > 0 => write!(f, " LIMIT -1 OFFSET {}", self.0.offset),
            None => Ok(()),
        }
    }
}

struct Column<'t>(&'t str);

impl Column<'_> {
    fn or_null(value: Option<String>) -> String {
        value.unwrap_or_else(|| NULL_TEXT.to_owned())
    }

    fn flag(value: i64) -> String {
        (if value != 0 { "yes" } else { "no" }).to_owned()
    }

    fn local_ids(&self) -> Vec<i64> {
        serde_json::from_str::<Vec<Value>>(self.0)
            .map(|arr| arr.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default()
    }

    fn keyword_list(&self, out: &mut String) {
        let names = serde_json::from_str::<Vec<String>>(self.0).unwrap_or_default();
        for (n, name) in names.iter().enumerate() {
            if n > 0 {
                out.push_str(", ");
            }
            out.push_str(name);
        }
    }

    fn first_message_id(&self, out: &mut String) {
        if let Some(id) = serde_json::from_str::<Value>(self.0)
            .ok()
            .as_ref()
            .and_then(|v| v.get(MESSAGE_IDS_KEY))
            .and_then(|m| m.get(0))
            .and_then(Value::as_str)
        {
            out.push_str(id);
        }
    }

    fn redacted_json(&self) -> String {
        let Ok(mut value) = serde_json::from_str::<Value>(self.0) else {
            return self.0.to_owned();
        };
        Column::redact(&mut value);
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| self.0.to_owned())
    }

    fn redact(value: &mut Value) {
        match value {
            Value::Object(map) => {
                if let Some(blob) = map.get(BLOB_KEY).and_then(Value::as_i64) {
                    map.clear();
                    map.insert(
                        BLOB_KEY.to_owned(),
                        Value::String(format!("local #{blob} (omitted)")),
                    );
                    return;
                }
                map.values_mut().for_each(Column::redact);
            }
            Value::Array(arr) => arr.iter_mut().for_each(Column::redact),
            _ => {}
        }
    }
}

struct Names {
    mailboxes: HashMap<i64, String>,
    address_books: HashMap<i64, String>,
    calendars: HashMap<i64, String>,
}

impl Names {
    fn load(view: View<'_>, ty: ObjectType) -> Result<Names, Error> {
        let load = |wanted: ObjectType, sql: &str| -> Result<HashMap<i64, String>, Error> {
            if ty != wanted {
                return Ok(HashMap::new());
            }
            let mut stmt = view.conn.prepare(sql)?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<Result<_, _>>()?)
        };
        Ok(Names {
            mailboxes: load(ObjectType::Email, "SELECT id, name FROM mailboxes")?,
            address_books: load(
                ObjectType::ContactCard,
                "SELECT id, name FROM address_books",
            )?,
            calendars: load(ObjectType::CalendarEvent, "SELECT id, name FROM calendars")?,
        })
    }

    fn join(&self, out: &mut String, kind: ObjectType, ids: &[i64]) {
        let map = match kind {
            ObjectType::Mailbox => &self.mailboxes,
            ObjectType::AddressBook => &self.address_books,
            ObjectType::Calendar => &self.calendars,
            _ => return,
        };
        for (n, id) in ids.iter().enumerate() {
            if n > 0 {
                out.push_str(", ");
            }
            let _ = match map.get(id) {
                Some(name) => write!(out, "{name}"),
                None => write!(out, "#{id} (missing)"),
            };
        }
    }

    fn render(&self, kind: ObjectType, ids: &[i64]) -> String {
        let mut out = String::new();
        self.join(&mut out, kind, ids);
        out
    }
}

struct Listing<'v, 'o, W> {
    view: View<'v>,
    page: Page,
    out: &'o mut W,
    written: u64,
}

impl View<'_> {
    pub(super) fn write_list(
        self,
        ty: ObjectType,
        page: Page,
        out: &mut impl Write,
    ) -> Result<(), Error> {
        let total = self.count(ty)?;
        let mut listing = Listing {
            view: self,
            page,
            out,
            written: 0,
        };
        listing.header(ty, total)?;
        if total == 0 {
            writeln!(
                listing.out,
                "{}",
                self.styled.paint(
                    Tone::Dim,
                    format_args!("No {} in the archive", Initial::Lower(ty.label()))
                )
            )?;
            return Ok(());
        }
        let names = Names::load(self, ty)?;
        listing.records(ty, &names)?;
        if let Some(lim) = page.limit {
            let next_offset = (page.offset as u64) + listing.written;
            if next_offset < total {
                writeln!(
                    listing.out,
                    "{}",
                    self.styled.paint(
                        Tone::Dim,
                        format_args!(
                            "{} more; continue with --offset {next_offset} --limit {lim}",
                            Grouped(total - next_offset)
                        )
                    )
                )?;
            }
        }
        Ok(())
    }
}

impl<W: Write> Listing<'_, '_, W> {
    fn header(&mut self, ty: ObjectType, total: u64) -> Result<(), Error> {
        let styled = self.view.styled;
        let name = styled.paint(Tone::Title, ty.label());
        let first = Grouped(self.page.offset as u64 + 1);
        match self.page.limit {
            None if self.page.offset == 0 => writeln!(self.out, "{name}  ({})", Grouped(total))?,
            None => writeln!(self.out, "{name}  ({}, from {first})", Grouped(total))?,
            Some(lim) => writeln!(
                self.out,
                "{name}  ({}, showing {first} to {})",
                Grouped(total),
                Grouped(total.min(self.page.offset as u64 + lim as u64))
            )?,
        }
        writeln!(self.out)?;
        Ok(())
    }

    fn records(&mut self, ty: ObjectType, names: &Names) -> Result<(), Error> {
        match ty {
            ObjectType::Identity => self.each(
                ObjectType::Identity,
                "SELECT id, name, email, reply_to, bcc, text_signature, html_signature
                 FROM identities ORDER BY id",
                |_, row| {
                    Ok(vec![
                        ("Name", row.get(1)?),
                        ("Email", row.get(2)?),
                        ("Reply to", Column::or_null(row.get(3)?)),
                        ("Bcc", Column::or_null(row.get(4)?)),
                        ("Text signature", row.get(5)?),
                        ("HTML signature", row.get(6)?),
                    ])
                },
            ),
            ObjectType::Email => self.emails(names),
            ObjectType::SieveScript => self.each(
                ObjectType::SieveScript,
                "SELECT id, name, is_active, blob_id FROM sieve_scripts ORDER BY id",
                |view, row| {
                    Ok(vec![
                        ("Name", Column::or_null(row.get(1)?)),
                        ("Active", Column::flag(row.get(2)?)),
                        ("Content", view.blob_summary(row.get::<_, BlobId>(3)?)?),
                    ])
                },
            ),
            ObjectType::AddressBook => self.each(
                ObjectType::AddressBook,
                "SELECT id, name, description, sort_order, is_default, is_subscribed
                 FROM address_books ORDER BY id",
                |_, row| {
                    Ok(vec![
                        ("Name", row.get(1)?),
                        ("Description", Column::or_null(row.get(2)?)),
                        ("Sort order", row.get::<_, i64>(3)?.to_string()),
                        ("Default", Column::flag(row.get(4)?)),
                        ("Subscribed", Column::flag(row.get(5)?)),
                    ])
                },
            ),
            ObjectType::ContactCard => self.each(
                ObjectType::ContactCard,
                "SELECT id, uid, address_book_ids, data FROM contact_cards ORDER BY id",
                |_, row| {
                    Ok(vec![
                        ("UID", row.get(1)?),
                        (
                            "Address books",
                            names.render(
                                ObjectType::AddressBook,
                                &Column(&row.get::<_, String>(2)?).local_ids(),
                            ),
                        ),
                        ("Data", Column(&row.get::<_, String>(3)?).redacted_json()),
                    ])
                },
            ),
            ObjectType::Calendar => self.each(
                ObjectType::Calendar,
                "SELECT id, name, description, color, sort_order, is_subscribed, is_visible, is_default,
                        include_in_availability, default_alerts_with_time, default_alerts_without_time,
                        time_zone
                 FROM calendars ORDER BY id",
                |_, row| {
                    Ok(vec![
                        ("Name", row.get(1)?),
                        ("Description", Column::or_null(row.get(2)?)),
                        ("Colour", Column::or_null(row.get(3)?)),
                        ("Sort order", row.get::<_, i64>(4)?.to_string()),
                        ("Subscribed", Column::flag(row.get(5)?)),
                        ("Visible", Column::flag(row.get(6)?)),
                        ("Default", Column::flag(row.get(7)?)),
                        ("Availability", row.get(8)?),
                        ("Default timed alerts", Column::or_null(row.get(9)?)),
                        (
                            "Default all-day alerts",
                            Column::or_null(row.get(10)?),
                        ),
                        ("Time zone", Column::or_null(row.get(11)?)),
                    ])
                },
            ),
            ObjectType::CalendarEvent => self.each(
                ObjectType::CalendarEvent,
                "SELECT id, calendar_ids, is_draft, use_default_alerts, data
                 FROM calendar_events ORDER BY id",
                |_, row| {
                    Ok(vec![
                        (
                            "Calendars",
                            names.render(
                                ObjectType::Calendar,
                                &Column(&row.get::<_, String>(1)?).local_ids(),
                            ),
                        ),
                        ("Draft", Column::flag(row.get(2)?)),
                        ("Default alerts", Column::flag(row.get(3)?)),
                        ("Data", Column(&row.get::<_, String>(4)?).redacted_json()),
                    ])
                },
            ),
            ObjectType::ParticipantIdentity => self.each(
                ObjectType::ParticipantIdentity,
                "SELECT id, name, calendar_address, is_default
                 FROM participant_identities ORDER BY id",
                |_, row| {
                    Ok(vec![
                        ("Name", row.get(1)?),
                        ("Calendar address", row.get(2)?),
                        ("Default", Column::flag(row.get(3)?)),
                    ])
                },
            ),
            ObjectType::Mailbox | ObjectType::FileNode => Ok(()),
        }
    }

    fn each(
        &mut self,
        ty: ObjectType,
        select: &str,
        fields: impl Fn(View<'_>, &Row<'_>) -> Result<Vec<(&'static str, String)>, Error>,
    ) -> Result<(), Error> {
        let sql = format!("{select}{}", LimitClause(self.page));
        let mut stmt = self.view.conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let fields = fields(self.view, row)?;
            self.record(ty, id, &fields)?;
            self.written += 1;
        }
        Ok(())
    }

    fn record(&mut self, ty: ObjectType, id: i64, fields: &[(&str, String)]) -> Result<(), Error> {
        let styled = self.view.styled;
        writeln!(
            self.out,
            "{}",
            styled.paint(
                Tone::Heading,
                format_args!("{} {id}", Initial::Upper(ty.noun()))
            )
        )?;
        let width = fields.iter().map(|(l, _)| l.len()).max().unwrap_or(0);
        let pad = width + 4;
        for (label, value) in fields {
            let mut lines = value.split('\n');
            if let Some(first) = lines.next() {
                writeln!(
                    self.out,
                    "  {}  {first}",
                    styled.paint(Tone::Dim, format_args!("{label:<width$}"))
                )?;
            }
            for line in lines {
                writeln!(self.out, "{:pad$}{line}", "")?;
            }
        }
        writeln!(self.out)?;
        Ok(())
    }

    fn emails(&mut self, names: &Names) -> Result<(), Error> {
        let sql = format!(
            "SELECT e.id, length(b.data), e.received_at, e.mailbox_ids, e.keywords, e.message_match
             FROM emails e LEFT JOIN blobs b ON b.id = e.blob_id ORDER BY e.id{}",
            LimitClause(self.page)
        );
        let mut stmt = self.view.conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        let mut table = Table::new(&EMAIL_COLUMNS);
        let mut text = String::new();
        while let Some(row) = rows.next()? {
            table.cell(row.get::<_, i64>(0)?, Some(Tone::Dim));
            match row.get::<_, Option<i64>>(1)? {
                Some(len) => table.cell(ByteSize(len.max(0) as u64), None),
                None => table.cell(MISSING, Some(Tone::Bad)),
            };
            table.cell(row.get::<_, String>(2)?, None);
            text.clear();
            names.join(
                &mut text,
                ObjectType::Mailbox,
                &Column(&row.get::<_, String>(3)?).local_ids(),
            );
            table.cell(&text, None);
            text.clear();
            Column(&row.get::<_, String>(4)?).keyword_list(&mut text);
            Self::text_or_dash(&mut table, &text);
            text.clear();
            Column(&row.get::<_, String>(5)?).first_message_id(&mut text);
            Self::text_or_dash(&mut table, &text);
            self.written += 1;
            if table.rows() == EMAIL_BATCH_ROWS {
                self.flush_table(&mut table)?;
            }
        }
        if table.rows() > 0 {
            self.flush_table(&mut table)?;
        }
        Ok(())
    }

    fn flush_table(&mut self, table: &mut Table<'_>) -> Result<(), Error> {
        table.write(self.view.styled, self.out)?;
        writeln!(self.out)?;
        table.clear();
        Ok(())
    }

    fn text_or_dash(table: &mut Table<'_>, text: &str) {
        if text.is_empty() {
            table.cell(DASH, Some(Tone::Dim));
        } else {
            table.cell(text, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Archive;
    use crate::console::Styled;
    use rusqlite::params;

    fn list(a: &Archive, ty: ObjectType, limit: Option<usize>) -> String {
        let mut buf = Vec::new();
        View::new(a.connection(), Styled::Plain)
            .write_list(ty, Page { limit, offset: 0 }, &mut buf)
            .unwrap();
        String::from_utf8(buf).unwrap()
    }

    fn add_mailbox(a: &Archive, name: &str) -> i64 {
        a.connection()
            .execute("INSERT INTO mailboxes (name) VALUES (?1)", params![name])
            .unwrap();
        a.connection().last_insert_rowid()
    }

    #[test]
    fn empty_table_prints_none_message() {
        let a = Archive::open_in_memory().unwrap();
        let s = list(&a, ObjectType::Identity, None);
        assert!(s.contains("Identities  (0)"));
        assert!(s.contains("No identities in the archive"));
    }

    #[test]
    fn identity_record_uses_aligned_fields_and_redacts_nothing() {
        let a = Archive::open_in_memory().unwrap();
        a.connection()
            .execute(
                "INSERT INTO identities (name, email, text_signature) VALUES ('Alice', 'a@x.test', 'best,A')",
                [],
            )
            .unwrap();
        let s = list(&a, ObjectType::Identity, None);
        assert!(s.contains("Identity 1\n"));
        assert!(s.contains("  Name            Alice"), "got:\n{s}");
        assert!(s.contains("  Email           a@x.test"));
        assert!(s.contains("  Bcc             -"));
    }

    #[test]
    fn email_resolves_mailbox_names_and_summarises_blob() {
        let a = Archive::open_in_memory().unwrap();
        let inbox = add_mailbox(&a, "Inbox");
        let other = add_mailbox(&a, "Personal");
        let blob = a
            .store()
            .blobs()
            .intern(b"From: a@x.test\r\nSubject: hi\r\n\r\nhello")
            .unwrap();
        a.connection()
            .execute(
                "INSERT INTO emails (blob_id, received_at, mailbox_ids, keywords)
                 VALUES (?1, '2024-01-01T00:00:00Z', ?2, ?3)",
                params![
                    blob,
                    format!("[{inbox},{other}]"),
                    "[\"$seen\",\"Important\"]"
                ],
            )
            .unwrap();
        let s = list(&a, ObjectType::Email, None);
        assert!(s.contains("  ID  Size  Received"), "got:\n{s}");
        assert!(
            s.contains("   1  36 B  2024-01-01T00:00:00Z  Inbox, Personal  $seen, Important  -"),
            "got:\n{s}"
        );
    }

    // The email table is written in batches, so a long listing streams with bounded memory.
    #[test]
    fn email_table_is_written_in_batches() {
        let a = Archive::open_in_memory().unwrap();
        let blob = a.store().blobs().intern(b"Subject: x\r\n\r\nx").unwrap();
        let c = a.connection();
        for _ in 0..=EMAIL_BATCH_ROWS {
            c.execute(
                "INSERT INTO emails (blob_id, received_at, mailbox_ids)
                 VALUES (?1, '2024-01-01T00:00:00Z', '[]')",
                params![blob],
            )
            .unwrap();
        }
        let s = list(&a, ObjectType::Email, None);
        assert_eq!(s.matches("  ID  Size").count(), 2, "got:\n{s}");
        let last = EMAIL_BATCH_ROWS + 1;
        assert!(s.contains(&format!("\n  {last}  ")), "got:\n{s}");
    }

    #[test]
    fn contact_card_redacts_blob_sentinel_in_data() {
        let a = Archive::open_in_memory().unwrap();
        let c = a.connection();
        c.execute("INSERT INTO address_books (name) VALUES ('Personal')", [])
            .unwrap();
        c.execute(
            "INSERT INTO contact_cards (uid, address_book_ids, data) VALUES (?1, ?2, ?3)",
            params![
                "urn:uuid:1",
                "[1,9]",
                r#"{"name":{"full":"Alice"},"photos":{"p":{"@blob":17}}}"#
            ],
        )
        .unwrap();
        let s = list(&a, ObjectType::ContactCard, None);
        assert!(s.contains("Contact card 1\n"));
        assert!(s.contains("Personal, #9 (missing)"), "got:\n{s}");
        assert!(s.contains("local #17 (omitted)"));
        assert!(!s.contains("\"@blob\": 17"));
        assert!(
            s.contains("  Data           {\n                   \"name\": {"),
            "got:\n{s}"
        );
    }

    #[test]
    fn pagination_writes_continuation_hint_when_more_remain() {
        let a = Archive::open_in_memory().unwrap();
        for i in 0..5 {
            a.connection()
                .execute(
                    "INSERT INTO identities (name, email) VALUES (?1, ?2)",
                    params![format!("u{i}"), format!("u{i}@x.test")],
                )
                .unwrap();
        }
        let s = list(&a, ObjectType::Identity, Some(2));
        assert!(s.contains("Identities  (5, showing 1 to 2)"));
        assert!(s.contains("Identity 1\n"));
        assert!(s.contains("Identity 2\n"));
        assert!(!s.contains("Identity 3\n"));
        assert!(s.contains("3 more; continue with --offset 2 --limit 2"));
    }

    #[test]
    fn pagination_at_end_does_not_print_continuation() {
        let a = Archive::open_in_memory().unwrap();
        for i in 0..3 {
            a.connection()
                .execute(
                    "INSERT INTO identities (name, email) VALUES (?1, ?2)",
                    params![format!("u{i}"), format!("u{i}@x.test")],
                )
                .unwrap();
        }
        let s = list(&a, ObjectType::Identity, Some(5));
        assert!(s.contains("Identities  (3, showing 1 to 3)"), "got:\n{s}");
        assert!(!s.contains("more;"), "got:\n{s}");
    }

    #[test]
    fn column_helpers_keep_the_old_texts() {
        assert_eq!(Column("not json").redacted_json(), "not json");
        assert_eq!(Column("[1,\"x\",3]").local_ids(), vec![1, 3]);
        assert_eq!(Column("{}").local_ids(), Vec::<i64>::new());
        assert_eq!(
            LimitClause(Page {
                limit: None,
                offset: 3
            })
            .to_string(),
            " LIMIT -1 OFFSET 3"
        );
        assert_eq!(LimitClause(Page::default()).to_string(), "");
        let mut text = String::new();
        Column(r#"{"m":["<a@x>","<b@x>"]}"#).first_message_id(&mut text);
        Column("[\"$seen\",\"$flagged\"]").keyword_list(&mut text);
        assert_eq!(text, "<a@x>$seen, $flagged");
        assert_eq!(Column::flag(0), "no");
        assert_eq!(Column::or_null(None), "-");
    }
}
