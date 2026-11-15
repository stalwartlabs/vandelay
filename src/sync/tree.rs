/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::cmp::Reverse;
use std::collections::HashMap;
use std::hash::Hash;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DanglingParent {
    Counts,
    Ignored,
}

#[derive(Debug, Clone)]
pub struct ParentMap<K> {
    parents: HashMap<K, Option<K>>,
    dangling: DanglingParent,
}

impl<K: Eq + Hash> ParentMap<K> {
    pub fn new(dangling: DanglingParent) -> ParentMap<K> {
        ParentMap {
            parents: HashMap::new(),
            dangling,
        }
    }

    pub fn with_capacity(dangling: DanglingParent, capacity: usize) -> ParentMap<K> {
        ParentMap {
            parents: HashMap::with_capacity(capacity),
            dangling,
        }
    }

    pub fn insert(&mut self, child: K, parent: Option<K>) {
        self.parents.insert(child, parent);
    }

    pub fn parent(&self, child: &K) -> Option<&K> {
        self.parents.get(child).and_then(Option::as_ref)
    }

    pub fn depth(&self, key: &K) -> usize {
        self.depth_with(key, &mut Vec::new())
    }

    fn depth_with<'s>(&'s self, key: &K, seen: &mut Vec<&'s K>) -> usize {
        seen.clear();
        let mut depth = 0;
        let mut next = self.parent(key);
        while let Some(parent) = next {
            if self.dangling == DanglingParent::Ignored && !self.parents.contains_key(parent) {
                break;
            }
            depth += 1;
            if parent == key || seen.contains(&parent) {
                break;
            }
            seen.push(parent);
            next = self.parent(parent);
        }
        depth
    }

    pub fn parents_first<T>(&self, items: &mut [T], key: impl Fn(&T) -> &K) {
        let mut seen = Vec::new();
        items.sort_by_cached_key(|item| self.depth_with(key(item), &mut seen));
    }

    pub fn children_first<T>(&self, items: &mut [T], key: impl Fn(&T) -> &K) {
        let mut seen = Vec::new();
        items.sort_by_cached_key(|item| Reverse(self.depth_with(key(item), &mut seen)));
    }
}
impl<K: Eq + Hash> Extend<(K, Option<K>)> for ParentMap<K> {
    fn extend<I: IntoIterator<Item = (K, Option<K>)>>(&mut self, iter: I) {
        self.parents.extend(iter);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FolderPath<'a> {
    path: &'a str,
    delimiter: char,
}

impl<'a> FolderPath<'a> {
    pub fn new(path: &'a str, delimiter: char) -> FolderPath<'a> {
        FolderPath { path, delimiter }
    }

    pub fn depth(self) -> usize {
        self.path.matches(self.delimiter).count()
    }

    pub fn parent(self) -> Option<&'a str> {
        self.path
            .rsplit_once(self.delimiter)
            .map(|(parent, _)| parent)
    }

    pub fn leaf(self) -> &'a str {
        self.path
            .rsplit_once(self.delimiter)
            .map_or(self.path, |(_, leaf)| leaf)
    }

    pub fn sort_children_first(paths: &mut [String], delimiter: char) {
        FolderPath::sort_children_first_by(paths, delimiter, String::as_str);
    }

    pub fn sort_children_first_by<T>(items: &mut [T], delimiter: char, path: impl Fn(&T) -> &str) {
        items.sort_by(|a, b| {
            let (a, b) = (path(a), path(b));
            let depth_a = FolderPath::new(a, delimiter).depth();
            let depth_b = FolderPath::new(b, delimiter).depth();
            depth_b.cmp(&depth_a).then_with(|| a.cmp(b))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(dangling: DanglingParent, edges: &[(i64, Option<i64>)]) -> ParentMap<i64> {
        let mut m = ParentMap::new(dangling);
        m.extend(edges.iter().copied());
        m
    }

    #[test]
    fn depth_walks_to_the_root() {
        let m = map(
            DanglingParent::Counts,
            &[(1, None), (2, Some(1)), (3, Some(2))],
        );
        assert_eq!(m.depth(&1), 0);
        assert_eq!(m.depth(&3), 2);
        assert_eq!(m.depth(&99), 0);
    }

    #[test]
    fn dangling_parent_policy() {
        let edges = [(1, Some(50)), (2, Some(1))];
        assert_eq!(map(DanglingParent::Counts, &edges).depth(&2), 2);
        assert_eq!(map(DanglingParent::Ignored, &edges).depth(&2), 1);
    }

    #[test]
    fn cycles_terminate() {
        let m = map(
            DanglingParent::Counts,
            &[(1, Some(2)), (2, Some(1)), (3, Some(3))],
        );
        assert_eq!(m.depth(&1), 2);
        assert_eq!(m.depth(&3), 1);
    }

    #[test]
    fn orders_items_by_depth_stably() {
        let m = map(
            DanglingParent::Counts,
            &[(1, None), (2, Some(1)), (3, Some(2)), (4, None)],
        );
        let mut items = vec![3, 1, 2, 4];
        m.parents_first(&mut items, |i| i);
        assert_eq!(items, [1, 4, 2, 3]);
        m.children_first(&mut items, |i| i);
        assert_eq!(items, [3, 2, 1, 4]);
    }

    #[test]
    fn folder_path_parts() {
        let p = FolderPath::new("A/B/C", '/');
        assert_eq!(p.depth(), 2);
        assert_eq!(p.parent(), Some("A/B"));
        assert_eq!(p.leaf(), "C");
        let root = FolderPath::new("INBOX", '.');
        assert_eq!(root.depth(), 0);
        assert_eq!(root.parent(), None);
        assert_eq!(root.leaf(), "INBOX");
    }

    #[test]
    fn sort_children_first_deepest_then_name() {
        let mut names = vec![
            "A".to_owned(),
            "A/B/C".to_owned(),
            "Z/Y".to_owned(),
            "A/B".to_owned(),
        ];
        FolderPath::sort_children_first(&mut names, '/');
        assert_eq!(names, ["A/B/C", "A/B", "Z/Y", "A"]);
    }
}
