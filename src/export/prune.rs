/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DestroyOrder {
    LeafFirst,
    ById,
}

#[derive(Debug, Clone)]
pub(super) struct TargetObj<'t> {
    pub(super) id: &'t str,
    pub(super) matched: bool,
    pub(super) protected: bool,
    pub(super) may_delete: bool,
    pub(super) parent: Option<&'t str>,
}

impl<'t> TargetObj<'t> {
    pub(super) fn candidates(objs: &[TargetObj<'t>], order: DestroyOrder) -> Vec<String> {
        let by_id: HashMap<&str, &TargetObj<'_>> = objs.iter().map(|o| (o.id, o)).collect();
        let protected = TargetObj::protected_with_ancestors(objs, &by_id);
        let mut out: Vec<&TargetObj<'_>> = objs
            .iter()
            .filter(|o| !o.matched && o.may_delete && !protected.contains(o.id))
            .collect();
        match order {
            DestroyOrder::LeafFirst => {
                out.sort_by_cached_key(|o| (Reverse(o.depth(&by_id)), o.id));
            }
            DestroyOrder::ById => out.sort_unstable_by_key(|o| o.id),
        }
        out.into_iter().map(|o| o.id.to_owned()).collect()
    }

    fn protected_with_ancestors<'m>(
        objs: &'m [TargetObj<'t>],
        by_id: &HashMap<&str, &'m TargetObj<'t>>,
    ) -> HashSet<&'t str> {
        let mut protected = HashSet::new();
        for o in objs.iter().filter(|o| o.protected) {
            let mut cur = Some(o.id);
            while let Some(id) = cur {
                if !protected.insert(id) {
                    break;
                }
                cur = by_id
                    .get(id)
                    .and_then(|x| x.parent)
                    .filter(|p| by_id.contains_key(p));
            }
        }
        protected
    }

    fn depth(&self, by_id: &HashMap<&str, &TargetObj<'t>>) -> usize {
        let mut depth = 0;
        let mut cur = self.parent;
        let mut seen: HashSet<&str> = HashSet::new();
        while let Some(p) = cur {
            if !seen.insert(p) || !by_id.contains_key(p) {
                break;
            }
            depth += 1;
            cur = by_id.get(p).and_then(|o| o.parent);
        }
        depth
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o<'t>(
        id: &'t str,
        matched: bool,
        protected: bool,
        parent: Option<&'t str>,
    ) -> TargetObj<'t> {
        TargetObj {
            id,
            matched,
            protected,
            may_delete: true,
            parent,
        }
    }

    #[test]
    fn unmatched_unprotected_are_candidates() {
        let objs = vec![
            o("a", true, false, None),
            o("b", false, false, None),
            o("c", false, true, None),
        ];
        assert_eq!(
            TargetObj::candidates(&objs, DestroyOrder::ById),
            vec!["b".to_owned()]
        );
    }

    #[test]
    fn protected_excludes_ancestors() {
        let objs = vec![
            o("root", false, false, None),
            o("mid", false, false, Some("root")),
            o("leaf", false, true, Some("mid")),
        ];
        assert!(TargetObj::candidates(&objs, DestroyOrder::LeafFirst).is_empty());
    }

    #[test]
    fn may_delete_false_is_skipped() {
        let mut x = o("x", false, false, None);
        x.may_delete = false;
        assert!(TargetObj::candidates(&[x], DestroyOrder::ById).is_empty());
    }

    #[test]
    fn tree_destroy_is_leaf_first() {
        let objs = vec![
            o("root", false, false, None),
            o("child", false, false, Some("root")),
            o("grand", false, false, Some("child")),
        ];
        assert_eq!(
            TargetObj::candidates(&objs, DestroyOrder::LeafFirst),
            vec!["grand".to_owned(), "child".to_owned(), "root".to_owned()]
        );
    }

    #[test]
    fn leaf_first_breaks_depth_ties_by_id() {
        let objs = vec![
            o("r", false, false, None),
            o("b", false, false, Some("r")),
            o("a", false, false, Some("r")),
            o("loop1", false, false, Some("loop2")),
            o("loop2", false, false, Some("loop1")),
        ];
        assert_eq!(
            TargetObj::candidates(&objs, DestroyOrder::LeafFirst),
            ["loop1", "loop2", "a", "b", "r"].map(str::to_owned)
        );
    }
}
