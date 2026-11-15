/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use super::folder_id_table;

folder_id_table!(
    MaildirIds,
    maildir_ids,
    "sync_id_maildir",
    "INSERT",
    String,
    unique_id: &str => String = "''"
);

#[cfg(test)]
mod tests {
    use crate::archive::sources::{SourceKey, SourceKind};
    use crate::archive::{Archive, LocalId};

    fn setup() -> Archive {
        Archive::open_in_memory().unwrap()
    }

    fn ids(archive: &Archive) -> super::MaildirIds<'_> {
        let sid = archive
            .store()
            .sources()
            .upsert(
                &SourceKey {
                    kind: SourceKind::Maildir,
                    url: "file:///home/alice/Maildir",
                    account_id: "/home/alice/Maildir",
                },
                Some("Maildir"),
                "",
            )
            .unwrap();
        archive.store().maildir_ids(sid)
    }

    fn id(raw: i64) -> LocalId {
        LocalId::from_raw(raw)
    }

    #[test]
    fn mailbox_roundtrip() {
        let archive = setup();
        let ids = ids(&archive);
        ids.insert_folder("INBOX", id(7)).unwrap();
        ids.insert_folder("Archive.2025", id(8)).unwrap();
        assert_eq!(ids.folder_id("INBOX").unwrap(), Some(id(7)));
        let all = ids.folders().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all.get("Archive.2025"), Some(&id(8)));
        ids.delete_folder("INBOX").unwrap();
        assert_eq!(ids.folder_id("INBOX").unwrap(), None);
    }

    #[test]
    fn email_roundtrip() {
        let archive = setup();
        let ids = ids(&archive);
        ids.insert_item("INBOX", "1739471123.M001.host", id(1))
            .unwrap();
        ids.insert_item("INBOX", "1739471200.M002.host", id(2))
            .unwrap();
        ids.insert_item("Sent", "1739471300.M003.host", id(3))
            .unwrap();
        assert_eq!(
            ids.item_id("INBOX", "1739471123.M001.host").unwrap(),
            Some(id(1))
        );
        let inbox = ids.items_in("INBOX").unwrap();
        assert_eq!(inbox.len(), 2);
        assert_eq!(inbox.get("1739471123.M001.host"), Some(&id(1)));
        let folders = ids.folders_with_items().unwrap();
        assert!(folders.contains("INBOX"));
        assert!(folders.contains("Sent"));
    }

    #[test]
    fn delete_email_removes_only_that_row() {
        let archive = setup();
        let ids = ids(&archive);
        ids.insert_item("INBOX", "u1", id(1)).unwrap();
        ids.insert_item("INBOX", "u2", id(2)).unwrap();
        ids.delete_item("INBOX", "u1").unwrap();
        let inbox = ids.items_in("INBOX").unwrap();
        assert_eq!(inbox.len(), 1);
        assert!(inbox.contains_key("u2"));
    }

    #[test]
    fn delete_all_emails_in_folder_wipes_only_that_folder() {
        let archive = setup();
        let ids = ids(&archive);
        ids.insert_item("INBOX", "u1", id(1)).unwrap();
        ids.insert_item("INBOX", "u2", id(2)).unwrap();
        ids.insert_item("Sent", "u3", id(3)).unwrap();
        let removed = ids.delete_items_in("INBOX").unwrap();
        assert_eq!(removed, 2);
        assert!(ids.items_in("INBOX").unwrap().is_empty());
        assert_eq!(ids.items_in("Sent").unwrap().len(), 1);
    }

    #[test]
    fn mailbox_and_email_namespaces_do_not_collide_on_local_id() {
        let archive = setup();
        let ids = ids(&archive);
        ids.insert_folder("INBOX", id(42)).unwrap();
        ids.insert_item("INBOX", "u1", id(42)).unwrap();
    }

    #[test]
    fn re_insert_with_same_unique_id_is_rejected_by_pk() {
        let archive = setup();
        let ids = ids(&archive);
        ids.insert_item("INBOX", "u1", id(1)).unwrap();
        let err = ids
            .insert_item("INBOX", "u1", id(2))
            .expect_err("PK conflict");
        assert!(matches!(err, rusqlite::Error::SqliteFailure(_, _)));
        assert_eq!(ids.item_id("INBOX", "u1").unwrap(), Some(id(1)));
        assert_eq!(ids.items_in("INBOX").unwrap().len(), 1);
    }

    #[test]
    fn re_insert_with_same_local_id_is_rejected_by_unique_constraint() {
        let archive = setup();
        let ids = ids(&archive);
        ids.insert_item("INBOX", "u1", id(42)).unwrap();
        let err = ids
            .insert_item("INBOX", "u2", id(42))
            .expect_err("UNIQUE conflict");
        assert!(matches!(err, rusqlite::Error::SqliteFailure(_, _)));
    }
}
