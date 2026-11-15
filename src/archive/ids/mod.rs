/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

pub mod dav;
pub mod ews;
pub mod graph;
pub mod imap;
pub mod jmap;
pub mod maildir;
pub mod sieve;
pub mod takeout;

use crate::types::ObjectType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeColumn {
    JmapName,
    Token,
}

impl TypeColumn {
    pub fn encode(self, ty: ObjectType) -> &'static str {
        match self {
            TypeColumn::JmapName => ty.jmap_name(),
            TypeColumn::Token => ty.token(),
        }
    }
}

macro_rules! flat_id_table {
    ($name:ident, $accessor:ident, $table:literal, $ext:literal, $type_column:expr, $insert:literal) => {
        #[derive(Clone, Copy)]
        pub struct $name<'a> {
            conn: &'a rusqlite::Connection,
            source: $crate::archive::SourceId,
        }

        impl<'a> $crate::archive::Store<'a> {
            pub fn $accessor(self, source: $crate::archive::SourceId) -> $name<'a> {
                $name {
                    conn: self.connection(),
                    source,
                }
            }
        }

        impl $name<'_> {
            const TYPE_COLUMN: $crate::archive::ids::TypeColumn = $type_column;

            pub fn insert(
                &self,
                ty: $crate::types::ObjectType,
                ext: &str,
                local: $crate::archive::LocalId,
            ) -> rusqlite::Result<()> {
                self.conn.execute(
                    concat!(
                        $insert,
                        " INTO ",
                        $table,
                        " (source_id, type_name, ",
                        $ext,
                        ", local_id) VALUES (?1, ?2, ?3, ?4)"
                    ),
                    rusqlite::params![self.source, Self::TYPE_COLUMN.encode(ty), ext, local],
                )?;
                Ok(())
            }

            pub fn local_id(
                &self,
                ty: $crate::types::ObjectType,
                ext: &str,
            ) -> rusqlite::Result<Option<$crate::archive::LocalId>> {
                rusqlite::OptionalExtension::optional(self.conn.query_row(
                    concat!(
                        "SELECT local_id FROM ",
                        $table,
                        " WHERE source_id = ?1 AND type_name = ?2 AND ",
                        $ext,
                        " = ?3"
                    ),
                    rusqlite::params![self.source, Self::TYPE_COLUMN.encode(ty), ext],
                    |row| row.get(0),
                ))
            }

            pub fn all(
                &self,
                ty: $crate::types::ObjectType,
            ) -> rusqlite::Result<std::collections::HashMap<String, $crate::archive::LocalId>> {
                let mut stmt = self.conn.prepare_cached(concat!(
                    "SELECT ",
                    $ext,
                    ", local_id FROM ",
                    $table,
                    " WHERE source_id = ?1 AND type_name = ?2"
                ))?;
                let rows = stmt.query_map(
                    rusqlite::params![self.source, Self::TYPE_COLUMN.encode(ty)],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                rows.collect()
            }

            pub fn delete(&self, ty: $crate::types::ObjectType, ext: &str) -> rusqlite::Result<()> {
                self.conn.execute(
                    concat!(
                        "DELETE FROM ",
                        $table,
                        " WHERE source_id = ?1 AND type_name = ?2 AND ",
                        $ext,
                        " = ?3"
                    ),
                    rusqlite::params![self.source, Self::TYPE_COLUMN.encode(ty), ext],
                )?;
                Ok(())
            }
        }
    };
}

macro_rules! folder_id_table {
    (
        $name:ident,
        $accessor:ident,
        $table:literal,
        $insert:literal,
        $key:ty,
        $($col:ident: $param:ty => $owned:ty = $placeholder:literal),+
    ) => {
        #[derive(Clone, Copy)]
        pub struct $name<'a> {
            conn: &'a rusqlite::Connection,
            source: $crate::archive::SourceId,
        }

        impl<'a> $crate::archive::Store<'a> {
            pub fn $accessor(self, source: $crate::archive::SourceId) -> $name<'a> {
                $name {
                    conn: self.connection(),
                    source,
                }
            }
        }

        impl $name<'_> {
            const FOLDER: &'static str = $crate::types::ObjectType::Mailbox.token();
            const ITEM: &'static str = $crate::types::ObjectType::Email.token();

            pub fn insert_folder(
                &self,
                folder: &str,
                local: $crate::archive::LocalId,
            ) -> rusqlite::Result<()> {
                self.conn.execute(
                    concat!(
                        $insert, " INTO ", $table,
                        " (source_id, type_name, folder, ", $(stringify!($col), ", ",)+
                        "local_id) VALUES (?1, ?2, ?3, ", $($placeholder, ", ",)+ "?4)"
                    ),
                    rusqlite::params![self.source, Self::FOLDER, folder, local],
                )?;
                Ok(())
            }

            pub fn insert_item(
                &self,
                folder: &str,
                $($col: $param,)+
                local: $crate::archive::LocalId,
            ) -> rusqlite::Result<()> {
                self.conn.prepare_cached(
                    concat!(
                        $insert, " INTO ", $table,
                        " (source_id, type_name, folder, ", $(stringify!($col), ", ",)+
                        "local_id) VALUES (:source, :type, :folder, ",
                        $(":", stringify!($col), ", ",)+ ":local)"
                    ),
                )?.execute(&[
                        (":source", &self.source as &dyn rusqlite::ToSql),
                        (":type", &Self::ITEM),
                        (":folder", &folder),
                        $((concat!(":", stringify!($col)), &$col),)+
                        (":local", &local),
                    ][..])?;
                Ok(())
            }

            pub fn folder_id(
                &self,
                folder: &str,
            ) -> rusqlite::Result<Option<$crate::archive::LocalId>> {
                rusqlite::OptionalExtension::optional(self.conn.query_row(
                    concat!(
                        "SELECT local_id FROM ", $table,
                        " WHERE source_id = ?1 AND type_name = ?2 AND folder = ?3",
                        $(" AND ", stringify!($col), " = ", $placeholder,)+
                    ),
                    rusqlite::params![self.source, Self::FOLDER, folder],
                    |row| row.get(0),
                ))
            }

            pub fn item_id(
                &self,
                folder: &str,
                $($col: $param,)+
            ) -> rusqlite::Result<Option<$crate::archive::LocalId>> {
                rusqlite::OptionalExtension::optional(self.conn.prepare_cached(
                    concat!(
                        "SELECT local_id FROM ", $table,
                        " WHERE source_id = :source AND type_name = :type AND folder = :folder",
                        $(" AND ", stringify!($col), " = :", stringify!($col),)+
                    ),
                )?.query_row(&[
                        (":source", &self.source as &dyn rusqlite::ToSql),
                        (":type", &Self::ITEM),
                        (":folder", &folder),
                        $((concat!(":", stringify!($col)), &$col),)+
                    ][..],
                    |row| row.get(0),
                ))
            }

            pub fn folders(
                &self,
            ) -> rusqlite::Result<std::collections::HashMap<String, $crate::archive::LocalId>> {
                let mut stmt = self.conn.prepare_cached(concat!(
                    "SELECT folder, local_id FROM ", $table,
                    " WHERE source_id = ?1 AND type_name = ?2",
                    $(" AND ", stringify!($col), " = ", $placeholder,)+
                ))?;
                let rows = stmt.query_map(rusqlite::params![self.source, Self::FOLDER], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?;
                rows.collect()
            }

            pub fn items_in(
                &self,
                folder: &str,
            ) -> rusqlite::Result<std::collections::HashMap<$key, $crate::archive::LocalId>> {
                let mut stmt = self.conn.prepare_cached(concat!(
                    "SELECT ", $(stringify!($col), ", ",)+ "local_id FROM ", $table,
                    " WHERE source_id = ?1 AND type_name = ?2 AND folder = ?3"
                ))?;
                let rows = stmt.query_map(
                    rusqlite::params![self.source, Self::ITEM, folder],
                    |row| {
                        Ok((
                            ($(row.get::<_, $owned>(stringify!($col))?),+),
                            row.get("local_id")?,
                        ))
                    },
                )?;
                rows.collect()
            }

            pub fn delete_folder(&self, folder: &str) -> rusqlite::Result<()> {
                self.conn.execute(
                    concat!(
                        "DELETE FROM ", $table,
                        " WHERE source_id = ?1 AND type_name = ?2 AND folder = ?3",
                        $(" AND ", stringify!($col), " = ", $placeholder,)+
                    ),
                    rusqlite::params![self.source, Self::FOLDER, folder],
                )?;
                Ok(())
            }

            pub fn delete_item(&self, folder: &str, $($col: $param,)+) -> rusqlite::Result<()> {
                self.conn.prepare_cached(
                    concat!(
                        "DELETE FROM ", $table,
                        " WHERE source_id = :source AND type_name = :type AND folder = :folder",
                        $(" AND ", stringify!($col), " = :", stringify!($col),)+
                    ),
                )?.execute(&[
                    (":source", &self.source as &dyn rusqlite::ToSql),
                    (":type", &Self::ITEM),
                    (":folder", &folder),
                    $((concat!(":", stringify!($col)), &$col),)+
                ][..])?;
                Ok(())
            }

            pub fn delete_items_in(&self, folder: &str) -> rusqlite::Result<usize> {
                self.conn.execute(
                    concat!(
                        "DELETE FROM ", $table,
                        " WHERE source_id = ?1 AND type_name = ?2 AND folder = ?3"
                    ),
                    rusqlite::params![self.source, Self::ITEM, folder],
                )
            }

            pub fn folders_with_items(
                &self,
            ) -> rusqlite::Result<std::collections::HashSet<String>> {
                let mut stmt = self.conn.prepare_cached(concat!(
                    "SELECT DISTINCT folder FROM ", $table,
                    " WHERE source_id = ?1 AND type_name = ?2"
                ))?;
                let rows =
                    stmt.query_map(rusqlite::params![self.source, Self::ITEM], |row| row.get(0))?;
                rows.collect()
            }
        }
    };
}

pub(crate) use flat_id_table;
pub(crate) use folder_id_table;

#[cfg(test)]
mod tests {
    use crate::archive::sources::{SourceKey, SourceKind};
    use crate::archive::{Archive, LocalId, SourceId};

    fn source(archive: &Archive, kind: SourceKind) -> SourceId {
        archive
            .store()
            .sources()
            .upsert(
                &SourceKey {
                    kind,
                    url: "u",
                    account_id: "a",
                },
                None,
                "a",
            )
            .unwrap()
    }

    #[test]
    fn folder_tables_keep_folders_and_items_apart() {
        let archive = Archive::open_in_memory().unwrap();
        let ids = archive.store().imap_ids(source(&archive, SourceKind::Imap));
        ids.insert_folder("INBOX", LocalId::from_raw(1)).unwrap();
        ids.insert_item("INBOX", 9, 4, LocalId::from_raw(10))
            .unwrap();
        ids.insert_item("INBOX", 9, 2, LocalId::from_raw(11))
            .unwrap();
        ids.insert_item("INBOX", 9, 4, LocalId::from_raw(12))
            .unwrap();
        assert_eq!(ids.folder_id("INBOX").unwrap(), Some(LocalId::from_raw(1)));
        assert_eq!(
            ids.item_id("INBOX", 9, 4).unwrap(),
            Some(LocalId::from_raw(12))
        );
        assert_eq!(ids.folders().unwrap().len(), 1);
        assert_eq!(ids.items_in("INBOX").unwrap().len(), 2);
        assert_eq!(ids.uids_at_validity("INBOX", 9).unwrap(), [2, 4]);
        assert_eq!(ids.folders_with_items().unwrap().len(), 1);
        ids.delete_item("INBOX", 9, 2).unwrap();
        assert_eq!(ids.delete_items_in("INBOX").unwrap(), 1);
        ids.delete_folder("INBOX").unwrap();
        assert!(ids.folders().unwrap().is_empty());
    }

    #[test]
    fn maildir_items_fail_on_conflict() {
        let archive = Archive::open_in_memory().unwrap();
        let ids = archive
            .store()
            .maildir_ids(source(&archive, SourceKind::Maildir));
        ids.insert_folder("", LocalId::from_raw(1)).unwrap();
        ids.insert_item("", "uniq", LocalId::from_raw(2)).unwrap();
        assert!(ids.insert_item("", "uniq", LocalId::from_raw(3)).is_err());
        assert_eq!(
            ids.items_in("").unwrap().get("uniq"),
            Some(&LocalId::from_raw(2))
        );
        assert_eq!(ids.folder_id("").unwrap(), Some(LocalId::from_raw(1)));
    }
}
