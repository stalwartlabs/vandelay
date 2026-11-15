/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};

use crate::archive::rows::SieveScriptRecord;
use crate::archive::{LocalId, SourceId, Store};
use crate::error::Error;
use crate::sync::PlanCounts;

pub(super) struct ServerScript {
    pub name: String,
    pub active: bool,
    pub hash: [u8; 32],
    pub bytes: Vec<u8>,
}

impl ServerScript {
    pub fn new(name: String, active: bool, bytes: Vec<u8>) -> ServerScript {
        ServerScript {
            name,
            active,
            hash: *blake3::hash(&bytes).as_bytes(),
            bytes,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PresentOutcome {
    Unchanged,
    ActiveOnly,
    ContentUpdated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StoredScript {
    pub hash: Option<[u8; 32]>,
    pub active: bool,
}

#[derive(Default)]
pub(super) struct LocalScripts {
    ids: HashMap<String, LocalId>,
    stored: HashMap<LocalId, StoredScript>,
}

impl LocalScripts {
    pub fn load(store: Store<'_>, source: SourceId) -> Result<LocalScripts, Error> {
        let ids = store.sieve_ids(source).all()?;
        let stored = store
            .sieve_scripts()
            .managesieve_states(source)?
            .into_iter()
            .map(|state| {
                (
                    state.id,
                    StoredScript {
                        hash: state.hash,
                        active: state.is_active,
                    },
                )
            })
            .collect();
        Ok(LocalScripts { ids, stored })
    }

    fn stored(&self, local: LocalId) -> StoredScript {
        self.stored.get(&local).copied().unwrap_or(StoredScript {
            hash: None,
            active: false,
        })
    }
}

#[derive(Clone, Copy)]
pub(super) enum Action<'p> {
    New(&'p ServerScript),
    Present {
        script: &'p ServerScript,
        local: LocalId,
        stored: StoredScript,
        outcome: PresentOutcome,
    },
    Vanished {
        name: &'p str,
        local: LocalId,
    },
}

pub(super) struct Plan<'p> {
    pub actions: Vec<Action<'p>>,
}

impl<'p> Plan<'p> {
    pub fn compute(server: &'p [ServerScript], local: &'p LocalScripts) -> Plan<'p> {
        let mut actions = Vec::with_capacity(server.len());
        let mut server_names: HashSet<&str> = HashSet::with_capacity(server.len());
        for script in server {
            server_names.insert(&script.name);
            let action = match local.ids.get(&script.name) {
                None => Action::New(script),
                Some(&id) => {
                    let stored = local.stored(id);
                    let outcome = if stored.hash != Some(script.hash) {
                        PresentOutcome::ContentUpdated
                    } else if stored.active != script.active {
                        PresentOutcome::ActiveOnly
                    } else {
                        PresentOutcome::Unchanged
                    };
                    Action::Present {
                        script,
                        local: id,
                        stored,
                        outcome,
                    }
                }
            };
            actions.push(action);
        }
        actions.extend(
            local
                .ids
                .iter()
                .filter(|(name, _)| !server_names.contains(name.as_str()))
                .map(|(name, &id)| Action::Vanished { name, local: id }),
        );
        Plan { actions }
    }

    pub fn counts(&self) -> PlanCounts {
        let mut plan = PlanCounts::default();
        for action in &self.actions {
            match action {
                Action::New(_) => plan.new += 1,
                Action::Present { .. } => plan.present += 1,
                Action::Vanished { .. } => plan.vanished += 1,
            }
        }
        plan
    }
}

#[derive(Clone, Copy)]
pub(super) struct ScriptWriter<'a> {
    pub store: Store<'a>,
    pub source: SourceId,
}

impl ScriptWriter<'_> {
    pub fn insert(&self, script: &ServerScript) -> Result<LocalId, Error> {
        let blob = self.store.blobs().intern(&script.bytes)?;
        let local = self.store.sieve_scripts().insert(&SieveScriptRecord {
            name: Some(&script.name),
            is_active: false,
            blob,
        })?;
        self.store
            .sieve_ids(self.source)
            .insert(&script.name, local)?;
        Ok(local)
    }

    pub fn update_content(
        &self,
        local: LocalId,
        stored: StoredScript,
        script: &ServerScript,
    ) -> Result<(), Error> {
        let blob = self.store.blobs().intern(&script.bytes)?;
        self.store.sieve_scripts().update(
            local,
            &SieveScriptRecord {
                name: Some(&script.name),
                is_active: stored.active,
                blob,
            },
        )?;
        Ok(())
    }

    pub fn delete(&self, name: &str, local: LocalId) -> Result<(), Error> {
        self.store.sieve_scripts().delete(local)?;
        self.store.sieve_ids(self.source).delete(name)?;
        Ok(())
    }

    pub fn assign_active(&self, active: Option<LocalId>) -> Result<(), Error> {
        self.store.sieve_scripts().set_active(active)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::OptionalExtension;

    use super::*;
    use crate::archive::Archive;
    use crate::archive::sources::{SourceKey, SourceKind};

    fn archive() -> (Archive, SourceId) {
        let archive = Archive::open_in_memory().unwrap();
        let source = archive
            .store()
            .sources()
            .upsert(
                &SourceKey {
                    kind: SourceKind::ManageSieve,
                    url: "sieve://h:4190",
                    account_id: "a",
                },
                None,
                "a",
            )
            .unwrap();
        (archive, source)
    }

    fn script(name: &str, active: bool, bytes: &[u8]) -> ServerScript {
        ServerScript::new(name.to_owned(), active, bytes.to_vec())
    }

    fn local(entries: &[(&str, i64, &[u8], bool)]) -> LocalScripts {
        let mut local = LocalScripts::default();
        for &(name, id, bytes, active) in entries {
            let id = LocalId::from_raw(id);
            local.ids.insert(name.to_owned(), id);
            local.stored.insert(
                id,
                StoredScript {
                    hash: Some(*blake3::hash(bytes).as_bytes()),
                    active,
                },
            );
        }
        local
    }

    fn outcomes(plan: &Plan<'_>) -> Vec<Option<PresentOutcome>> {
        plan.actions
            .iter()
            .map(|action| match action {
                Action::Present { outcome, .. } => Some(*outcome),
                Action::New(_) | Action::Vanished { .. } => None,
            })
            .collect()
    }

    fn active_id(archive: &Archive) -> Option<LocalId> {
        archive
            .connection()
            .query_row(
                "SELECT id FROM sieve_scripts WHERE is_active = 1",
                [],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
    }

    fn count(archive: &Archive, table: &str) -> i64 {
        archive
            .connection()
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn plan_classifies_new_present_unchanged_and_vanished() {
        let server = [
            script("a", false, b"v"),
            script("b", true, b"w"),
            script("c", false, b"v"),
        ];
        let local = local(&[
            ("b", 2, b"w", true),
            ("c", 3, b"v", false),
            ("d", 4, b"v", false),
        ]);
        let plan = Plan::compute(&server, &local);
        assert_eq!(
            plan.counts(),
            PlanCounts {
                new: 1,
                vanished: 1,
                present: 2,
            }
        );
        assert!(matches!(
            plan.actions[3],
            Action::Vanished { name: "d", .. }
        ));
    }

    #[test]
    fn plan_flags_content_update_when_hash_differs() {
        let server = [script("a", false, b"w")];
        let local = local(&[("a", 1, b"v", false)]);
        let plan = Plan::compute(&server, &local);
        assert_eq!(outcomes(&plan), [Some(PresentOutcome::ContentUpdated)]);
    }

    #[test]
    fn plan_flags_active_only_when_only_flag_differs() {
        let server = [script("a", true, b"same")];
        let local = local(&[("a", 1, b"same", false)]);
        let plan = Plan::compute(&server, &local);
        assert_eq!(outcomes(&plan), [Some(PresentOutcome::ActiveOnly)]);
    }

    #[test]
    fn insert_writes_script_and_id_mapping() {
        let (archive, source) = archive();
        let writer = ScriptWriter {
            store: archive.store(),
            source,
        };
        let id = writer
            .insert(&script("vacation", true, b"require[\"x\"];"))
            .unwrap();
        assert_eq!(
            archive
                .store()
                .sieve_ids(source)
                .local_id("vacation")
                .unwrap(),
            Some(id)
        );
        assert_eq!(count(&archive, "sieve_scripts"), 1);
        assert_eq!(active_id(&archive), None);
    }

    #[test]
    fn content_update_keeps_the_active_flag_and_orphans_the_old_blob() {
        let (archive, source) = archive();
        let writer = ScriptWriter {
            store: archive.store(),
            source,
        };
        let id = writer.insert(&script("a", true, b"v1")).unwrap();
        writer.assign_active(Some(id)).unwrap();
        let stored = LocalScripts::load(archive.store(), source)
            .unwrap()
            .stored(id);
        writer
            .update_content(id, stored, &script("a", true, b"v2"))
            .unwrap();
        let data: Vec<u8> = archive
            .connection()
            .query_row(
                "SELECT b.data FROM sieve_scripts s JOIN blobs b ON b.id = s.blob_id WHERE s.id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(data, b"v2");
        assert_eq!(active_id(&archive), Some(id));
        assert_eq!(count(&archive, "blobs"), 2);
        archive.store().blobs().gc_orphans().unwrap();
        assert_eq!(count(&archive, "blobs"), 1);
    }

    #[test]
    fn assign_active_swaps_and_clears() {
        let (archive, source) = archive();
        let writer = ScriptWriter {
            store: archive.store(),
            source,
        };
        let a = writer.insert(&script("a", false, b"x")).unwrap();
        let b = writer.insert(&script("b", false, b"y")).unwrap();
        for target in [Some(a), Some(b), None] {
            writer.assign_active(target).unwrap();
            assert_eq!(active_id(&archive), target);
        }
    }

    #[test]
    fn delete_removes_an_active_script_and_its_mapping() {
        let (archive, source) = archive();
        let writer = ScriptWriter {
            store: archive.store(),
            source,
        };
        let id = writer.insert(&script("a", false, b"x")).unwrap();
        writer.assign_active(Some(id)).unwrap();
        writer.delete("a", id).unwrap();
        assert_eq!(count(&archive, "sieve_scripts"), 0);
        assert!(
            archive
                .store()
                .sieve_ids(source)
                .local_id("a")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn identical_script_bytes_share_one_blob() {
        let (archive, source) = archive();
        let writer = ScriptWriter {
            store: archive.store(),
            source,
        };
        let bytes = b"require [\"fileinto\"];\n";
        writer.insert(&script("a", false, bytes)).unwrap();
        writer.insert(&script("b", false, bytes)).unwrap();
        assert_eq!(count(&archive, "blobs"), 1);
    }

    #[test]
    fn partial_unique_index_rejects_two_active_rows() {
        let (archive, source) = archive();
        let writer = ScriptWriter {
            store: archive.store(),
            source,
        };
        let a = writer.insert(&script("a", false, b"x")).unwrap();
        let b = writer.insert(&script("b", false, b"y")).unwrap();
        let set = "UPDATE sieve_scripts SET is_active = 1 WHERE id = ?1";
        archive.connection().execute(set, [a]).unwrap();
        let err = archive.connection().execute(set, [b]).unwrap_err();
        assert!(err.to_string().contains("UNIQUE"), "{err}");
    }
}
