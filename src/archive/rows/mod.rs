/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod address_books;
mod calendar_events;
mod calendars;
mod contact_cards;
mod emails;
mod file_nodes;
mod identities;
mod mailboxes;
mod participant_identities;
mod sieve_scripts;
mod vacation;

use std::collections::HashSet;
use std::fmt::Write as _;

use rusqlite::types::{FromSql, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, params};

use super::{LocalId, Store};

pub use self::address_books::AddressBookRecord;
pub use self::calendar_events::{CalendarEventRecord, EventDataType};
pub use self::calendars::CalendarRecord;
pub use self::contact_cards::ContactCardRecord;
pub use self::emails::EmailRecord;
pub use self::file_nodes::{FileNodeRecord, NodeType};
pub use self::identities::IdentityRecord;
pub use self::mailboxes::MailboxRecord;
pub use self::participant_identities::ParticipantIdentityRecord;
pub use self::sieve_scripts::{SieveScriptRecord, SieveScriptState};
pub use self::vacation::{StoredVacation, Vacation, VacationRecord};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Write {
    Changed,
    Unchanged,
}

impl Write {
    fn from_rows(rows: usize) -> Write {
        if rows > 0 {
            Write::Changed
        } else {
            Write::Unchanged
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subscribed {
    Yes,
    No,
}

impl From<bool> for Subscribed {
    fn from(subscribed: bool) -> Subscribed {
        if subscribed {
            Subscribed::Yes
        } else {
            Subscribed::No
        }
    }
}

impl ToSql for Subscribed {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(matches!(self, Subscribed::Yes)))
    }
}

impl FromSql for Subscribed {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        bool::column_result(value).map(Subscribed::from)
    }
}

struct LocalIdArray<'a>(&'a [LocalId]);

impl ToSql for LocalIdArray<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        let mut out = String::with_capacity(2 + self.0.len() * 4);
        out.push('[');
        for (i, id) in self.0.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "{id}");
        }
        out.push(']');
        Ok(ToSqlOutput::from(out))
    }
}

macro_rules! tables {
    ($($handle:ident => $accessor:ident, $table:literal;)+) => {
        $(
            #[derive(Clone, Copy)]
            pub struct $handle<'a> {
                conn: &'a Connection,
            }

            impl $handle<'_> {
                pub fn delete(&self, id: LocalId) -> rusqlite::Result<()> {
                    self.conn
                        .execute(concat!("DELETE FROM ", $table, " WHERE id = ?1"), [id])?;
                    Ok(())
                }
            }
        )+

        impl<'a> Store<'a> {
            $(
                pub fn $accessor(self) -> $handle<'a> {
                    $handle { conn: self.conn }
                }
            )+
        }
    };
}

tables! {
    Mailboxes => mailboxes, "mailboxes";
    Emails => emails, "emails";
    Identities => identities, "identities";
    SieveScripts => sieve_scripts, "sieve_scripts";
    AddressBooks => address_books, "address_books";
    ContactCards => contact_cards, "contact_cards";
    Calendars => calendars, "calendars";
    CalendarEvents => calendar_events, "calendar_events";
    ParticipantIdentities => participant_identities, "participant_identities";
    FileNodes => file_nodes, "file_nodes";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberList {
    EmailMailboxes,
    EventCalendars,
    CardAddressBooks,
}

impl MemberList {
    const fn statements(self) -> (&'static str, &'static str) {
        match self {
            MemberList::EmailMailboxes => (
                "SELECT id, mailbox_ids FROM emails",
                "UPDATE emails SET mailbox_ids = ?1 WHERE id = ?2",
            ),
            MemberList::EventCalendars => (
                "SELECT id, calendar_ids FROM calendar_events",
                "UPDATE calendar_events SET calendar_ids = ?1 WHERE id = ?2",
            ),
            MemberList::CardAddressBooks => (
                "SELECT id, address_book_ids FROM contact_cards",
                "UPDATE contact_cards SET address_book_ids = ?1 WHERE id = ?2",
            ),
        }
    }
}

impl Store<'_> {
    pub fn prune_members(
        self,
        list: MemberList,
        removed: &HashSet<LocalId>,
    ) -> rusqlite::Result<()> {
        let (select, update) = list.statements();
        let rows: Vec<(LocalId, String)> = self
            .conn
            .prepare(select)?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        let mut update = self.conn.prepare(update)?;
        let mut kept = Vec::new();
        for (id, json) in rows {
            let members: Vec<i64> = serde_json::from_str(&json).unwrap_or_default();
            if members.iter().any(|&m| removed.contains(&LocalId(m))) {
                kept.clear();
                kept.extend(
                    members
                        .into_iter()
                        .map(LocalId)
                        .filter(|m| !removed.contains(m)),
                );
                update.execute(params![LocalIdArray(&kept), id])?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Archive;

    #[test]
    fn local_id_array_matches_the_json_encoding() {
        let archive = Archive::open_in_memory().unwrap();
        let ids = [LocalId::from_raw(1), LocalId::from_raw(23)];
        let text: String = archive
            .connection()
            .query_row("SELECT ?1", [LocalIdArray(&ids)], |r| r.get(0))
            .unwrap();
        assert_eq!(text, "[1,23]");
        let empty: String = archive
            .connection()
            .query_row("SELECT ?1", [LocalIdArray(&[])], |r| r.get(0))
            .unwrap();
        assert_eq!(empty, "[]");
    }
}
