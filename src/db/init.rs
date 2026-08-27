/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use rusqlite::Connection;

pub const SCHEMA_SQL: &str = include_str!("schema.sql");

pub fn open(path: &std::path::Path) -> Result<Connection, OpenError> {
    let conn = Connection::open(path)?;
    apply_pragmas(&conn)?;
    apply_schema(&conn)?;
    Ok(conn)
}

pub fn apply_schema(conn: &Connection) -> Result<(), OpenError> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(SCHEMA_SQL)?;
    ensure_calendar_events_data_type(&tx)?;
    ensure_graph_ids_accept_file_nodes(&tx)?;
    tx.commit()?;
    Ok(())
}

fn ensure_calendar_events_data_type(conn: &Connection) -> Result<(), OpenError> {
    let mut stmt = conn.prepare("PRAGMA table_info(calendar_events)")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    let has_column = rows.filter_map(|r| r.ok()).any(|name| name == "data_type");
    if !has_column {
        conn.execute(
            "ALTER TABLE calendar_events ADD COLUMN data_type TEXT NOT NULL DEFAULT 'Event'",
            [],
        )?;
    }
    Ok(())
}

fn ensure_graph_ids_accept_file_nodes(conn: &Connection) -> Result<(), OpenError> {
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

fn apply_pragmas(conn: &Connection) -> Result<(), OpenError> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}
