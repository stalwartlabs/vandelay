/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;
use std::io;
use std::path::Path;

use crate::archive::ids::takeout::TakeoutIds;
use crate::archive::rows::{EmailRecord, MailboxRecord, Subscribed, Write};
use crate::archive::{LocalId, Store};
use crate::console::Counted;
use crate::error::Error;
use crate::imap::automap::Automap;
use crate::logging::Logger;
use crate::sync::TypeCounts;
use crate::sync::emailmeta::EmailMeta;
use crate::types::ObjectType;

use super::labels::{GmailLabels, LabelClassification};
use super::mbox::Message;

const DEFAULT_MAILBOX: &str = "Imported";
const SHORT_HASH: usize = 12;
const PROGRESS_TICK: u64 = 256;

pub struct MailWriter<'a> {
    pub store: Store<'a>,
    pub ids: TakeoutIds<'a>,
    pub automap: Automap,
    pub fallback: &'a str,
    pub mailboxes: &'a mut HashMap<String, LocalId>,
    pub mailbox_counts: &'a mut TypeCounts,
    pub email_counts: &'a mut TypeCounts,
    pub logger: Logger,
}

impl MailWriter<'_> {
    pub fn fallback_mailbox(path: &Path) -> &str {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(DEFAULT_MAILBOX)
    }

    pub fn write_all(&mut self, path: &Path, messages: impl Iterator<Item = io::Result<Message>>) {
        let logger = self.logger;
        let mut processed: u64 = 0;
        for message in messages {
            let message = match message {
                Ok(message) => message,
                Err(e) => {
                    logger.warn(format_args!("cannot read {}: {e}", path.display()));
                    self.email_counts.failed += 1;
                    break;
                }
            };
            match self.write_message(&message) {
                Ok(()) => {
                    processed += 1;
                    if processed.is_multiple_of(PROGRESS_TICK) {
                        logger.progress(format_args!(
                            "{}: {} processed",
                            path.display(),
                            Counted::new(processed, "message", "messages")
                        ));
                    }
                }
                Err(e) => {
                    logger.warn(format_args!(
                        "cannot import a message from {}: {e}",
                        path.display()
                    ));
                    self.email_counts.failed += 1;
                }
            }
        }
    }

    fn write_message(&mut self, message: &Message) -> Result<(), Error> {
        let bytes = message.contents();
        let hash = blake3::hash(bytes).to_hex();
        let msg_hash = hash.as_str();
        let short = msg_hash.get(..SHORT_HASH).unwrap_or(msg_hash);

        let header = GmailLabels::header(bytes);
        let tokens = header
            .as_deref()
            .map(GmailLabels::parse)
            .unwrap_or_default();
        let mut classification = LabelClassification::classify(&tokens);
        if classification.mailboxes.is_empty() {
            let fallback = match self.fallback.trim() {
                "" => DEFAULT_MAILBOX,
                fallback => fallback,
            };
            if header.is_some() {
                self.logger.warn(format_args!(
                    "message {short} has an empty X-Gmail-Labels header; filed under \"{fallback}\""
                ));
            } else {
                self.logger.warn(format_args!(
                    "message {short} has no X-Gmail-Labels header; filed under \"{fallback}\""
                ));
            }
            classification.mailboxes.push(fallback.to_owned());
        }
        if classification.opened_won_over_unread {
            self.logger.warn(format_args!(
                "message {short} is labelled both Opened and Unread; marked as seen"
            ));
        }

        let mut mailbox_ids: Vec<LocalId> = Vec::with_capacity(classification.mailboxes.len());
        for path in &classification.mailboxes {
            mailbox_ids.push(self.ensure_mailbox(path)?);
        }
        mailbox_ids.sort_unstable();
        mailbox_ids.dedup();
        let keywords = classification.keywords.to_json();

        let emails = self.store.emails();
        if let Some(local) = self.ids.local_id(ObjectType::Email, msg_hash)? {
            match emails.update_placement(local, &mailbox_ids, &keywords)? {
                Write::Changed => self.email_counts.fetched += 1,
                Write::Unchanged => self.email_counts.skipped += 1,
            }
            return Ok(());
        }

        let blob = self.store.blobs().intern(bytes)?;
        let EmailMeta { index, date } = EmailMeta::from_blob(bytes);
        let message_match = index.to_json();
        let received_at = message.received_at(date);
        let local = emails.insert(&EmailRecord {
            blob,
            received_at: &received_at,
            mailboxes: &mailbox_ids,
            keywords: &keywords,
            message_match: Some(&message_match),
        })?;
        self.ids.insert(ObjectType::Email, msg_hash, local)?;
        self.email_counts.created += 1;
        self.email_counts.fetched += 1;
        Ok(())
    }

    fn ensure_mailbox(&mut self, path: &str) -> Result<LocalId, Error> {
        if let Some(&id) = self.mailboxes.get(path) {
            return Ok(id);
        }
        if let Some(id) = self.ids.local_id(ObjectType::Mailbox, path)? {
            self.mailboxes.insert(path.to_owned(), id);
            return Ok(id);
        }
        let (parent, leaf) = match path.rsplit_once('/') {
            Some((parent, leaf)) => (Some(self.ensure_mailbox(parent)?), leaf),
            None => (None, path),
        };
        let id = self.store.mailboxes().insert(&MailboxRecord {
            name: leaf,
            parent,
            role: GmailLabels::role(path, self.automap),
            sort_order: 0,
            subscribed: Subscribed::Yes,
        })?;
        self.ids.insert(ObjectType::Mailbox, path, id)?;
        self.mailboxes.insert(path.to_owned(), id);
        self.mailbox_counts.created += 1;
        self.mailbox_counts.fetched += 1;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Archive;
    use crate::archive::sources::{SourceKey, SourceKind};
    use crate::logging::Level;

    struct Planned {
        rows: Vec<(String, Option<String>, Option<String>)>,
        ids: Vec<LocalId>,
        created: u64,
        mapped: u64,
    }

    fn plan(paths: &[&str], automap: Automap) -> Planned {
        let archive = Archive::open_in_memory().unwrap();
        let store = archive.store();
        let source = store
            .sources()
            .upsert(
                &SourceKey {
                    kind: SourceKind::Takeout,
                    url: "file:///tmp/Takeout",
                    account_id: "/tmp/Takeout",
                },
                Some("Takeout"),
                "",
            )
            .unwrap();
        let mut mailboxes = HashMap::new();
        let mut mailbox_counts = TypeCounts::default();
        let mut email_counts = TypeCounts::default();
        let mut writer = MailWriter {
            store,
            ids: store.takeout_ids(source),
            automap,
            fallback: "",
            mailboxes: &mut mailboxes,
            mailbox_counts: &mut mailbox_counts,
            email_counts: &mut email_counts,
            logger: Logger::new(Level::Quiet),
        };
        let ids = paths
            .iter()
            .map(|path| writer.ensure_mailbox(path).unwrap())
            .collect();
        let mapped = store
            .takeout_ids(source)
            .count(ObjectType::Mailbox)
            .unwrap();
        let rows = archive
            .connection()
            .prepare(
                "SELECT m.name, p.name, m.role FROM mailboxes m
                 LEFT JOIN mailboxes p ON p.id = m.parent_id ORDER BY m.name",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        Planned {
            rows,
            ids,
            created: mailbox_counts.created,
            mapped,
        }
    }

    fn row(
        name: &str,
        parent: Option<&str>,
        role: Option<&str>,
    ) -> (String, Option<String>, Option<String>) {
        (
            name.to_owned(),
            parent.map(str::to_owned),
            role.map(str::to_owned),
        )
    }

    #[test]
    fn flat_paths_create_top_level_mailboxes_only() {
        let p = plan(&["Inbox", "Sent", "Github"], Automap::Disabled);
        assert_eq!(
            p.rows,
            [
                row("Github", None, None),
                row("Inbox", None, None),
                row("Sent", None, None)
            ]
        );
        assert_eq!((p.created, p.mapped), (3, 3));
    }

    #[test]
    fn nested_path_creates_its_missing_parent() {
        let p = plan(&["Label_001/Label_002_under_Label_001"], Automap::Disabled);
        assert_eq!(
            p.rows,
            [
                row("Label_001", None, None),
                row("Label_002_under_Label_001", Some("Label_001"), None)
            ]
        );
        assert_eq!((p.created, p.mapped), (2, 2));
    }

    #[test]
    fn explicit_parent_is_reused_by_its_children() {
        let p = plan(
            &["Label_001", "Label_001/Sub", "Label_001/Other"],
            Automap::Disabled,
        );
        assert_eq!(
            p.rows,
            [
                row("Label_001", None, None),
                row("Other", Some("Label_001"), None),
                row("Sub", Some("Label_001"), None)
            ]
        );
        assert_eq!((p.created, p.mapped), (3, 3));
    }

    #[test]
    fn three_level_path_creates_every_missing_ancestor() {
        let p = plan(&["A/B/C"], Automap::Disabled);
        assert_eq!(
            p.rows,
            [
                row("A", None, None),
                row("B", Some("A"), None),
                row("C", Some("B"), None)
            ]
        );
        assert_eq!(p.created, 3);
    }

    #[test]
    fn role_applies_to_an_auto_created_parent() {
        let p = plan(&["Inbox/Sub"], Automap::Enabled);
        assert_eq!(
            p.rows,
            [
                row("Inbox", None, Some("inbox")),
                row("Sub", Some("Inbox"), None)
            ]
        );
    }

    #[test]
    fn role_applies_to_an_explicit_top_level_mailbox() {
        let p = plan(&["Inbox"], Automap::Enabled);
        assert_eq!(p.rows, [row("Inbox", None, Some("inbox"))]);
    }

    #[test]
    fn duplicate_paths_share_one_mailbox() {
        let p = plan(&["Inbox", "Inbox", "Inbox"], Automap::Disabled);
        assert_eq!(p.rows.len(), 1);
        assert!(p.ids.windows(2).all(|w| w[0] == w[1]));
        assert_eq!((p.created, p.mapped), (1, 1));
    }

    #[test]
    fn no_paths_create_no_mailboxes() {
        let p = plan(&[], Automap::Enabled);
        assert!(p.rows.is_empty());
        assert_eq!((p.created, p.mapped), (0, 0));
    }

    #[test]
    fn fallback_mailbox_strips_extension() {
        let p = Path::new("/some/dir/Github.mbox");
        assert_eq!(MailWriter::fallback_mailbox(p), "Github");
    }

    #[test]
    fn fallback_mailbox_handles_dotted_basename() {
        let p = Path::new("/x/All mail Including Spam and Trash.mbox");
        assert_eq!(
            MailWriter::fallback_mailbox(p),
            "All mail Including Spam and Trash"
        );
    }
}
