//! Merge bases in a history of immutable records with snapshots: the
//! acceptances of targets, each after the one before it and the
//! acceptances it landed, or the revisions of a change, each after its
//! predecessors.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use origin_platform::Platform;
use origin_shared::TreeId;

use crate::store::{Store, StoreError};

/// A history, as far as it was walked: the parents and the snapshot of
/// each record in it.
#[derive(Debug)]
pub struct Ancestry<I> {
    records: HashMap<I, (Vec<I>, TreeId)>,
}

impl<I: Copy + Eq + Hash + Ord> Ancestry<I> {
    /// Walks everything `tips` descend from, asking `read` for the parents
    /// and the snapshot of each record, once.
    ///
    /// # Errors
    ///
    /// Fails as `read` does.
    pub fn walk<E>(
        tips: &[I],
        mut read: impl FnMut(I) -> Result<(Vec<I>, TreeId), E>,
    ) -> Result<Self, E> {
        let mut records = HashMap::new();
        let mut next = tips.to_vec();
        while let Some(id) = next.pop() {
            if records.contains_key(&id) {
                continue;
            }
            let record = read(id)?;
            next.extend(&record.0);
            records.insert(id, record);
        }
        Ok(Self { records })
    }

    /// The snapshot to merge `tips` from, `None` if they share no ancestor:
    /// that of their best common ancestor. Several, as histories that
    /// merged each other criss-cross have, merge into one first, as git's
    /// recursive strategy does: each from their own merge base, with what
    /// [`Store::merge_bases`] leaves where they conflict, so a merge from
    /// the result conflicts there, and under it, unless both sides agree. Writes the trees
    /// that makes, which merging reads.
    ///
    /// # Errors
    ///
    /// Fails if a tree cannot be read or written.
    pub fn merge_base<P: Platform>(
        &self,
        store: &Store<P>,
        tips: &[I],
    ) -> Result<Option<TreeId>, StoreError> {
        let groups: Vec<_> = tips.iter().map(std::slice::from_ref).collect();
        self.merge_base_of(store, &groups)
    }

    /// The snapshot to merge `groups` from, each standing for all it holds.
    fn merge_base_of<P: Platform>(
        &self,
        store: &Store<P>,
        groups: &[&[I]],
    ) -> Result<Option<TreeId>, StoreError> {
        let bases = self.best_common(groups);
        let Some((&first, others)) = bases.split_first() else {
            return Ok(None);
        };
        let mut merged = vec![first];
        let mut tree = self.snapshot(first);
        for &other in others {
            let base = self.merge_base_of(store, &[&merged, &[other]])?;
            let merge = store.merge_bases(base, tree, self.snapshot(other))?;
            store.write_all(&merge.trees)?;
            tree = merge.tree;
            merged.push(other);
        }
        Ok(Some(tree))
    }

    /// The best common ancestors of `groups`: the records that every group
    /// has one that is or descends from, but that none of the others
    /// descends from, sorted. Each record is visited a bounded number of
    /// times.
    fn best_common(&self, groups: &[&[I]]) -> Vec<I> {
        let Some((first, others)) = groups.split_first() else {
            return Vec::new();
        };
        let mut common = self.reachable(first.iter().copied());
        for group in others {
            let reachable = self.reachable(group.iter().copied());
            common.retain(|id| reachable.contains(id));
        }
        let parents = common.iter().flat_map(|&id| self.parents(id));
        let below = self.reachable(parents.copied());
        let mut bases: Vec<_> = common.difference(&below).copied().collect();
        bases.sort_unstable();
        bases
    }

    /// The records `from` are or descend from.
    fn reachable(&self, from: impl IntoIterator<Item = I>) -> HashSet<I> {
        let mut reached = HashSet::new();
        let mut next: Vec<_> = from.into_iter().collect();
        while let Some(id) = next.pop() {
            if reached.insert(id) {
                next.extend(self.parents(id));
            }
        }
        reached
    }

    fn parents(&self, id: I) -> &[I] {
        self.records.get(&id).map_or(&[], |(parents, _)| parents)
    }

    fn snapshot(&self, id: I) -> TreeId {
        self.records[&id].1
    }
}

#[cfg(test)]
mod tests {
    use origin_shared::{Record, Tree};

    use super::*;

    /// The history of `edges`, each a record and its parents.
    fn ancestry(edges: &[(u8, &[u8])]) -> Ancestry<u8> {
        let tips: Vec<_> = edges.iter().map(|&(id, _)| id).collect();
        let empty = Tree::default().encode().id().into();
        Ancestry::walk(&tips, |id| {
            let parents = edges.iter().find(|&&(of, _)| of == id).map(|&(_, p)| p);
            Ok::<_, ()>((parents.unwrap_or_default().to_vec(), empty))
        })
        .unwrap()
    }

    fn bases(history: &Ancestry<u8>, tips: &[u8]) -> Vec<u8> {
        let groups: Vec<_> = tips.iter().map(std::slice::from_ref).collect();
        history.best_common(&groups)
    }

    #[test]
    fn the_newest_shared_ancestor_wins_however_far() {
        // 2 follows 1; 3 to 6 and 7 to 10 grow apart from 2, then each
        // lands a side line forked from 1: 1 is nearer, but 2 is newer.
        let history = ancestry(&[
            (2, &[1]),
            (3, &[2]),
            (4, &[3]),
            (5, &[4]),
            (6, &[5]),
            (7, &[2]),
            (8, &[7]),
            (9, &[8]),
            (10, &[9]),
            (11, &[1]),
            (12, &[6, 11]),
            (13, &[1]),
            (14, &[10, 13]),
        ]);
        assert_eq!(bases(&history, &[12, 14]), [2]);
        assert_eq!(bases(&history, &[12, 6]), [6], "an ancestor of the other");
    }

    #[test]
    fn criss_cross_merges_have_two_bases() {
        // 2 and 3 fork from 1, then each merges the other.
        let history = ancestry(&[(2, &[1]), (3, &[1]), (4, &[2, 3]), (5, &[3, 2])]);
        assert_eq!(bases(&history, &[4, 5]), [2, 3]);
        assert_eq!(bases(&history, &[4, 5, 3]), [3]);
        // Merging those two needs their own base.
        assert_eq!(history.best_common(&[&[2], &[3]]), [1]);
    }

    #[test]
    fn unrelated_histories_have_none() {
        let history = ancestry(&[(2, &[1]), (4, &[3])]);
        assert_eq!(bases(&history, &[2, 4]), []);
    }
}
