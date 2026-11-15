/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};

use rusqlite::params;
use serde_json::Value;

use super::prune::{DestroyOrder, TargetObj};
use super::target::{ClientId, Maps, TargetJson};
use super::uploader::Uploader;
use super::{Exporter, Plan};
use crate::archive::LocalId;
use crate::error::Error;
use crate::jmap::request::SetOutcome;
use crate::jmap::wire::JmapId;
use crate::jmap::wire::file_node::NodeKind;
use crate::jmap::wire::mailbox::Mailbox;
use crate::sync::TypeCounts;
use crate::sync::jmap_map::FileNodeRow;
use crate::sync::keys::FoldedName;
use crate::types::ObjectType;

const TARGET_PROPS: [&str; 4] = ["role", "name", "parentId", "myRights"];
const DEFAULT_MEDIA_TYPE: &str = "application/octet-stream";

struct LocalNode {
    local: LocalId,
    parent: Option<LocalId>,
    name: FoldedName,
    role: Option<String>,
}

struct TargetNode {
    id: String,
    parent: Option<String>,
    name: FoldedName,
    role: Option<String>,
    may_delete: bool,
}

struct TargetTree<'t> {
    roots: Vec<&'t TargetNode>,
    children: HashMap<&'t str, Vec<&'t TargetNode>>,
}

enum ParentTarget {
    Root,
    Created(JmapId),
    Missing(LocalId),
}

impl LocalNode {
    fn depth(&self, parents: &HashMap<LocalId, Option<LocalId>>) -> usize {
        let mut depth = 0;
        let mut cur = parents.get(&self.local).copied().flatten();
        let mut seen = HashSet::new();
        while let Some(p) = cur {
            if !seen.insert(p) {
                break;
            }
            depth += 1;
            cur = parents.get(&p).copied().flatten();
        }
        depth
    }

    fn parent_target(&self, ty: ObjectType, maps: &Maps) -> ParentTarget {
        match self.parent {
            None => ParentTarget::Root,
            Some(p) => maps
                .target(ty, p)
                .map_or(ParentTarget::Missing(p), ParentTarget::Created),
        }
    }
}

impl ParentTarget {
    fn id(&self) -> Option<&str> {
        match self {
            ParentTarget::Created(id) => Some(&id.0),
            ParentTarget::Root | ParentTarget::Missing(_) => None,
        }
    }
}

impl<'t> TargetTree<'t> {
    fn new(targets: &'t [TargetNode]) -> TargetTree<'t> {
        let mut tree = TargetTree {
            roots: Vec::new(),
            children: HashMap::new(),
        };
        for t in targets {
            match t.parent.as_deref() {
                None => tree.roots.push(t),
                Some(p) => tree.children.entry(p).or_default().push(t),
            }
        }
        tree
    }

    fn children(&self, parent: Option<&str>) -> &[&'t TargetNode] {
        match parent {
            None => &self.roots,
            Some(p) => self.children.get(p).map_or(&[], Vec::as_slice),
        }
    }

    fn named(&self, parent: Option<&str>, name: &FoldedName) -> Option<&'t TargetNode> {
        self.children(parent)
            .iter()
            .find(|t| &t.name == name)
            .copied()
    }
}

impl Exporter<'_> {
    pub(super) fn reconcile_tree(
        &self,
        ty: ObjectType,
        maps: &mut Maps,
        counts: &mut TypeCounts,
    ) -> Result<Plan, Error> {
        let locals = self.tree_locals(ty)?;
        let targets = self.tree_targets(ty)?;
        let tree = TargetTree::new(&targets);

        let mut matched: HashMap<LocalId, &str> = HashMap::new();
        let mut tmatched: HashSet<String> = HashSet::new();

        if ty == ObjectType::Mailbox {
            for t in &targets {
                if let Some(r) = &t.role
                    && let Some(l) = locals.iter().find(|l| {
                        l.role.as_deref() == Some(r.as_str()) && !matched.contains_key(&l.local)
                    })
                {
                    matched.insert(l.local, &t.id);
                    tmatched.insert(t.id.clone());
                }
            }
        }

        let mut local_children: HashMap<Option<LocalId>, Vec<&LocalNode>> = HashMap::new();
        for n in &locals {
            local_children.entry(n.parent).or_default().push(n);
        }
        let mut stack: Vec<(Option<LocalId>, Option<&str>)> = Vec::with_capacity(matched.len() + 1);
        stack.push((None, None));
        stack.extend(matched.iter().map(|(l, t)| (Some(*l), Some(*t))));
        while let Some((lp, tp)) = stack.pop() {
            for ln in local_children.get(&lp).into_iter().flatten() {
                if matched.contains_key(&ln.local) {
                    continue;
                }
                if let Some(tn) = tree
                    .children(tp)
                    .iter()
                    .find(|t| !tmatched.contains(&t.id) && t.name == ln.name)
                {
                    matched.insert(ln.local, &tn.id);
                    tmatched.insert(tn.id.clone());
                    stack.push((Some(ln.local), Some(&tn.id)));
                }
            }
        }

        for (l, tid) in &matched {
            maps.insert(ty, *l, JmapId((*tid).to_owned()));
        }
        counts.skipped += matched.len() as u64;

        let parents: HashMap<LocalId, Option<LocalId>> =
            locals.iter().map(|n| (n.local, n.parent)).collect();
        let mut to_create: Vec<(usize, &LocalNode)> = locals
            .iter()
            .filter(|n| !matched.contains_key(&n.local))
            .map(|n| (n.depth(&parents), n))
            .collect();
        to_create.sort_by_key(|(depth, _)| *depth);

        let mut uploader = Uploader::new(self);
        let mut taken_roles: HashSet<&str> =
            targets.iter().filter_map(|t| t.role.as_deref()).collect();
        for level in to_create.chunk_by(|a, b| a.0 == b.0) {
            let mut batch: Vec<(ClientId, Value)> = Vec::new();
            for (_, n) in level {
                let parent = match n.parent_target(ty, maps) {
                    ParentTarget::Missing(p) => {
                        self.logger.warn(format_args!(
                            "{} local {} skipped: parent {p} not created",
                            ty.noun(),
                            n.local
                        ));
                        counts.failed += 1;
                        continue;
                    }
                    parent => parent,
                };
                if let Some(t) = tree.named(parent.id(), &n.name) {
                    self.record_merge(
                        ty,
                        n.local,
                        t.id.clone(),
                        "name already present on target",
                        maps,
                        &mut tmatched,
                    );
                    counts.skipped += 1;
                    continue;
                }
                if ty == ObjectType::FileNode {
                    let cid = ClientId::object(n.local);
                    uploader.clear_touched();
                    let obj = match self.tree_create(ty, n.local, maps, &mut uploader) {
                        Ok(o) => o,
                        Err(e) => {
                            self.warn_tree_skip(ty, n.local, &e, counts);
                            continue;
                        }
                    };
                    let outcome = self.create(ty, vec![(cid.clone(), obj)])?;
                    match self.retry_if_blob_missing(ty, &cid, &mut uploader, outcome, |up| {
                        self.tree_create(ty, n.local, maps, up)
                    }) {
                        Ok(outcome) => {
                            self.absorb_tree_outcome(ty, &outcome, maps, &mut tmatched, counts)
                        }
                        Err(e) => self.warn_tree_skip(ty, n.local, &e, counts),
                    }
                    continue;
                }
                match self.tree_create(ty, n.local, maps, &mut uploader) {
                    Ok(mut obj) => {
                        if let Some(r) = n.role.as_deref() {
                            if taken_roles.contains(r) {
                                if let Value::Object(m) = &mut obj {
                                    m.remove("role");
                                }
                                self.logger.warn(format_args!(
                                    "mailbox local {}: role \"{r}\" already on the target, creating a plain folder",
                                    n.local
                                ));
                            } else {
                                taken_roles.insert(r);
                            }
                        }
                        batch.push((ClientId::object(n.local), obj));
                    }
                    Err(e) => self.warn_tree_skip(ty, n.local, &e, counts),
                }
            }
            if batch.is_empty() {
                continue;
            }
            let outcome = self.create(ty, batch)?;
            self.absorb_tree_outcome(ty, &outcome, maps, &mut tmatched, counts);
        }

        let objs: Vec<TargetObj<'_>> = targets
            .iter()
            .map(|t| TargetObj {
                id: &t.id,
                matched: tmatched.contains(&t.id),
                protected: t.role.is_some(),
                may_delete: t.may_delete,
                parent: t.parent.as_deref(),
            })
            .collect();
        Ok(Plan::prune(TargetObj::candidates(
            &objs,
            DestroyOrder::LeafFirst,
        )))
    }

    fn tree_locals(&self, ty: ObjectType) -> Result<Vec<LocalNode>, Error> {
        let table = ty.table();
        let mut stmt = self
            .store
            .connection()
            .prepare(&format!("SELECT id, parent_id, name, role FROM {table}"))
            .map_err(Error::partial)?;
        stmt.query_map([], |r| {
            Ok(LocalNode {
                local: r.get(0)?,
                parent: r.get(1)?,
                name: FoldedName::new(&r.get::<_, String>(2)?),
                role: r.get(3)?,
            })
        })
        .and_then(Iterator::collect)
        .map_err(Error::partial)
    }

    fn tree_targets(&self, ty: ObjectType) -> Result<Vec<TargetNode>, Error> {
        let list = self.query_get(ty, Some(&TARGET_PROPS))?;
        let mut own_rights: HashMap<&str, bool> = HashMap::with_capacity(list.len());
        for v in &list {
            let v = TargetJson(v);
            if let (Some(id), Some(may)) = (v.id(), v.may_delete()) {
                own_rights.insert(id, may);
            }
        }
        Ok(list
            .iter()
            .filter_map(|v| {
                let v = TargetJson(v);
                let id = v.id()?;
                let parent = v.text("parentId");
                let may_delete = own_rights
                    .get(id)
                    .or_else(|| parent.and_then(|p| own_rights.get(p)))
                    .copied()
                    .unwrap_or(true);
                Some(TargetNode {
                    id: id.to_owned(),
                    parent: parent.map(str::to_owned),
                    name: FoldedName::new(v.text("name")?),
                    role: v.text("role").map(str::to_owned),
                    may_delete,
                })
            })
            .collect())
    }

    fn record_merge(
        &self,
        ty: ObjectType,
        local: LocalId,
        target_id: String,
        reason: &str,
        maps: &mut Maps,
        tmatched: &mut HashSet<String>,
    ) {
        self.logger.warn(format_args!(
            "{} local {local} merged into existing target {target_id}: {reason}",
            ty.noun()
        ));
        maps.insert(ty, local, JmapId(target_id.clone()));
        tmatched.insert(target_id);
    }

    fn absorb_tree_outcome(
        &self,
        ty: ObjectType,
        outcome: &SetOutcome,
        maps: &mut Maps,
        tmatched: &mut HashSet<String>,
        counts: &mut TypeCounts,
    ) {
        maps.record_created(ty, outcome, counts);
        for (cid, err) in &outcome.not_created {
            if let Some(local) = ClientId::parse(cid)
                && let Some(existing) = TargetJson(err).existing_id()
            {
                self.record_merge(
                    ty,
                    local,
                    existing.to_owned(),
                    "alreadyExists: reusing existing target",
                    maps,
                    tmatched,
                );
                counts.skipped += 1;
                continue;
            }
            self.warn_not_created(ty, cid, err, counts);
        }
    }

    fn warn_tree_skip(&self, ty: ObjectType, local: LocalId, e: &Error, counts: &mut TypeCounts) {
        self.logger
            .warn(format_args!("{} local {local} skipped: {e}", ty.noun()));
        counts.failed += 1;
    }

    fn tree_create(
        &self,
        ty: ObjectType,
        local: LocalId,
        maps: &Maps,
        uploader: &mut Uploader<'_>,
    ) -> Result<Value, Error> {
        let conn = self.store.connection();
        let resolve = |ty, id| maps.target(ty, id);
        if ty == ObjectType::Mailbox {
            let wire = conn
                .query_row(
                    &format!("{} WHERE id = ?1", Mailbox::SELECT),
                    params![local],
                    |row| Ok(Mailbox::from_row(row, &resolve)),
                )
                .map_err(Error::partial)??;
            return serde_json::to_value(&wire).map_err(Error::partial);
        }
        let row = conn
            .query_row(
                &format!("{} WHERE id = ?1", FileNodeRow::SELECT),
                params![local],
                |row| Ok(FileNodeRow::from_row(row, &resolve)),
            )
            .map_err(Error::partial)??;
        let mut wire = row.wire;
        if let (NodeKind::File(file), Some(blob)) = (&mut wire.kind, row.blob) {
            let media_type = file.media_type.as_deref().unwrap_or(DEFAULT_MEDIA_TYPE);
            file.blob_id = Some(uploader.upload(blob, media_type)?);
        }
        let mut obj = serde_json::to_value(&wire).map_err(Error::partial)?;
        if let Value::Object(m) = &mut obj {
            m.remove("created");
            m.remove("modified");
            m.remove("isSubscribed");
        }
        Ok(obj)
    }
}
