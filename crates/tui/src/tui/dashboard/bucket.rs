//! One row state's targets in the order they entered it, with the n-th row
//! and a row's position each a `log n` walk, so a frame costs what it shows.

use super::intern::Id;

/// A slot whose target left the bucket.
const VACANT: Id = Id::MAX;

/// Prefix counts over slots (a Fenwick tree) that can grow at the end.
#[derive(Debug, Default)]
struct Counts {
    /// 1-based: `tree[0]` is unused.
    tree: Vec<u32>,
    total: u32,
}

fn lowbit(i: usize) -> usize {
    i.isolate_lowest_one()
}

impl Counts {
    fn n(&self) -> usize {
        self.tree.len().saturating_sub(1)
    }

    /// Live entries among the first `i` slots.
    fn prefix(&self, mut i: usize) -> u32 {
        let mut sum = 0u32;
        while i > 0 {
            sum = sum.wrapping_add(self.tree.get(i).copied().unwrap_or(0));
            i -= lowbit(i);
        }
        sum
    }

    fn push(&mut self, v: u32) {
        if self.tree.is_empty() {
            self.tree.push(0);
        }
        let i = self.tree.len();
        // tree[i] covers (i - lowbit(i), i]: v plus the slots before it.
        let node = v
            .wrapping_add(self.prefix(i - 1))
            .wrapping_sub(self.prefix(i - lowbit(i)));
        self.tree.push(node);
        self.total = self.total.wrapping_add(v);
    }

    /// Slot `slot` (0-based) gains or loses one.
    fn bump(&mut self, slot: usize, up: bool) {
        let n = self.n();
        let mut i = slot + 1;
        while i <= n {
            if let Some(t) = self.tree.get_mut(i) {
                *t = if up {
                    t.wrapping_add(1)
                } else {
                    t.wrapping_sub(1)
                };
            }
            i += lowbit(i);
        }
        self.total = if up {
            self.total.wrapping_add(1)
        } else {
            self.total.wrapping_sub(1)
        };
    }

    /// The slot of the `k`-th (0-based) live entry.
    fn select(&self, k: usize) -> Option<usize> {
        let n = self.n();
        let mut rem = u32::try_from(k).ok()?.checked_add(1)?;
        if rem > self.total {
            return None;
        }
        let mut pos = 0usize;
        let mut step = if n == 0 { 0 } else { 1usize << n.ilog2() };
        while step > 0 {
            let next = pos + step;
            if next <= n {
                let t = self.tree.get(next).copied().unwrap_or(0);
                if t < rem {
                    pos = next;
                    rem -= t;
                }
            }
            step >>= 1;
        }
        // `pos` slots hold fewer than k + 1 entries; the next one is it.
        (pos < n).then_some(pos)
    }

    /// Rebuilt from one value per slot, in `O(n)`.
    fn rebuild(values: impl Iterator<Item = u32>) -> Self {
        let mut tree = vec![0u32];
        tree.extend(values);
        let n = tree.len() - 1;
        for i in 1..=n {
            let t = tree.get(i).copied().unwrap_or(0);
            let j = i + lowbit(i);
            if j <= n
                && let Some(up) = tree.get_mut(j)
            {
                *up = up.wrapping_add(t);
            }
        }
        let mut counts = Self { tree, total: 0 };
        counts.total = counts.prefix(n);
        counts
    }
}

/// The targets of one row state, oldest arrival first.
///
/// A target that leaves its state leaves a vacant slot behind; once vacant
/// slots outnumber live ones the bucket compacts, and the caller re-reads
/// [`Bucket::slots`] to update the slots its records keep.
#[derive(Debug, Default)]
pub struct Bucket {
    slots: Vec<Id>,
    all: Counts,
    matched: Counts,
}

impl Bucket {
    /// Add `id` at the end; returns its slot.
    pub fn push(&mut self, id: Id, matched: bool) -> u32 {
        let slot = u32::try_from(self.slots.len()).unwrap_or(u32::MAX);
        self.slots.push(id);
        self.all.push(1);
        self.matched.push(u32::from(matched));
        slot
    }

    /// Take the target at `slot` out.
    pub fn remove(&mut self, slot: u32, matched: bool) {
        let i = slot as usize;
        let Some(s) = self.slots.get_mut(i) else {
            return;
        };
        if *s == VACANT {
            return;
        }
        *s = VACANT;
        self.all.bump(i, false);
        if matched {
            self.matched.bump(i, false);
        }
    }

    /// The target at `slot` became matched.
    pub fn mark_matched(&mut self, slot: u32) {
        let i = slot as usize;
        if self.slots.get(i).is_some_and(|&s| s != VACANT) {
            self.matched.bump(i, true);
        }
    }

    pub fn len(&self, matched_only: bool) -> usize {
        self.counts(matched_only).total as usize
    }

    /// The `k`-th target in arrival order (`rev`: newest first).
    pub fn nth(&self, matched_only: bool, k: usize, rev: bool) -> Option<Id> {
        let len = self.len(matched_only);
        let k = if rev { len.checked_sub(k + 1)? } else { k };
        let slot = self.counts(matched_only).select(k)?;
        self.slots.get(slot).copied()
    }

    /// How many targets come before `slot` (`rev`: after it).
    pub fn rank(&self, matched_only: bool, slot: u32, rev: bool) -> usize {
        let counts = self.counts(matched_only);
        let before = counts.prefix(slot as usize) as usize;
        if rev {
            // The entries after it: all, less those before, less itself.
            (counts.total as usize).saturating_sub(before + 1)
        } else {
            before
        }
    }

    /// The live targets in arrival order, every scope.
    pub fn ids(&self) -> impl DoubleEndedIterator<Item = Id> + '_ {
        self.slots.iter().copied().filter(|&id| id != VACANT)
    }

    /// Every slot; a vacant one holds [`Id::MAX`].
    pub fn slots(&self) -> &[Id] {
        &self.slots
    }

    /// Drop the vacant slots once they outnumber the live ones. `true` when
    /// it did: every live target's slot moved.
    pub fn compact_if_sparse(&mut self, is_matched: impl Fn(Id) -> bool) -> bool {
        let live = self.all.total as usize;
        if self.slots.len() < 64 || live * 2 >= self.slots.len() {
            return false;
        }
        self.slots.retain(|&id| id != VACANT);
        self.slots.shrink_to(self.slots.len() * 2);
        self.all = Counts::rebuild(self.slots.iter().map(|_| 1));
        self.matched = Counts::rebuild(self.slots.iter().map(|&id| u32::from(is_matched(id))));
        true
    }

    fn counts(&self, matched_only: bool) -> &Counts {
        if matched_only {
            &self.matched
        } else {
            &self.all
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nth_and_rank_follow_arrivals_and_removals() {
        let mut b = Bucket::default();
        let slots: Vec<u32> = (0..200).map(|id| b.push(id, id % 3 == 0)).collect();
        for &s in slots.iter().filter(|&&s| s % 2 == 1) {
            b.remove(s, s % 3 == 0);
        }
        let live: Vec<Id> = (0..200).filter(|id| id % 2 == 0).collect();
        let live_m: Vec<Id> = live.iter().copied().filter(|id| id % 3 == 0).collect();
        assert_eq!(b.len(false), live.len());
        assert_eq!(b.len(true), live_m.len());
        for (k, &id) in live.iter().enumerate() {
            assert_eq!(b.nth(false, k, false), Some(id));
            assert_eq!(b.rank(false, id, false), k);
            assert_eq!(b.nth(false, k, true), live.iter().rev().nth(k).copied());
        }
        for (k, &id) in live_m.iter().enumerate() {
            assert_eq!(b.nth(true, k, false), Some(id));
        }
        assert_eq!(b.nth(false, live.len(), false), None);
        // Half the slots are vacant: not yet more than half.
        b.remove(0, true);
        assert!(b.compact_if_sparse(|id| id % 3 == 0));
        assert_eq!(b.len(false), live.len() - 1);
        assert_eq!(b.len(true), live_m.len() - 1);
        assert_eq!(b.nth(false, 0, false), Some(2));
        assert_eq!(b.nth(true, 0, false), Some(6));
        assert_eq!(b.ids().count(), live.len() - 1);
    }

    #[test]
    fn marking_matched_later_counts_in_the_matched_scope() {
        let mut b = Bucket::default();
        let s = b.push(7, false);
        assert_eq!(b.len(true), 0);
        b.mark_matched(s);
        assert_eq!(b.len(true), 1);
        assert_eq!(b.nth(true, 0, false), Some(7));
    }
}
