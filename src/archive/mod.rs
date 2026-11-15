/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

pub mod blobs;
mod defaults;
pub mod ids;
mod init;
pub mod rows;
pub mod sources;
pub mod state;

use std::fmt;
use std::path::Path;

use rusqlite::types::{FromSql, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, Transaction};

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

macro_rules! row_id {
    ($($name:ident),+) => {
        $(
            #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
            pub struct $name(i64);

            impl $name {
                pub fn get(self) -> i64 {
                    self.0
                }
            }

            impl ToSql for $name {
                fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                    Ok(ToSqlOutput::from(self.0))
                }
            }

            impl FromSql for $name {
                fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                    i64::column_result(value).map($name)
                }
            }

            impl fmt::Display for $name {
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    self.0.fmt(f)
                }
            }
        )+
    };
}

row_id!(SourceId, LocalId, BlobId);

impl LocalId {
    pub fn from_raw(raw: i64) -> LocalId {
        LocalId(raw)
    }
}

impl BlobId {
    pub fn from_raw(raw: i64) -> BlobId {
        BlobId(raw)
    }
}

pub struct Archive {
    conn: Connection,
}

impl Archive {
    pub fn open(path: impl AsRef<Path>) -> Result<Archive, ArchiveError> {
        Archive::open_inner(path.as_ref())
    }

    fn open_inner(path: &Path) -> Result<Archive, ArchiveError> {
        Ok(Archive {
            conn: Archive::connect(path)?,
        })
    }

    pub fn open_in_memory() -> Result<Archive, ArchiveError> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Archive::apply_schema(&conn)?;
        Ok(Archive { conn })
    }

    pub fn store(&self) -> Store<'_> {
        Store { conn: &self.conn }
    }

    pub fn transaction(&mut self) -> rusqlite::Result<Tx<'_>> {
        self.conn.transaction().map(Tx)
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }
}

pub struct Tx<'a>(Transaction<'a>);

impl Tx<'_> {
    pub fn store(&self) -> Store<'_> {
        Store { conn: &self.0 }
    }

    pub fn commit(self) -> rusqlite::Result<()> {
        self.0.commit()
    }
}

#[derive(Clone, Copy)]
pub struct Store<'a> {
    conn: &'a Connection,
}

impl<'a> Store<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Store<'a> {
        Store { conn }
    }

    pub fn begin(self) -> rusqlite::Result<Tx<'a>> {
        self.conn.unchecked_transaction().map(Tx)
    }

    pub fn connection(self) -> &'a Connection {
        self.conn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_archive_has_the_schema() {
        let archive = Archive::open_in_memory().unwrap();
        let n: i64 = archive
            .connection()
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'mailboxes'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn dropped_transaction_rolls_back() {
        let mut archive = Archive::open_in_memory().unwrap();
        {
            let tx = archive.transaction().unwrap();
            tx.store()
                .connection()
                .execute(
                    "INSERT INTO identities (name, email) VALUES ('a', 'a@example.com')",
                    [],
                )
                .unwrap();
        }
        let n: i64 = archive
            .connection()
            .query_row("SELECT count(*) FROM identities", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn ids_round_trip_through_sql() {
        let archive = Archive::open_in_memory().unwrap();
        let id: LocalId = archive
            .connection()
            .query_row("SELECT ?1", [LocalId::from_raw(42)], |r| r.get(0))
            .unwrap();
        assert_eq!(id, LocalId::from_raw(42));
        assert_eq!(id.to_string(), "42");
    }
}
