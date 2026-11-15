/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use crate::archive::LocalId;
use crate::sync::PlanCounts;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdDiff<K> {
    pub new: Vec<K>,
    pub present: Vec<(K, LocalId)>,
    pub vanished: Vec<(K, LocalId)>,
}

impl<K: Eq + Hash + Ord + Clone> IdDiff<K> {
    pub fn compute(server: impl IntoIterator<Item = K>, local: &HashMap<K, LocalId>) -> IdDiff<K> {
        let server = server.into_iter();
        let mut seen = HashSet::with_capacity(server.size_hint().0);
        let mut new = Vec::new();
        let mut present = Vec::with_capacity(local.len());
        for key in server {
            if seen.contains(&key) {
                continue;
            }
            match local.get(&key) {
                Some(&id) => present.push((key.clone(), id)),
                None => new.push(key.clone()),
            }
            seen.insert(key);
        }
        let mut vanished: Vec<(K, LocalId)> = local
            .iter()
            .filter(|(key, _)| !seen.contains(*key))
            .map(|(key, &id)| (key.clone(), id))
            .collect();
        vanished.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        IdDiff {
            new,
            present,
            vanished,
        }
    }

    pub fn is_unchanged(&self) -> bool {
        self.new.is_empty() && self.vanished.is_empty()
    }

    pub fn plan(&self) -> PlanCounts {
        PlanCounts {
            new: self.new.len() as u64,
            vanished: self.vanished.len() as u64,
            present: self.present.len() as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(pairs: &[(&str, i64)]) -> HashMap<String, LocalId> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), LocalId::from_raw(*v)))
            .collect()
    }

    fn server(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn partitions_new_present_vanished() {
        let d = IdDiff::compute(
            server(&["a", "b", "c"]),
            &local(&[("b", 2), ("c", 3), ("e", 5), ("d", 4)]),
        );
        assert_eq!(d.new, ["a"]);
        assert_eq!(
            d.present,
            [
                ("b".to_owned(), LocalId::from_raw(2)),
                ("c".to_owned(), LocalId::from_raw(3))
            ]
        );
        assert_eq!(
            d.vanished,
            [
                ("d".to_owned(), LocalId::from_raw(4)),
                ("e".to_owned(), LocalId::from_raw(5))
            ]
        );
        assert!(!d.is_unchanged());
    }

    #[test]
    fn duplicate_server_ids_count_once_in_server_order() {
        let d = IdDiff::compute(server(&["z", "a", "z", "a", "b"]), &local(&[("b", 1)]));
        assert_eq!(d.new, ["z", "a"]);
        assert_eq!(d.present.len(), 1);
        assert!(d.vanished.is_empty());
    }

    #[test]
    fn composite_keys_work() {
        let mut map = HashMap::new();
        map.insert((7u32, 1u32), LocalId::from_raw(1));
        let d = IdDiff::compute([(7u32, 2u32)], &map);
        assert_eq!(d.new, [(7, 2)]);
        assert_eq!(d.vanished, [((7, 1), LocalId::from_raw(1))]);
    }

    #[test]
    fn empty_both_sides_is_unchanged() {
        let d = IdDiff::<String>::compute(Vec::new(), &HashMap::new());
        assert!(d.is_unchanged());
    }
}
