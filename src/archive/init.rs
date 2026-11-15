/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::path::Path;

use rusqlite::Connection;

use crate::archive::{Archive, ArchiveError};

const SCHEMA_SQL: &str = include_str!("schema.sql");

impl Archive {
    pub(super) fn connect(path: &Path) -> Result<Connection, ArchiveError> {
        let conn = Connection::open(path)?;
        Archive::apply_pragmas(&conn)?;
        Archive::apply_schema(&conn)?;
        Ok(conn)
    }

    pub(super) fn apply_schema(conn: &Connection) -> Result<(), ArchiveError> {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(SCHEMA_SQL)?;
        Archive::ensure_calendar_events_data_type(&tx)?;
        Archive::ensure_file_nodes_executable(&tx)?;
        Archive::ensure_graph_ids_accept_file_nodes(&tx)?;
        tx.commit()?;
        Ok(())
    }

    fn ensure_calendar_events_data_type(conn: &Connection) -> Result<(), ArchiveError> {
        Archive::ensure_column(
            conn,
            "calendar_events",
            "data_type",
            "ALTER TABLE calendar_events ADD COLUMN data_type TEXT NOT NULL DEFAULT 'Event'",
        )
    }

    fn ensure_file_nodes_executable(conn: &Connection) -> Result<(), ArchiveError> {
        Archive::ensure_column(
            conn,
            "file_nodes",
            "executable",
            "ALTER TABLE file_nodes ADD COLUMN executable INTEGER NOT NULL DEFAULT 0",
        )
    }

    fn ensure_column(
        conn: &Connection,
        table: &str,
        column: &str,
        alter: &str,
    ) -> Result<(), ArchiveError> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
        let has_column = rows.filter_map(|r| r.ok()).any(|name| name == column);
        if !has_column {
            conn.execute(alter, [])?;
        }
        Ok(())
    }

    fn ensure_graph_ids_accept_file_nodes(conn: &Connection) -> Result<(), ArchiveError> {
        let sql: Option<String> = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' \
                 AND name = 'sync_id_exchange_graph'",
                [],
                |row| row.get(0),
            )
            .ok();
        let Some(sql) = sql else { return Ok(()) };
        if sql.contains("'filenode'") {
            return Ok(());
        }
        conn.execute_batch(
            "ALTER TABLE sync_id_exchange_graph RENAME TO sync_id_exchange_graph_old;
             CREATE TABLE sync_id_exchange_graph (
                 source_id   INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
                 type_name   TEXT    NOT NULL CHECK (type_name IN (
                                                         'mailbox','email',
                                                         'calendar','calendarevent',
                                                         'addressbook','contactcard',
                                                         'filenode')),
                 graph_id    TEXT    NOT NULL,
                 local_id    INTEGER NOT NULL,
                 PRIMARY KEY (source_id, type_name, graph_id),
                 UNIQUE (source_id, type_name, local_id)
             );
             INSERT INTO sync_id_exchange_graph SELECT * FROM sync_id_exchange_graph_old;
             DROP TABLE sync_id_exchange_graph_old;
             CREATE INDEX IF NOT EXISTS sync_id_exchange_graph_type_idx
                 ON sync_id_exchange_graph (source_id, type_name);",
        )?;
        Ok(())
    }

    fn apply_pragmas(conn: &Connection) -> Result<(), ArchiveError> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
        Ok(())
    }
}
