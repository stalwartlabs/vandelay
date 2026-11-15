/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod list;
mod tree;

use std::fmt::Display;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use crate::archive::sources::SourceKind;
use crate::archive::{Archive, BlobId};
use crate::console::{ByteSize, Column, Console, Grouped, Initial, Severity, Styled, Table, Tone};
use crate::error::Error;
use crate::types::ObjectType;

const KEY_WIDTH: usize = 7;
const SUMMARY_COLUMNS: [Column; 2] = [Column::left("Type"), Column::right("Objects")];

pub struct Inspect {
    pub archive: PathBuf,
    pub target: Option<ObjectType>,
    pub page: Page,
    pub console: Console,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Page {
    pub limit: Option<usize>,
    pub offset: usize,
}

impl Inspect {
    pub fn run(self) -> Result<(), Error> {
        let archive = Archive::open(&self.archive)?;
        let stdout = io::stdout();
        let mut out = stdout.lock();
        self.write(View::new(archive.connection(), self.console.out), &mut out)
    }

    fn write(&self, view: View<'_>, out: &mut impl Write) -> Result<(), Error> {
        match self.target {
            None => view.write_summary(&self.archive, out),
            Some(ObjectType::Mailbox) => {
                self.page.note_ignored(self.console, ObjectType::Mailbox);
                view.write_mailboxes(out)
            }
            Some(ObjectType::FileNode) => {
                self.page.note_ignored(self.console, ObjectType::FileNode);
                view.write_file_nodes(out)
            }
            Some(ty) => view.write_list(ty, self.page, out),
        }
    }
}

impl Page {
    fn note_ignored(self, console: Console, ty: ObjectType) {
        if self.limit.is_some() || self.offset > 0 {
            eprintln!(
                "{} --limit and --offset are ignored for {}; the tree always shows the full hierarchy",
                console.err.severity(Severity::Note),
                Initial::Lower(ty.label())
            );
        }
    }
}

struct SourceRow {
    kind: String,
    session_url: String,
    account_id: String,
    account_name: Option<String>,
}

#[derive(Clone, Copy)]
struct View<'c> {
    conn: &'c Connection,
    styled: Styled,
}

impl<'c> View<'c> {
    fn new(conn: &'c Connection, styled: Styled) -> View<'c> {
        View { conn, styled }
    }

    fn key(self, out: &mut impl Write, key: &str, value: impl Display) -> io::Result<()> {
        writeln!(
            out,
            "{}  {value}",
            self.styled
                .paint(Tone::Heading, format_args!("{key:<KEY_WIDTH$}"))
        )
    }

    fn write_summary(self, archive: &Path, out: &mut impl Write) -> Result<(), Error> {
        let styled = self.styled;
        self.key(out, "Archive", archive.display())?;
        match self.source()? {
            Some(src) => {
                let kind = match SourceKind::parse(&src.kind) {
                    Some(kind) => kind.label(),
                    None => src.kind.as_str(),
                };
                self.key(out, "Source", format_args!("{kind}, {}", src.session_url))?;
                match &src.account_name {
                    Some(name) => {
                        self.key(out, "Account", format_args!("{name} ({})", src.account_id))?
                    }
                    None => self.key(out, "Account", &src.account_id)?,
                }
            }
            None => self.key(out, "Source", styled.paint(Tone::Dim, "none recorded"))?,
        }
        let (blob_count, blob_bytes) = self.blob_totals()?;
        self.key(
            out,
            "Blobs",
            format_args!("{}, {}", Grouped(blob_count), ByteSize(blob_bytes)),
        )?;
        writeln!(out)?;
        let mut table = Table::new(&SUMMARY_COLUMNS);
        for ty in ObjectType::ALL {
            let n = self.count(ty)?;
            table
                .cell(ty.label(), (n == 0).then_some(Tone::Dim))
                .count(n, None);
        }
        table.write(styled, out)?;
        Ok(())
    }

    fn source(self) -> Result<Option<SourceRow>, Error> {
        Ok(self
            .conn
            .query_row(
                "SELECT kind, session_url, account_id, account_name FROM sources LIMIT 1",
                [],
                |r| {
                    Ok(SourceRow {
                        kind: r.get(0)?,
                        session_url: r.get(1)?,
                        account_id: r.get(2)?,
                        account_name: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    fn count(self, ty: ObjectType) -> Result<u64, Error> {
        let n: i64 =
            self.conn
                .query_row(&format!("SELECT COUNT(*) FROM {}", ty.table()), [], |r| {
                    r.get(0)
                })?;
        Ok(n as u64)
    }

    fn blob_totals(self) -> Result<(u64, u64), Error> {
        let (count, bytes): (i64, i64) = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(length(data)), 0) FROM blobs",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((count as u64, bytes.max(0) as u64))
    }

    fn blob_summary(self, blob: BlobId) -> Result<String, Error> {
        let row = self
            .conn
            .query_row(
                "SELECT length(data), substr(lower(hex(hash)), 1, 12) FROM blobs WHERE id = ?1",
                params![blob],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?;
        Ok(match row {
            Some((len, hex)) => format!("{} blake3={hex}", ByteSize(len.max(0) as u64)),
            None => format!("(missing blob #{blob})"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::sources::{SourceKey, SourceKind};

    #[test]
    fn blob_summary_reports_size_and_short_hash() {
        let a = Archive::open_in_memory().unwrap();
        let id = a.store().blobs().intern(b"hello world").unwrap();
        let s = View::new(a.connection(), Styled::Plain)
            .blob_summary(id)
            .unwrap();
        assert!(s.starts_with("11 B blake3="), "got {s}");
        assert!(s.len() > "11 B blake3=".len());
    }

    #[test]
    fn blob_summary_handles_missing_id() {
        let a = Archive::open_in_memory().unwrap();
        let s = View::new(a.connection(), Styled::Plain)
            .blob_summary(BlobId::from_raw(9_999))
            .unwrap();
        assert_eq!(s, "(missing blob #9999)");
    }

    #[test]
    fn summary_lists_every_type_and_blob_totals() {
        let a = Archive::open_in_memory().unwrap();
        a.store()
            .sources()
            .upsert(
                &SourceKey {
                    kind: SourceKind::Jmap,
                    url: "https://example.test/jmap",
                    account_id: "u-1",
                },
                Some("alice@example.test"),
                "alice@example.test",
            )
            .unwrap();
        a.connection()
            .execute("INSERT INTO mailboxes (name) VALUES (?1)", params!["Inbox"])
            .unwrap();
        a.store().blobs().intern(b"some bytes").unwrap();
        let mut buf = Vec::new();
        View::new(a.connection(), Styled::Plain)
            .write_summary(Path::new("test.sqlite"), &mut buf)
            .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("Archive  test.sqlite\n"), "got:\n{s}");
        assert!(s.contains("Source   JMAP, https://example.test/jmap\n"));
        assert!(s.contains("Account  alice@example.test (u-1)\n"));
        assert!(s.contains("Blobs    1, 10 B\n"));
        for ty in ObjectType::ALL {
            assert!(s.contains(ty.label()), "missing {} in:\n{s}", ty.label());
        }
    }

    #[test]
    fn summary_without_source_reports_none() {
        let a = Archive::open_in_memory().unwrap();
        let mut buf = Vec::new();
        View::new(a.connection(), Styled::Plain)
            .write_summary(Path::new("x.sqlite"), &mut buf)
            .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("Source   none recorded"));
    }

    #[test]
    fn summary_table_aligns_labels_and_grouped_counts() {
        // Pins the summary table: sentence-case labels, right-aligned grouped counts.
        let a = Archive::open_in_memory().unwrap();
        for n in 0..1_234 {
            a.connection()
                .execute(
                    "INSERT INTO identities (name, email) VALUES (?1, ?1)",
                    params![n.to_string()],
                )
                .unwrap();
        }
        let mut buf = Vec::new();
        View::new(a.connection(), Styled::Plain)
            .write_summary(Path::new("x.sqlite"), &mut buf)
            .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(
            s.contains("  Type                    Objects\n  Mailboxes                     0\n  Emails                        0\n  Identities                1,234\n"),
            "got:\n{s}"
        );
    }
}
