/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::archive::rows::EmailRecord;
use crate::console::Counted;
use crate::error::Error;
use crate::exchange::graph::client::Accept;
use crate::exchange::graph::error::GraphError;
use crate::sync::TypeCounts;
use crate::sync::dates::MissingDate;
use crate::sync::diff::IdDiff;
use crate::sync::emailmeta::EmailMeta;
use crate::sync::keywords::{Keywords, MessageFlags};
use crate::sync::pool::Pool;
use crate::types::ObjectType;

use super::coordinator::{CHUNK_SIZE, GraphCoordinator};
use super::folders::GraphFolder;

const FLAGGED: &str = "flagged";

#[derive(Clone, Copy)]
struct MessageStub<'v>(&'v Value);

impl MessageStub<'_> {
    fn is(self, key: &str) -> bool {
        self.0.get(key).and_then(Value::as_bool) == Some(true)
    }

    fn keywords(self) -> Keywords {
        let mut keywords = MessageFlags {
            read: self.is("isRead"),
            draft: self.is("isDraft"),
            flagged: self
                .0
                .get("flag")
                .and_then(|f| f.get("flagStatus"))
                .and_then(Value::as_str)
                == Some(FLAGGED),
        }
        .keywords();
        if let Some(categories) = self.0.get("categories").and_then(Value::as_array) {
            for category in categories.iter().filter_map(Value::as_str) {
                keywords.push_category(category);
            }
        }
        keywords
    }
}

impl GraphCoordinator<'_> {
    pub fn reconcile_messages(
        &self,
        folders: &[GraphFolder],
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        let local = self.known(ObjectType::Email)?;
        let mut server_total: HashSet<String> = HashSet::new();
        let mut any_failure = false;

        for folder in folders {
            let url = self
                .endpoints
                .folder_messages_ids(&folder.graph_id, self.top);
            let stubs = match self.client.collect_all_values(&url, &[]) {
                Ok(stubs) => stubs,
                Err(e) => {
                    self.logger.warn(format_args!(
                        "mailbox {} enumeration failed: {e}",
                        folder.graph_id
                    ));
                    counts.failed += 1;
                    any_failure = true;
                    continue;
                }
            };
            self.logger.progress(format_args!(
                "Mailbox {}: {} listed",
                folder.graph_id,
                Counted::new(stubs.len() as u64, "email", "emails")
            ));
            let mut wanted: Vec<(String, String)> = Vec::new();
            for stub in &stubs {
                let Some(id) = stub.get("id").and_then(Value::as_str) else {
                    continue;
                };
                if local.contains_key(id) {
                    counts.fetched += 1;
                    server_total.insert(id.to_owned());
                } else if server_total.insert(id.to_owned()) {
                    wanted.push((id.to_owned(), MessageStub(stub).keywords().to_json()));
                }
            }
            if !wanted.is_empty() {
                self.fetch_messages(folder, &wanted, counts)?;
            }
        }

        if any_failure {
            self.logger.warn(format_args!(
                "vanished emails not removed: one or more mailboxes failed to enumerate; \
                 a clean re-run will reconcile deletions"
            ));
            return Ok(());
        }
        let vanished = IdDiff::compute(server_total, &local).vanished;
        self.sweep(ObjectType::Email, &vanished, counts)
    }

    fn fetch_messages(
        &self,
        folder: &GraphFolder,
        wanted: &[(String, String)],
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        let client = self.client.clone();
        let endpoints = self.endpoints.clone();
        let pool: Pool<String, (String, Result<Vec<u8>, GraphError>)> =
            Pool::new(self.workers, move |id: String| {
                let result = client
                    .get_with_prefer(&endpoints.message_mime(&id), Accept::Text, &[])
                    .map(|response| response.body);
                (id, result)
            });
        for (id, _) in wanted {
            pool.submit(id.clone());
        }
        let keywords: HashMap<&str, &str> = wanted
            .iter()
            .map(|(id, keywords)| (id.as_str(), keywords.as_str()))
            .collect();
        let mailboxes = [folder.local_id];
        let mut batch = None;
        let mut in_batch = 0usize;
        for _ in 0..wanted.len() {
            let Ok((graph_id, result)) = pool.results().recv() else {
                break;
            };
            let bytes = match result {
                Ok(bytes) => bytes,
                Err(GraphError::Vanished) => {
                    counts.skipped += 1;
                    continue;
                }
                Err(e) => {
                    counts.failed += 1;
                    self.logger
                        .warn(format_args!("email {graph_id} fetch failed: {e}"));
                    continue;
                }
            };
            let tx = match batch.take() {
                Some(tx) => tx,
                None => self.store.begin()?,
            };
            let store = tx.store();
            let meta = EmailMeta::from_blob(&bytes);
            let received_at = meta.date.unwrap_or_else(|| MissingDate::Epoch.resolve());
            let message_match = meta.index.to_json();
            let id = store.emails().insert(&EmailRecord {
                blob: store.blobs().intern(&bytes)?,
                received_at: &received_at,
                mailboxes: &mailboxes,
                keywords: keywords.get(graph_id.as_str()).copied().unwrap_or("[]"),
                message_match: Some(&message_match),
            })?;
            self.ids_in(store)
                .insert(ObjectType::Email, &graph_id, id)?;
            counts.created += 1;
            in_batch += 1;
            if in_batch >= CHUNK_SIZE {
                tx.commit()?;
                in_batch = 0;
            } else {
                batch = Some(tx);
            }
        }
        if let Some(tx) = batch {
            tx.commit()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn stub_keywords_keep_the_flag_order_then_categories() {
        let stub = json!({
            "isRead": true,
            "isDraft": true,
            "flag": {"flagStatus": "flagged"},
            "categories": ["Review / Legal", "review / legal", "$seen", "  "]
        });
        assert_eq!(
            MessageStub(&stub).keywords().to_json(),
            r#"["$seen","$draft","$flagged","review_/_legal","seen"]"#
        );
        assert_eq!(
            MessageStub(&json!({"isRead": false})).keywords().to_json(),
            "[]"
        );
    }
}
