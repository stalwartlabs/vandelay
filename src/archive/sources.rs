/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt;

use rusqlite::types::{ToSql, ToSqlOutput};
use rusqlite::{Connection, OptionalExtension, params};

use super::{SourceId, Store};

macro_rules! source_kinds {
    ($($variant:ident => $token:literal, $label:literal;)+) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum SourceKind {
            $($variant,)+
        }

        impl SourceKind {
            pub const ALL: [SourceKind; [$(SourceKind::$variant),+].len()] =
                [$(SourceKind::$variant),+];

            pub const fn as_str(self) -> &'static str {
                match self {
                    $(SourceKind::$variant => $token,)+
                }
            }

            pub const fn label(self) -> &'static str {
                match self {
                    $(SourceKind::$variant => $label,)+
                }
            }

            pub fn parse(token: &str) -> Option<SourceKind> {
                hashify::map!(token.as_bytes(), SourceKind, $($token => SourceKind::$variant,)+)
                    .copied()
            }
        }
    };
}

source_kinds! {
    Jmap => "jmap", "JMAP";
    Imap => "imap", "IMAP";
    CalDav => "caldav", "CalDAV";
    CardDav => "carddav", "CardDAV";
    WebDav => "webdav", "WebDAV";
    ManageSieve => "managesieve", "ManageSieve";
    Maildir => "maildir", "Maildir";
    Takeout => "takeout", "Google Takeout";
    ExchangeEws => "exchange_ews", "Exchange (EWS)";
    ExchangeGraph => "exchange_graph", "Exchange (Graph)";
}

impl fmt::Display for SourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ToSql for SourceKind {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceKey<'k> {
    pub kind: SourceKind,
    pub url: &'k str,
    pub account_id: &'k str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceChange {
    Reject,
    Allow,
    Replace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtherSource {
    pub id: SourceId,
    pub url: String,
    pub account_id: String,
}

#[derive(Clone, Copy)]
pub struct Sources<'a> {
    conn: &'a Connection,
}

impl<'a> Store<'a> {
    pub fn sources(self) -> Sources<'a> {
        Sources { conn: self.conn }
    }
}

impl Sources<'_> {
    pub fn find(&self, key: &SourceKey<'_>) -> rusqlite::Result<Option<SourceId>> {
        self.conn
            .query_row(
                "SELECT id FROM sources WHERE kind = ?1 AND session_url = ?2 AND account_id = ?3",
                params![key.kind, key.url, key.account_id],
                |row| row.get(0),
            )
            .optional()
    }

    pub fn conflicting(&self, key: &SourceKey<'_>) -> rusqlite::Result<Option<(String, String)>> {
        self.conn
            .query_row(
                "SELECT session_url, account_id FROM sources
                 WHERE kind = ?1 AND NOT (session_url = ?2 AND account_id = ?3) LIMIT 1",
                params![key.kind, key.url, key.account_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
    }

    pub fn others(&self, key: &SourceKey<'_>) -> rusqlite::Result<Vec<OtherSource>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_url, account_id FROM sources
             WHERE kind = ?1 AND NOT (session_url = ?2 AND account_id = ?3) ORDER BY id",
        )?;
        stmt.query_map(params![key.kind, key.url, key.account_id], |row| {
            Ok(OtherSource {
                id: row.get(0)?,
                url: row.get(1)?,
                account_id: row.get(2)?,
            })
        })?
        .collect()
    }

    pub fn upsert(
        &self,
        key: &SourceKey<'_>,
        account_name: Option<&str>,
        username: &str,
    ) -> rusqlite::Result<SourceId> {
        self.conn.execute(
            "INSERT INTO sources (kind, session_url, account_id, account_name, username)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (kind, session_url, account_id)
             DO UPDATE SET account_name = excluded.account_name, username = excluded.username",
            params![key.kind, key.url, key.account_id, account_name, username],
        )?;
        self.find(key)
            .map(|found| found.unwrap_or_else(|| SourceId(self.conn.last_insert_rowid())))
    }

    pub fn delete(&self, id: SourceId) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM sources WHERE id = ?1", [id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Archive;

    // Every stored token parses back to its kind; labels are display names.
    #[test]
    fn source_kind_tokens_round_trip() {
        for kind in SourceKind::ALL {
            assert_eq!(SourceKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(SourceKind::parse("Jmap"), None);
        assert_eq!(SourceKind::Takeout.label(), "Google Takeout");
        assert_eq!(SourceKind::ExchangeGraph.label(), "Exchange (Graph)");
    }

    fn key<'k>(url: &'k str, acc: &'k str) -> SourceKey<'k> {
        SourceKey {
            kind: SourceKind::Jmap,
            url,
            account_id: acc,
        }
    }

    #[test]
    fn upsert_is_idempotent_and_returns_stable_id() {
        let archive = Archive::open_in_memory().unwrap();
        let sources = archive.store().sources();
        let k = key("https://a/jmap", "w");
        let id1 = sources.upsert(&k, Some("alice"), "alice").unwrap();
        let id2 = sources.upsert(&k, Some("alice2"), "alice").unwrap();
        assert_eq!(id1, id2);
        assert_eq!(sources.find(&k).unwrap(), Some(id1));
    }

    #[test]
    fn conflicting_source_detects_a_different_account() {
        let archive = Archive::open_in_memory().unwrap();
        let sources = archive.store().sources();
        sources
            .upsert(&key("https://a/jmap", "w"), None, "alice")
            .unwrap();
        assert!(
            sources
                .conflicting(&key("https://a/jmap", "w"))
                .unwrap()
                .is_none()
        );
        let conflict = sources
            .conflicting(&key("https://b/jmap", "v"))
            .unwrap()
            .unwrap();
        assert_eq!(conflict, ("https://a/jmap".to_owned(), "w".to_owned()));
    }

    #[test]
    fn other_sources_lists_every_other_source_of_the_kind() {
        let archive = Archive::open_in_memory().unwrap();
        let sources = archive.store().sources();
        let a = sources
            .upsert(&key("https://a/jmap", "w"), None, "alice")
            .unwrap();
        let b = sources
            .upsert(&key("https://b/jmap", "w"), None, "alice")
            .unwrap();
        sources
            .upsert(&key("https://c/jmap", "w"), None, "alice")
            .unwrap();
        let ids: Vec<SourceId> = sources
            .others(&key("https://c/jmap", "w"))
            .unwrap()
            .into_iter()
            .map(|o| o.id)
            .collect();
        assert_eq!(ids, [a, b]);
        let imap = SourceKey {
            kind: SourceKind::Imap,
            ..key("https://c/jmap", "w")
        };
        assert!(sources.others(&imap).unwrap().is_empty());
    }

    #[test]
    fn stored_kind_strings_are_unchanged() {
        let kinds = [
            SourceKind::Jmap,
            SourceKind::Imap,
            SourceKind::CalDav,
            SourceKind::CardDav,
            SourceKind::WebDav,
            SourceKind::ManageSieve,
            SourceKind::Maildir,
            SourceKind::Takeout,
            SourceKind::ExchangeEws,
            SourceKind::ExchangeGraph,
        ]
        .map(SourceKind::as_str);
        assert_eq!(
            kinds,
            [
                "jmap",
                "imap",
                "caldav",
                "carddav",
                "webdav",
                "managesieve",
                "maildir",
                "takeout",
                "exchange_ews",
                "exchange_graph"
            ]
        );
    }

    #[test]
    fn delete_removes_the_source() {
        let archive = Archive::open_in_memory().unwrap();
        let sources = archive.store().sources();
        let k = key("https://a/jmap", "w");
        let id = sources.upsert(&k, None, "alice").unwrap();
        sources.delete(id).unwrap();
        assert_eq!(sources.find(&k).unwrap(), None);
    }
}
