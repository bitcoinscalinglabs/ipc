//! A store of the EOTS votes a node keeps to check again, with an upper bound per validator.

use std::collections::BTreeMap;

/// The EOTS votes a node keeps to check again (`slashing.md`, "Accepting an EOTS vote"): the first
/// vote per validator and subnet height, at most `cap` per validator, those of the lowest heights.
pub(super) struct KeptVotes<K, V> {
    cap: usize,
    by_validator: BTreeMap<K, BTreeMap<u64, V>>,
}

impl<K: Ord, V> KeptVotes<K, V> {
    /// A store that keeps at most `cap` votes per validator; with `cap` 0 it keeps none.
    pub(super) fn new(cap: usize) -> Self {
        Self {
            cap,
            by_validator: BTreeMap::new(),
        }
    }

    /// Keeps the vote of `validator` at `height`, unless it keeps one there already. A validator
    /// at its cap keeps the lowest heights: the vote replaces the validator's highest kept vote if
    /// it is below it, and is refused otherwise. Returns whether a vote of the validator at
    /// `height` is kept.
    pub(super) fn keep(&mut self, validator: K, height: u64, vote: V) -> bool {
        if self.cap == 0 {
            return false;
        }
        let votes = self.by_validator.entry(validator).or_default();
        if votes.contains_key(&height) {
            return true;
        }
        if votes.len() >= self.cap {
            match votes.last_entry() {
                Some(highest) if *highest.key() > height => {
                    highest.remove();
                }
                _ => return false,
            }
        }
        votes.insert(height, vote);
        true
    }

    /// Deletes the votes at or below `height`. Visits every validator with kept votes, at most the
    /// validator set: O(v + r log n) for v such validators, r deleted votes and n = `cap`.
    pub(super) fn delete_up_to(&mut self, height: u64) {
        self.take_up_to(height);
    }

    /// Takes the votes at or below `height` out of the store.
    pub(super) fn take_up_to(&mut self, height: u64) -> Vec<V> {
        let mut taken = Vec::new();
        self.by_validator.retain(|_, votes| {
            while let Some(lowest) = votes.first_entry().filter(|e| *e.key() <= height) {
                taken.push(lowest.remove());
            }
            !votes.is_empty()
        });
        taken
    }

    /// Takes all kept votes out of the store.
    pub(super) fn take(&mut self) -> impl Iterator<Item = V> {
        std::mem::take(&mut self.by_validator)
            .into_values()
            .flat_map(BTreeMap::into_values)
    }

    /// The number of kept votes.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.by_validator.values().map(BTreeMap::len).sum()
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.by_validator.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::KeptVotes;

    fn kept(store: &mut KeptVotes<u8, (u8, u64)>) -> Vec<(u8, u64)> {
        store.take().collect()
    }

    #[test]
    fn the_first_vote_per_validator_and_height_is_kept() {
        let mut store = KeptVotes::new(2);
        assert!(store.keep(1, 5, (1, 5)));
        assert!(store.keep(1, 5, (9, 9)));
        assert_eq!(store.len(), 1);
        assert_eq!(kept(&mut store), vec![(1, 5)]);
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn a_validator_at_its_cap_keeps_the_lowest_heights() {
        let mut store = KeptVotes::new(3);
        for h in [4, 6, 8] {
            assert!(store.keep(1, h, (1, h)));
        }
        // Above or at the highest kept height: refused
        assert!(!store.keep(1, 9, (1, 9)));
        assert!(store.keep(1, 8, (1, 8)));
        assert_eq!(store.len(), 3);
        // Below it: replaces it
        assert!(store.keep(1, 7, (1, 7)));
        assert!(store.keep(1, 2, (1, 2)));
        assert_eq!(store.len(), 3);
        assert!(!store.keep(1, 8, (1, 8)));
        assert_eq!(kept(&mut store), vec![(1, 2), (1, 4), (1, 6)]);
    }

    #[test]
    fn a_validator_at_its_cap_does_not_affect_another() {
        let mut store = KeptVotes::new(2);
        assert!(store.keep(1, 1, (1, 1)));
        assert!(store.keep(1, 2, (1, 2)));
        assert!(!store.keep(1, 3, (1, 3)));
        assert!(store.keep(2, 3, (2, 3)));
        assert!(store.keep(2, 4, (2, 4)));
        assert_eq!(kept(&mut store), vec![(1, 1), (1, 2), (2, 3), (2, 4)]);
    }

    #[test]
    fn deletion_removes_the_votes_at_or_below_the_height_and_frees_the_slots() {
        let mut store = KeptVotes::new(2);
        for (v, h) in [(1, 3), (1, 5), (2, 4)] {
            assert!(store.keep(v, h, (v, h)));
        }
        assert!(!store.keep(1, 6, (1, 6)));
        store.delete_up_to(4);
        assert_eq!(store.len(), 1);
        // Validator 2 has no vote left and no entry
        assert_eq!(store.by_validator.len(), 1);
        assert!(store.keep(1, 6, (1, 6)));
        assert_eq!(kept(&mut store), vec![(1, 5), (1, 6)]);
    }

    #[test]
    fn taking_up_to_a_height_leaves_the_votes_above_it() {
        let mut store = KeptVotes::new(3);
        for (v, h) in [(1, 3), (1, 5), (2, 4), (2, 6)] {
            assert!(store.keep(v, h, (v, h)));
        }
        assert_eq!(store.take_up_to(4), vec![(1, 3), (2, 4)]);
        assert_eq!(kept(&mut store), vec![(1, 5), (2, 6)]);
    }

    #[test]
    fn a_zero_cap_keeps_no_vote() {
        let mut store = KeptVotes::new(0);
        assert!(!store.keep(1, 0, (1, 0)));
        assert!(!store.keep(1, u64::MAX, (1, u64::MAX)));
        assert!(store.is_empty());
        store.delete_up_to(u64::MAX);
        assert!(kept(&mut store).is_empty());
    }

    #[test]
    fn a_cap_of_one_keeps_the_lowest_height() {
        let mut store = KeptVotes::new(1);
        assert!(store.keep(1, u64::MAX, (1, u64::MAX)));
        assert!(store.keep(1, 0, (1, 0)));
        assert!(!store.keep(1, u64::MAX, (1, u64::MAX)));
        assert_eq!(kept(&mut store), vec![(1, 0)]);
        assert!(store.is_empty());
        assert!(store.keep(1, u64::MAX, (1, u64::MAX)));
        store.delete_up_to(u64::MAX);
        assert!(store.is_empty());
        assert!(kept(&mut store).is_empty());
    }
}
