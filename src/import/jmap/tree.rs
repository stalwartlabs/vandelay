/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;

use serde_json::Value;

use crate::types::ObjectType;

pub(super) struct TreeOrder {
    pub(super) order: Vec<usize>,
    pub(super) orphans: Vec<usize>,
    pub(super) cycle_roots: Vec<usize>,
}

impl TreeOrder {
    pub(super) fn compute(
        items: &[(String, Option<String>)],
        is_known: impl Fn(&str) -> bool,
    ) -> TreeOrder {
        let index: HashMap<&str, usize> = items
            .iter()
            .enumerate()
            .map(|(i, (id, _))| (id.as_str(), i))
            .collect();
        let mut order = Vec::with_capacity(items.len());
        let mut orphans = Vec::new();
        let mut cycle_roots = Vec::new();
        let mut placed = vec![false; items.len()];

        while order.len() < items.len() {
            let mut progressed = false;
            for (i, (_, parent)) in items.iter().enumerate() {
                if placed.get(i).copied().unwrap_or(true) {
                    continue;
                }
                let ready = match parent {
                    None => true,
                    Some(p) => match index.get(p.as_str()) {
                        Some(&pi) => placed.get(pi).copied().unwrap_or(false),
                        None => {
                            if !is_known(p) {
                                orphans.push(i);
                            }
                            true
                        }
                    },
                };
                if ready {
                    order.push(i);
                    if let Some(slot) = placed.get_mut(i) {
                        *slot = true;
                    }
                    progressed = true;
                }
            }
            if !progressed {
                let Some((root, _)) = items
                    .iter()
                    .zip(&placed)
                    .enumerate()
                    .filter(|(_, (_, placed))| !**placed)
                    .min_by(|(_, (a, _)), (_, (b, _))| a.0.cmp(&b.0))
                else {
                    break;
                };
                cycle_roots.push(root);
                order.push(root);
                if let Some(slot) = placed.get_mut(root) {
                    *slot = true;
                }
            }
        }

        TreeOrder {
            order,
            orphans,
            cycle_roots,
        }
    }

    pub(super) fn of_objects(
        ty: ObjectType,
        objects: &[Value],
        is_known: impl Fn(&str) -> bool,
    ) -> (Vec<usize>, Vec<String>) {
        if !ty.is_tree() {
            return ((0..objects.len()).collect(), Vec::new());
        }
        let items: Vec<(String, Option<String>)> = objects
            .iter()
            .map(|o| {
                (
                    o.get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    o.get("parentId").and_then(Value::as_str).map(str::to_owned),
                )
            })
            .collect();
        let result = TreeOrder::compute(&items, is_known);
        let mut warnings = Vec::with_capacity(result.orphans.len() + result.cycle_roots.len());
        for (id, parent) in result.orphans.iter().filter_map(|&i| items.get(i)) {
            warnings.push(format!(
                "{} {id}: parent {} not present in batch or archive; inserting with NULL parent",
                ty.jmap_name(),
                parent.as_deref().unwrap_or("<unknown>"),
            ));
        }
        for (id, _) in result.cycle_roots.iter().filter_map(|&i| items.get(i)) {
            warnings.push(format!(
                "{} {id}: parent cycle detected; broken at this node",
                ty.jmap_name(),
            ));
        }
        (result.order, warnings)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn item(id: &str, parent: Option<&str>) -> (String, Option<String>) {
        (id.to_owned(), parent.map(str::to_owned))
    }

    fn pos(order: &[usize], items: &[(String, Option<String>)], id: &str) -> usize {
        order
            .iter()
            .position(|&i| items[i].0 == id)
            .expect("id in order")
    }

    #[test]
    fn parents_precede_children() {
        let items = vec![item("c", Some("b")), item("b", Some("a")), item("a", None)];
        let t = TreeOrder::compute(&items, |_| false);
        assert_eq!(t.order.len(), 3);
        assert!(pos(&t.order, &items, "a") < pos(&t.order, &items, "b"));
        assert!(pos(&t.order, &items, "b") < pos(&t.order, &items, "c"));
        assert!(t.orphans.is_empty());
        assert!(t.cycle_roots.is_empty());
    }

    #[test]
    fn parent_not_fetched_and_not_existing_is_orphan() {
        let items = vec![item("x", Some("missing"))];
        let t = TreeOrder::compute(&items, |_| false);
        assert_eq!(t.order, vec![0]);
        assert_eq!(t.orphans, vec![0]);
    }

    #[test]
    fn parent_already_in_db_is_not_orphan() {
        let items = vec![item("x", Some("p"))];
        let existing: HashSet<String> = ["p".to_owned()].into_iter().collect();
        let t = TreeOrder::compute(&items, |p| existing.contains(p));
        assert_eq!(t.order, vec![0]);
        assert!(t.orphans.is_empty());
    }

    #[test]
    fn cycle_is_broken_by_smallest_id() {
        let items = vec![item("b", Some("a")), item("a", Some("b"))];
        let t = TreeOrder::compute(&items, |_| false);
        assert_eq!(t.order.len(), 2);
        assert_eq!(t.cycle_roots.len(), 1);
        assert_eq!(items[t.cycle_roots[0]].0, "a");
        assert!(pos(&t.order, &items, "a") < pos(&t.order, &items, "b"));
    }
}

#[cfg(test)]
mod object_tests {
    use std::collections::HashSet;

    use serde_json::json;

    use super::TreeOrder;
    use crate::types::ObjectType;

    #[test]
    fn order_objects_does_not_warn_when_parent_already_in_archive() {
        let objects = vec![json!({"id":"child","parentId":"existing"})];
        let existing: HashSet<String> = ["existing".to_owned()].into_iter().collect();
        let (ordered, warnings) =
            TreeOrder::of_objects(ObjectType::Mailbox, &objects, |p| existing.contains(p));
        assert_eq!(ordered, vec![0]);
        assert!(
            warnings.is_empty(),
            "no orphan warning expected: {warnings:?}"
        );
    }

    #[test]
    fn order_objects_warns_on_orphan_parent() {
        let objects = vec![json!({"id":"orphan","parentId":"ghost"})];
        let (ordered, warnings) = TreeOrder::of_objects(ObjectType::Mailbox, &objects, |_| false);
        assert_eq!(ordered, vec![0]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("ghost"), "msg was: {}", warnings[0]);
    }

    #[test]
    fn order_objects_warns_on_cycle() {
        let objects = vec![
            json!({"id":"a","parentId":"b"}),
            json!({"id":"b","parentId":"a"}),
        ];
        let (ordered, warnings) = TreeOrder::of_objects(ObjectType::Mailbox, &objects, |_| false);
        assert_eq!(ordered.len(), 2);
        assert!(
            warnings.iter().any(|w| w.contains("cycle")),
            "expected a cycle warning, got: {warnings:?}"
        );
    }
}
