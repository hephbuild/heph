//! The in-process view of the index (spec: Mechanism, Snapshot; Decision 22).
//!
//! A [`Snapshot`] is two `Arc`s, an immutable [`Base`] (the rows as of one
//! reload, with a fold multimap and an addr map) and a small [`Overlay`] of
//! rows this process published since, each tagged with its commit's `gen` and
//! a process-local publish sequence, deletions as tombstones. A lookup takes
//! the higher `gen` of the two, the overlay winning a tie.
//!
//! [`State`] is the current pair plus `loaded_through` and the generations
//! this process published above it. The index keeps it behind an `RwLock`
//! held only to clone or swap; the rules for changing it live here, so they
//! can be tested without a db:
//!
//! - **publish** writes its own entries into a persistent copy of the
//!   overlay (never a deep clone, never the base), takes the next sequence,
//!   and advances `loaded_through` contiguously;
//! - **reload install** replaces the base, keeps every overlay entry whose
//!   `gen` is above the reload's generation, then advances contiguously;
//! - **fold install** replaces the base with base + the overlay as copied at
//!   sequence `S`, and keeps every overlay entry published after the copy
//!   (sequence ≥ `S`), by sequence, not by `gen`, because publishes can
//!   arrive out of `gen` order.

use super::fs::IndexFs;
use super::section::Section;
use super::store::Row;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

/// The rows as of one reload (or a fold of one).
#[derive(Debug, Default, Clone)]
pub struct Base {
    rows: FxHashMap<Vec<u8>, Row>,
    /// `fold → [path]`. May list a path whose row has gone (a fold applies a
    /// tombstone without pruning it): readers resolve each path through
    /// [`Snapshot::row`].
    fold: FxHashMap<u64, Vec<Vec<u8>>>,
    /// `addr → [path]` of `rows`, built on the first call that asks: only
    /// the commands (`keep`, `discard`, `why`) do, never a glob, which paid
    /// for it on every reload. A fold starts it empty again.
    by_addr: OnceLock<FxHashMap<String, Vec<Vec<u8>>>>,
    /// Paths a fold deleted since the reload, with the deleting `gen`: a
    /// late publish below it must not bring the row back (see
    /// [`State::publish`]). Disjoint from `rows`; a reload starts it empty.
    tombs: FxHashMap<Vec<u8>, u64>,
    /// The generation the rows were loaded at.
    generation: u64,
}

impl Base {
    /// The base a reload at `generation` builds from the rows it read.
    pub fn from_rows(generation: u64, rows: Vec<Row>) -> Self {
        let mut base = Self {
            rows: FxHashMap::with_capacity_and_hasher(rows.len(), Default::default()),
            generation,
            ..Self::default()
        };
        for row in rows {
            base.index(&row);
            base.rows.insert(row.path.clone(), row);
        }
        base
    }

    fn index(&mut self, row: &Row) {
        self.fold
            .entry(row.fold)
            .or_default()
            .push(row.path.clone());
    }

    fn by_addr(&self) -> &FxHashMap<String, Vec<Vec<u8>>> {
        self.by_addr.get_or_init(|| {
            let mut m: FxHashMap<String, Vec<Vec<u8>>> = FxHashMap::default();
            for (path, row) in &self.rows {
                m.entry(row.addr.clone()).or_default().push(path.clone());
            }
            m
        })
    }

    /// The `gen` an overlay entry at `path` must reach to win: the base row's,
    /// or a folded deletion's.
    fn floor(&self, path: &[u8]) -> Option<u64> {
        match self.rows.get(path) {
            Some(r) => Some(r.generation),
            None => self.tombs.get(path).copied(),
        }
    }

    /// This base with `overlay` applied, entry by entry under the merge rule.
    fn folded(&self, overlay: &Overlay) -> Self {
        let mut base = self.clone();
        for (path, e) in &overlay.entries {
            let wins = base.floor(path).is_none_or(|g| e.generation >= g);
            if !wins {
                continue;
            }
            let old = base.rows.remove(path);
            let Some(row) = &e.row else {
                base.tombs.insert(path.clone(), e.generation);
                continue;
            };
            // Index a path only where it is not listed already (a folded
            // deletion leaves its path listed under its fold key).
            let tomb = base.tombs.remove(path.as_slice());
            if old.is_none() && tomb.is_none() {
                base.fold.entry(row.fold).or_default().push(path.clone());
            }
            base.rows.insert(path.clone(), row.clone());
        }
        base.by_addr = OnceLock::new();
        base
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// One published change: the row, or `None` for a deletion.
#[derive(Debug, Clone)]
struct OverlayEntry {
    row: Option<Row>,
    generation: u64,
    seq: u64,
}

/// A persistent hash map: a clone is two pointer copies, an insert into a
/// clone path-copies the trie nodes on the way to its key and shares the rest.
type PMap<K, V> = rpds::HashTrieMapSync<K, V, rustc_hash::FxBuildHasher>;

/// The rows this process published since its base.
///
/// Both maps are persistent, so a publish costs the entries it writes and
/// never the overlay it extends: a snapshot reading the previous overlay
/// keeps its own root, and the new one shares every untouched node with it.
/// A `FxHashMap` here was deep-cloned on every publish, which made a run of
/// write-backs quadratic until the fold.
#[derive(Default, Clone)]
pub struct Overlay {
    entries: PMap<Vec<u8>, OverlayEntry>,
    /// `fold → [path]` of the overlay's rows; stale like [`Base::fold`]. A
    /// bucket is the paths sharing one fold key, so copying it to push one is
    /// a copy of a path or two, not of the overlay.
    fold: PMap<u64, Vec<Vec<u8>>>,
}

// By hand: `FxBuildHasher` has no `Debug`.
impl std::fmt::Debug for Overlay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map()
            .entries(
                self.entries
                    .iter()
                    .map(|(p, e)| (String::from_utf8_lossy(p), e)),
            )
            .finish()
    }
}

impl Overlay {
    pub fn len(&self) -> usize {
        self.entries.size()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Add one change, unless the overlay already holds a later `gen` for
    /// the path (publishes can arrive out of `gen` order). Whether it did.
    fn put(&mut self, path: &[u8], row: Option<&Row>, generation: u64, seq: u64) -> bool {
        let old = self.entries.get(path);
        if old.is_some_and(|e| e.generation > generation) {
            return false;
        }
        if let Some(row) = row
            && old.is_none()
        {
            self.index_fold(row.fold, path);
        }
        self.entries.insert_mut(
            path.to_vec(),
            OverlayEntry {
                row: row.cloned(),
                generation,
                seq,
            },
        );
        true
    }

    fn index_fold(&mut self, fold: u64, path: &[u8]) {
        let mut paths = self.fold.get(&fold).cloned().unwrap_or_default();
        paths.push(path.to_vec());
        self.fold.insert_mut(fold, paths);
    }

    /// The entries `keep` selects, with their fold index.
    fn retain(&self, keep: impl Fn(&OverlayEntry) -> bool) -> Self {
        let mut out = Self::default();
        for (path, e) in self.entries.iter().filter(|(_, e)| keep(e)) {
            if let Some(row) = &e.row {
                out.index_fold(row.fold, path);
            }
            out.entries.insert_mut(path.clone(), e.clone());
        }
        out
    }
}

/// The current pair and its generation accounting.
#[derive(Debug, Default)]
pub struct State {
    pub(super) base: Arc<Base>,
    pub(super) overlay: Arc<Overlay>,
    /// Every committed row with `gen` at or below this is in base + overlay.
    pub(super) loaded_through: u64,
    /// Generations this process published above `loaded_through`.
    published_above: BTreeSet<u64>,
    next_seq: u64,
}

impl State {
    /// While the generation right after `loaded_through` is one this process
    /// published, move onto it. A generation it did not publish is a foreign
    /// commit: `loaded_through` stops below it until a reload.
    fn advance(&mut self) {
        while self.published_above.remove(&(self.loaded_through + 1)) {
            self.loaded_through += 1;
        }
        let lt = self.loaded_through;
        self.published_above.retain(|g| *g > lt);
    }

    /// Publish one commit's rows and deletions. Copies only the entries it
    /// writes, never the rest of the overlay nor the base (the overlay's
    /// maps share every other node with the one snapshots still hold);
    /// returns how many entries it wrote.
    ///
    /// **Why a deletion's `gen` can be dropped.** A fold applies a tombstone
    /// by removing the base row, and a reload never sees the row at all; the
    /// overlay entry that carried the deletion's `gen` is gone either way. A
    /// publish that lands later with a lower `gen` for the same path would
    /// then find nothing to lose to and bring the row back. What keeps that
    /// from happening:
    ///
    /// - every delete names its addr (`DELETE … WHERE path AND addr`) and is
    ///   committed and published under that addr's codegen lock, so no
    ///   publish of *that* addr's rows can still be in flight behind it;
    /// - another addr's row at the path can be: B commits `p` at `g`, A
    ///   takes `p` over at `g' > g` and deletes it at `G`, and B publishes
    ///   `g` only now. For that, a fold keeps each deletion as a `gen` in
    ///   [`Base`]'s tombs, which a lower overlay `gen` loses to;
    /// - and a publish at or below the base's reload generation is skipped
    ///   here: the rows were read at that generation, so they already hold
    ///   that commit or something newer (a deletion included).
    pub fn publish(&mut self, generation: u64, rows: &[Row], tombstones: &[Vec<u8>]) -> usize {
        if generation <= self.base.generation {
            // Already in the base; `loaded_through` is at or above it.
            return 0;
        }
        let mut overlay = Overlay::clone(&self.overlay);
        let mut written = 0;
        let seq = self.next_seq;
        self.next_seq += 1;
        let upserted: FxHashSet<&[u8]> = rows.iter().map(|r| r.path.as_slice()).collect();
        for path in tombstones {
            if !upserted.contains(path.as_slice()) {
                written += usize::from(overlay.put(path, None, generation, seq));
            }
        }
        for row in rows {
            written += usize::from(overlay.put(&row.path, Some(row), generation, seq));
        }
        self.overlay = Arc::new(overlay);
        if generation > self.loaded_through {
            self.published_above.insert(generation);
        }
        self.advance();
        written
    }

    /// Install a reload's base: keep every overlay entry above its generation
    /// (a publish that landed during the reload), then advance.
    pub fn install_reload(&mut self, base: Base) {
        let r = base.generation;
        self.overlay = Arc::new(self.overlay.retain(|e| e.generation > r));
        self.base = Arc::new(base);
        // Above `r`, only this process's own publishes can have moved it, and
        // those are all in the overlay just kept.
        self.loaded_through = self.loaded_through.max(r);
        self.advance();
    }

    /// What a fold builds from: the pair and the sequence it was copied at.
    pub fn fold_copy(&self) -> (Arc<Base>, Arc<Overlay>, u64) {
        (
            Arc::clone(&self.base),
            Arc::clone(&self.overlay),
            self.next_seq,
        )
    }

    /// Build the folded base off any lock; see [`Self::install_fold`].
    pub fn build_fold(base: &Base, overlay: &Overlay) -> Base {
        base.folded(overlay)
    }

    /// Install a fold copied at sequence `s`: every entry published since the
    /// copy stays in the overlay. `loaded_through` and the published set are
    /// unchanged.
    pub fn install_fold(&mut self, base: Base, s: u64) {
        self.overlay = Arc::new(self.overlay.retain(|e| e.seq >= s));
        self.base = Arc::new(base);
    }

    pub fn snapshot(&self, section: Arc<Section>, env: Arc<LookupEnv>) -> Snapshot {
        Snapshot {
            base: Arc::clone(&self.base),
            overlay: Arc::clone(&self.overlay),
            loaded_through: self.loaded_through,
            section,
            env,
        }
    }

    pub fn loaded_through(&self) -> u64 {
        self.loaded_through
    }

    pub fn overlay_len(&self) -> usize {
        self.overlay.len()
    }
}

/// The rows and section one glob, `file()` lookup, guard pass or command
/// reads (J10). Holds `Arc`s, never a lock.
#[derive(Debug, Clone)]
pub struct Snapshot {
    base: Arc<Base>,
    overlay: Arc<Overlay>,
    loaded_through: u64,
    section: Arc<Section>,
    env: Arc<LookupEnv>,
}

/// What a lookup needs besides the rows: the root its paths are relative to,
/// and the filesystem it stats and hashes through. One per index.
pub struct LookupEnv {
    pub(crate) root: PathBuf,
    pub(crate) fs: Arc<dyn IndexFs>,
}

impl LookupEnv {
    pub fn new(root: PathBuf, fs: Arc<dyn IndexFs>) -> Self {
        Self { root, fs }
    }
}

impl std::fmt::Debug for LookupEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LookupEnv")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl Snapshot {
    /// The root and filesystem lookups go through.
    pub(crate) fn env(&self) -> &LookupEnv {
        &self.env
    }

    /// The row at `path`: the higher `gen` of base and overlay, the overlay
    /// winning a tie; a winning tombstone is no row, and neither is an
    /// overlay row below a folded deletion.
    pub fn row(&self, path: &[u8]) -> Option<&Row> {
        match self.overlay.entries.get(path) {
            Some(o) if self.base.floor(path).is_none_or(|g| o.generation >= g) => o.row.as_ref(),
            _ => self.base.rows.get(path),
        }
    }

    /// Whether any row has a fold key at all: a fold probe is skipped when not.
    pub fn has_fold_entries(&self) -> bool {
        !self.base.fold.is_empty() || !self.overlay.fold.is_empty()
    }

    /// The rows whose path folds to `fold`: other casings or normalisations
    /// of a path. A caller still checks `(dev, ino)` (spec: Lookup step 0).
    pub fn fold_candidates(&self, fold: u64) -> Vec<&Row> {
        let paths = self
            .base
            .fold
            .get(&fold)
            .into_iter()
            .chain(self.overlay.fold.get(&fold))
            .flatten();
        self.resolve(paths, |r| r.fold == fold)
    }

    /// One target's rows.
    pub fn rows_for_addr(&self, addr: &str) -> Vec<&Row> {
        let overlay = self
            .overlay
            .entries
            .iter()
            .filter(|(_, e)| e.row.as_ref().is_some_and(|r| r.addr == addr))
            .map(|(p, _)| p);
        let paths = self
            .base
            .by_addr()
            .get(addr)
            .into_iter()
            .flatten()
            .chain(overlay);
        self.resolve(paths, |r| r.addr == addr)
    }

    /// Each distinct path's merged row, kept when `keep` holds.
    fn resolve<'a>(
        &'a self,
        paths: impl Iterator<Item = &'a Vec<u8>>,
        keep: impl Fn(&Row) -> bool,
    ) -> Vec<&'a Row> {
        let mut seen = FxHashSet::default();
        paths
            .filter(|p| seen.insert(p.as_slice()))
            .filter_map(|p| self.row(p))
            .filter(|r| keep(r))
            .collect()
    }

    /// Every row, merged.
    pub fn rows(&self) -> impl Iterator<Item = &Row> {
        let base = self
            .base
            .rows
            .iter()
            .filter(|(p, b)| {
                self.overlay
                    .entries
                    .get(*p)
                    .is_none_or(|o| o.generation < b.generation)
            })
            .map(|(_, b)| b);
        let overlay = self
            .overlay
            .entries
            .iter()
            .filter(|(p, o)| self.base.floor(p).is_none_or(|g| o.generation >= g))
            .filter_map(|(_, o)| o.row.as_ref());
        base.chain(overlay)
    }

    /// No row at all.
    pub fn is_empty(&self) -> bool {
        self.rows().next().is_none()
    }

    /// The managed section as of this snapshot.
    pub fn section(&self) -> &Section {
        &self.section
    }

    /// Every committed row with `gen` at or below this is in the snapshot.
    pub fn loaded_through(&self) -> u64 {
        self.loaded_through
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen_index::store::RowKind;

    fn row(path: &str, addr: &str, generation: u64) -> Row {
        let mut r = Row::new(path.as_bytes().to_vec(), addr, RowKind::File);
        r.generation = generation;
        r
    }

    fn snap(st: &State) -> Snapshot {
        let env = LookupEnv::new(
            PathBuf::from("/nonexistent"),
            Arc::new(crate::codegen_index::testfs::TestFs::default()),
        );
        st.snapshot(Arc::new(Section::empty()), Arc::new(env))
    }

    fn paths(rows: Vec<&Row>) -> Vec<String> {
        let mut v: Vec<_> = rows
            .into_iter()
            .map(|r| String::from_utf8(r.path.clone()).unwrap())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn merged_lookup_takes_the_higher_gen_overlay_winning_ties() {
        let mut st = State::default();
        st.install_reload(Base::from_rows(
            5,
            vec![
                row("a", "//x:a", 5),
                row("b", "//x:a", 5),
                row("c", "//x:a", 3),
            ],
        ));
        // A stale publish (gen 4 < base's 5) does not hide the base row.
        st.publish(4, &[row("a", "//x:old", 4)], &[]);
        st.publish(6, &[row("b", "//x:b", 6)], &[]);
        st.publish(7, &[], &[b"c".to_vec()]);
        let s = snap(&st);
        assert_eq!(s.row(b"a").unwrap().addr, "//x:a");
        assert_eq!(s.row(b"b").unwrap().addr, "//x:b");
        assert!(s.row(b"c").is_none(), "tombstone");
        assert_eq!(paths(s.rows_for_addr("//x:a")), ["a"]);
        assert_eq!(paths(s.rows().collect()), ["a", "b"]);
        assert_eq!(s.loaded_through(), 7);
    }

    #[test]
    fn merged_lookup_overlay_wins_a_tie_with_a_folded_row() {
        let mut st = State::default();
        st.install_reload(Base::from_rows(0, vec![]));
        st.publish(1, &[row("a", "//x:a", 1)], &[]);
        let (b, o, s) = st.fold_copy();
        let folded = State::build_fold(&b, &o);
        // Republished at the same gen after the copy (not a real sequence,
        // the merge rule's tie-break).
        st.publish(1, &[row("a", "//x:b", 1)], &[]);
        st.install_fold(folded, s);
        assert_eq!(snap(&st).row(b"a").unwrap().addr, "//x:b");
    }

    #[test]
    fn out_of_order_publishes_reach_the_last_contiguous_generation() {
        let mut st = State::default();
        st.install_reload(Base::from_rows(8, vec![]));
        st.publish(10, &[row("a", "//a:a", 10)], &[]);
        assert_eq!(st.loaded_through(), 8, "9 is not ours yet");
        st.publish(9, &[row("a", "//a:a", 9)], &[]);
        assert_eq!(st.loaded_through(), 10);
        assert_eq!(
            snap(&st).row(b"a").unwrap().generation,
            10,
            "the later generation stays"
        );
        // A gap (foreign 11) stops the advance.
        st.publish(12, &[], &[]);
        assert_eq!(st.loaded_through(), 10);
    }

    #[test]
    fn fold_keeps_entries_published_after_the_copy() {
        let mut st = State::default();
        st.install_reload(Base::from_rows(0, vec![]));
        st.publish(2, &[row("b", "//b:b", 2)], &[]);
        let (base, overlay, s) = st.fold_copy();
        let folded = State::build_fold(&base, &overlay);
        // Lands between the copy and the install, out of gen order.
        st.publish(1, &[row("a", "//a:a", 1)], &[]);
        st.install_fold(folded, s);
        assert_eq!(st.overlay_len(), 1, "only the post-copy publish");
        let sn = snap(&st);
        assert_eq!(paths(sn.rows().collect()), ["a", "b"]);
        assert_eq!(sn.loaded_through(), 2);
    }

    /// The by-target map is built on first use; a fold that moves a path to
    /// another target, after a lookup filled the map, is seen by the next.
    #[test]
    fn rows_for_addr_after_a_fold_sees_the_folded_rows() {
        let mut st = State::default();
        st.install_reload(Base::from_rows(
            1,
            vec![row("p", "//x:a", 1), row("q", "//x:a", 1)],
        ));
        assert_eq!(paths(snap(&st).rows_for_addr("//x:a")), ["p", "q"]);
        st.publish(
            2,
            &[row("p", "//x:b", 2), row("r", "//x:b", 2)],
            &[b"q".to_vec()],
        );
        let (b, o, s) = st.fold_copy();
        let folded = State::build_fold(&b, &o);
        st.install_fold(folded, s);
        assert!(snap(&st).overlay.is_empty(), "the fold took the overlay");
        assert!(snap(&st).rows_for_addr("//x:a").is_empty());
        assert_eq!(paths(snap(&st).rows_for_addr("//x:b")), ["p", "r"]);
    }

    /// B commits `p` at 1 and is slow to publish; A takes `p` over at 2 and
    /// deletes it at 3 (both under A's lock, both published); the overlay
    /// folds. B's publish of 1 then lands: the row stays deleted.
    #[test]
    fn late_publish_below_a_folded_tombstone_stays_deleted() {
        let mut st = State::default();
        st.install_reload(Base::from_rows(0, vec![]));
        st.publish(2, &[row("p", "//x:a", 2)], &[]);
        st.publish(3, &[], &[b"p".to_vec()]);
        let (b, o, s) = st.fold_copy();
        let folded = State::build_fold(&b, &o);
        st.install_fold(folded, s);
        st.publish(1, &[row("p", "//x:b", 1)], &[]);
        assert!(snap(&st).row(b"p").is_none(), "B's stale row came back");
        assert!(snap(&st).rows().next().is_none());
        assert!(snap(&st).rows_for_addr("//x:b").is_empty());
        // A later commit at the path is a row again, through a fold too.
        st.publish(4, &[row("p", "//x:c", 4)], &[]);
        assert_eq!(snap(&st).row(b"p").unwrap().addr, "//x:c");
        let (b, o, s) = st.fold_copy();
        let folded = State::build_fold(&b, &o);
        st.install_fold(folded, s);
        assert_eq!(snap(&st).row(b"p").unwrap().addr, "//x:c");
        assert_eq!(st.loaded_through(), 4);
    }

    /// The same, across a reload: the db read at 3 has no `p`, and B's
    /// publish of 1 is older than everything the base reflects.
    #[test]
    fn late_publish_at_or_below_the_reload_is_already_in_the_base() {
        let mut st = State::default();
        st.install_reload(Base::from_rows(3, vec![row("q", "//x:a", 3)]));
        st.publish(1, &[row("p", "//x:b", 1)], &[]);
        st.publish(2, &[], &[b"q".to_vec()]);
        let s = snap(&st);
        assert!(s.row(b"p").is_none(), "B's stale row came back");
        assert_eq!(s.row(b"q").unwrap().addr, "//x:a");
        assert_eq!(st.overlay_len(), 0);
    }

    #[test]
    fn fold_candidates_resolve_through_the_merge() {
        let mut st = State::default();
        st.install_reload(Base::from_rows(1, vec![row("p/A.go", "//p:a", 1)]));
        st.publish(2, &[row("p/a.go", "//p:a", 2)], &[b"p/A.go".to_vec()]);
        let fold = crate::codegen_index::fold_hash(b"p/a.go");
        assert!(snap(&st).has_fold_entries());
        assert_eq!(paths(snap(&st).fold_candidates(fold)), ["p/a.go"]);
        let (b, o, s) = st.fold_copy();
        let folded = State::build_fold(&b, &o);
        st.install_fold(folded, s);
        assert_eq!(paths(snap(&st).fold_candidates(fold)), ["p/a.go"]);
    }
}
