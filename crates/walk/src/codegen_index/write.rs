//! The write protocol of one `copy` target's write-back (spec: Mechanism,
//! Write protocol; J2, J5, J9; Decisions 9, 10, 11, 19), shaped so the engine
//! wraps its unchanged byte-moving code:
//!
//! 1. [`Snapshot::needs_write_back`], no lock: `None` when every landing
//!    entry is already recorded, verified, equal and flagged, and no
//!    `written`, `imported` or pending row of the target lies outside the
//!    write-back. **A warm run writes nothing**: no lock, no commit, no
//!    generation move, even once a `dropped` row exists.
//! 2. [`CodegenIndex::begin_write_back`], under the target's codegen lock:
//!    re-read the target's rows from the db, lstat again, redo the selection
//!    reusing step 1's hashes, then one commit setting `pending_hash` on every
//!    entry to write, published before any byte moves (J2). A failed commit
//!    aborts before any byte moves.
//! 3. The engine writes [`WriteBack::writes`].
//! 4. [`WriteBack::settle`]: all file IO first (lstat what was written; fresh
//!    hash, then unlink, each leftover heph wrote that a current `copy`
//!    output still covers), then one transaction that only writes rows.
//!    Dropping the [`WriteBack`] releases the lock; dropping it before
//!    `settle` is a crash, which the next write-back (or a command's
//!    [`CodegenIndex::settle_pending`]) settles.

use super::TargetLockGuard;
use super::fs::{FileStat, IndexFs, StatKind, lstat_opt};
use super::index::CodegenIndex;
use super::lookup::{read_link_opt, verify};
use super::paths::rel_path;
use super::section::Section;
use super::snapshot::Snapshot;
use super::store::{Record, Row, RowKind, RowState, Store};
use anyhow::{Context as _, Result, bail, ensure};
use rustc_hash::{FxHashMap, FxHashSet};
use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;

/// One artifact entry a `copy` write-back puts in the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landing {
    /// Root-relative, normalised (`normalize_rel`).
    pub path: Vec<u8>,
    /// The declared output that produces it.
    pub output: String,
    pub content: LandingContent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LandingContent {
    /// A regular file: `bytes_hashout` of the bytes about to land, computed
    /// for every entry, including those the byte-mover skips as unchanged.
    File { hash: String, x: bool },
    /// A symlink to `target`.
    Symlink { target: Vec<u8> },
}

impl Landing {
    pub fn kind(&self) -> RowKind {
        match self.content {
            LandingContent::File { .. } => RowKind::File,
            LandingContent::Symlink { .. } => RowKind::Symlink,
        }
    }

    /// What the row records for it: a file's content hash; a symlink's target
    /// hashed as bytes.
    pub fn hash(&self) -> Cow<'_, str> {
        match &self.content {
            LandingContent::File { hash, .. } => Cow::Borrowed(hash),
            LandingContent::Symlink { target } => Cow::Owned(symlink_hash(target)),
        }
    }
}

/// The hash a symlink row records: its target, hashed as bytes.
pub fn symlink_hash(target: &[u8]) -> String {
    crate::bytes_hashout(target, false)
}

/// Step 1's verdict that a write-back has work: what step 2 redoes under the
/// lock.
#[derive(Debug, Clone)]
pub struct Plan {
    addr: String,
    landings: Vec<Landing>,
}

impl Plan {
    pub fn addr(&self) -> &str {
        &self.addr
    }

    pub fn landings(&self) -> &[Landing] {
        &self.landings
    }
}

/// What step 2 decided for one landing entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryAction {
    /// Recorded, verified, equal and flagged: nothing.
    Skip,
    /// Bytes and row: marked pending at step 2, recorded at step 4.
    Write,
    /// Only `seen_in_section` turns true, at step 4.
    FlagOnly,
}

#[derive(Debug)]
struct Selected {
    action: EntryAction,
    /// A section line matches the path now.
    matched: bool,
    /// What was on disk differed from the row's record: a hand edit the
    /// write overwrites.
    edited: bool,
}

/// Step 1's selection for one entry against `row` (the row at its path,
/// whoever owns it). Hashes the bytes on disk only through the cached hash,
/// and only when the stat cannot prove the record.
fn select(
    fs: &dyn IndexFs,
    root: &Path,
    section: &Section,
    addr: &str,
    landing: &Landing,
    row: Option<&Row>,
) -> Result<Selected> {
    let matched = section.match_point(&landing.path, false).is_territory();
    let write = |edited| Selected {
        action: EntryAction::Write,
        matched,
        edited,
    };
    let Some(row) = row else {
        return Ok(write(false));
    };
    if row.addr != addr
        || row.pending_hash.is_some()
        || row.kind != landing.kind()
        || row.state != RowState::Written
        || row.output.as_deref() != Some(landing.output.as_str())
    {
        return Ok(write(false));
    }
    let abs = root.join(rel_path(&landing.path));
    let st = lstat_opt(fs, &abs)?;
    if !verify(fs, row, st.as_ref(), &abs)? {
        return Ok(write(st.is_some()));
    }
    let equal = match &landing.content {
        LandingContent::File { hash, .. } => row.record.as_ref().is_some_and(|r| &r.hash == hash),
        LandingContent::Symlink { target } => row.link_target.as_deref() == Some(target.as_slice()),
    };
    if !equal {
        return Ok(write(false));
    }
    Ok(Selected {
        action: if matched && !row.seen_in_section {
            EntryAction::FlagOnly
        } else {
            EntryAction::Skip
        },
        matched,
        edited: false,
    })
}

/// A row of the target that no landing entry produces and that still needs
/// work: `written`, `imported` or pending. `dropped` rows never do.
fn is_leftover(row: &Row, produced: &FxHashSet<&[u8]>) -> bool {
    row.state != RowState::Dropped && !produced.contains(row.path.as_slice())
}

impl Snapshot {
    /// Write protocol step 1, no lock: `None` when the write-back has nothing
    /// to do, so the caller takes no lock and commits nothing.
    pub fn needs_write_back(&self, addr: &str, landings: &[Landing]) -> Result<Option<Plan>> {
        let env = self.env();
        let produced: FxHashSet<&[u8]> = landings.iter().map(|l| l.path.as_slice()).collect();
        let mut needs = self
            .rows_for_addr(addr)
            .into_iter()
            .any(|r| is_leftover(r, &produced));
        for landing in landings {
            if needs {
                break;
            }
            let row = self.row(&landing.path);
            let s = select(
                env.fs.as_ref(),
                &env.root,
                self.section(),
                addr,
                landing,
                row,
            )?;
            needs = s.action != EntryAction::Skip;
        }
        Ok(needs.then(|| Plan {
            addr: addr.to_owned(),
            landings: landings.to_vec(),
        }))
    }
}

/// Injectable crash points of a write-back (tests only inject).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fault {
    /// Inside step 2's transaction, before its commit.
    PendingCommit,
    /// After step 4's unlinks, before its transaction.
    AfterUnlink,
    /// Inside step 4's transaction, before its commit.
    SettleCommit,
}

#[derive(Debug)]
struct Entry {
    landing: Landing,
    sel: Selected,
}

/// A write-back between steps 2 and 4: it holds the target's codegen lock
/// until dropped.
#[derive(Debug)]
pub struct WriteBack {
    index: Arc<CodegenIndex>,
    _guard: TargetLockGuard,
    addr: String,
    entries: Vec<Entry>,
    /// The target's rows no entry produces, as read under the lock.
    leftovers: Vec<Row>,
    /// The generation of step 2's commit, if it made one.
    pending_generation: Option<u64>,
    mark_hint: bool,
}

/// What step 4 did, for the engine's messages.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SettleReport {
    /// Step 4's commit, if it made one.
    pub generation: Option<u64>,
    /// Paths written over bytes that differed from their record: hand edits.
    pub overwritten_edits: Vec<Vec<u8>>,
    /// Leftovers heph wrote, unlinked, and their rows deleted.
    pub deleted: Vec<Vec<u8>>,
    /// Leftovers whose rows are now `dropped`, files kept.
    pub dropped: Vec<Vec<u8>>,
}

/// How a command's settling of one pending row ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingOutcome {
    /// The file holds the pending bytes: they are the record now.
    Recorded,
    /// The file holds the recorded bytes: the write never landed.
    Cleared,
    /// Neither (partial bytes, or no file): an interrupted write-back; the
    /// row stays pending until the target is rebuilt.
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSettled {
    pub path: Vec<u8>,
    pub outcome: PendingOutcome,
}

/// The current bytes at `abs` hashed the way the row records them, fresh and
/// uncached: a file's content hash, a symlink's target. `None` when nothing
/// of the row's kind is there.
fn fresh_row_hash(
    fs: &dyn IndexFs,
    abs: &Path,
    kind: RowKind,
    st: Option<&FileStat>,
) -> Result<Option<(String, Option<Vec<u8>>)>> {
    let Some(st) = st else { return Ok(None) };
    Ok(match (kind, st.kind) {
        (RowKind::File, StatKind::File) => Some((fs.fresh_hash(abs, st.exec())?, None)),
        (RowKind::Symlink, StatKind::Symlink) => {
            read_link_opt(fs, abs)?.map(|t| (symlink_hash(&t), Some(t)))
        }
        _ => None,
    })
}

fn record_of(hash: String, st: &FileStat, written_at_ns: i64) -> Record {
    Record {
        hash,
        size: st.size,
        mtime_ns: st.mtime_ns,
        ino: st.ino,
        dev: st.dev,
        mode: st.mode,
        written_at_ns,
    }
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

impl CodegenIndex {
    fn require_store(&self) -> Result<&Store> {
        self.open_store()?.with_context(|| {
            format!(
                "no codegen index at {}: a write-back's caller creates it \
                 (create_db, under the import lock) before taking the target lock",
                self.db_path().display()
            )
        })
    }

    /// `meta.no_section_hint_shown`; `false` while no db exists.
    pub fn no_section_hint_shown(&self) -> Result<bool> {
        match self.open_store()? {
            Some(s) => s.no_section_hint_shown(),
            None => Ok(false),
        }
    }

    /// Write protocol step 2, under the target's codegen lock (`guard`, for
    /// `plan`'s addr). The db must exist: a caller that found none took the
    /// import lock, ran [`CodegenIndex::create_db`] and released it first.
    pub fn begin_write_back(
        self: &Arc<Self>,
        guard: TargetLockGuard,
        plan: Plan,
    ) -> Result<WriteBack> {
        ensure!(
            guard.addr() == plan.addr,
            "write-back of {} under the codegen lock of {}",
            plan.addr,
            guard.addr()
        );
        let store = self.require_store()?;
        let section = self.section()?;
        let fs = self.fs().as_ref();
        let root = self.root();
        let addr = plan.addr;
        let mut rows: FxHashMap<Vec<u8>, Row> = store
            .rows_for_addr(&addr)?
            .into_iter()
            .map(|r| (r.path.clone(), r))
            .collect();
        let mut entries = Vec::with_capacity(plan.landings.len());
        for landing in plan.landings {
            let row = match rows.remove(&landing.path) {
                Some(r) => Some(r),
                None => store.row(&landing.path)?,
            };
            let sel = select(fs, root, &section, &addr, &landing, row.as_ref())?;
            entries.push(Entry { landing, sel });
        }
        let leftovers: Vec<Row> = rows
            .into_values()
            .filter(|r| r.state != RowState::Dropped)
            .collect();

        let writes: Vec<&Landing> = entries
            .iter()
            .filter(|e| e.sel.action == EntryAction::Write)
            .map(|e| &e.landing)
            .collect();
        let pending_generation = if writes.is_empty() {
            None
        } else {
            let committed = store.write(|txn| {
                for landing in &writes {
                    let mut row = match txn.row(&landing.path)? {
                        Some(r) if r.addr == addr => r,
                        _ => Row::new(landing.path.clone(), addr.as_str(), landing.kind()),
                    };
                    if row.kind != landing.kind() {
                        row.kind = landing.kind();
                        row.record = None;
                        row.link_target = None;
                    }
                    row.output = Some(landing.output.clone());
                    row.pending_hash = Some(landing.hash().into_owned());
                    txn.put(&row)?;
                }
                self.fault(Fault::PendingCommit)
            })?;
            self.publish_committed(&committed);
            Some(committed.generation)
        };
        Ok(WriteBack {
            index: Arc::clone(self),
            _guard: guard,
            addr,
            entries,
            leftovers,
            pending_generation,
            mark_hint: false,
        })
    }

    /// For a command holding the target's lock (`codegen keep`, `discard`,
    /// `status`): settle each of its pending rows from a fresh hash of the
    /// file. The pending bytes become the record; the recorded bytes clear
    /// the pending mark; anything else stays pending (an interrupted
    /// write-back, reported with the target to rebuild).
    pub fn settle_pending(&self, guard: &TargetLockGuard) -> Result<Vec<PendingSettled>> {
        let Some(store) = self.open_store()? else {
            return Ok(Vec::new());
        };
        let fs = self.fs().as_ref();
        let mut out = Vec::new();
        let mut changes = Vec::new();
        for row in store.rows_for_addr(guard.addr())? {
            let Some(pending) = row.pending_hash.as_deref() else {
                continue;
            };
            let abs = self.root().join(rel_path(&row.path));
            let st = lstat_opt(fs, &abs)?;
            let fresh = fresh_row_hash(fs, &abs, row.kind, st.as_ref())?;
            let outcome = match (&fresh, st) {
                (Some((h, _)), Some(_)) if h == pending => PendingOutcome::Recorded,
                (Some((h, _)), _) if row.record.as_ref().is_some_and(|r| &r.hash == h) => {
                    PendingOutcome::Cleared
                }
                _ => PendingOutcome::Interrupted,
            };
            out.push(PendingSettled {
                path: row.path.clone(),
                outcome,
            });
            if let (PendingOutcome::Recorded, Some(st), Some((h, link))) = (outcome, st, fresh) {
                changes.push((row.path, Some((record_of(h, &st, now_ns()), link))));
            } else if outcome == PendingOutcome::Cleared {
                changes.push((row.path, None));
            }
        }
        if changes.is_empty() {
            return Ok(out);
        }
        let addr = guard.addr();
        let committed = store.write(|txn| {
            for (path, recorded) in changes {
                let Some(mut row) = txn.row(&path)?.filter(|r| r.addr == addr) else {
                    continue;
                };
                if let Some((record, link)) = recorded {
                    row.record = Some(record);
                    row.state = RowState::Written;
                    if row.kind == RowKind::Symlink {
                        row.link_target = link;
                    }
                }
                row.pending_hash = None;
                txn.put(&row)?;
            }
            Ok(())
        })?;
        self.publish_committed(&committed);
        Ok(out)
    }
}

enum LeftoverFate {
    /// Unlinked (or already gone): delete the row by `(path, addr)`.
    Delete,
    /// Keep the file; the row becomes `dropped`, its pending mark settled
    /// when the file proves it (`Some(record)`: the pending bytes landed;
    /// `clear`: the recorded bytes are there).
    Drop {
        record: Option<(Record, Option<Vec<u8>>)>,
        clear_pending: bool,
    },
}

impl WriteBack {
    /// The entries the engine must write at step 3, in landing order.
    pub fn writes(&self) -> impl Iterator<Item = &Landing> {
        self.entries
            .iter()
            .filter(|e| e.sel.action == EntryAction::Write)
            .map(|e| &e.landing)
    }

    /// What step 2 decided for each landing entry, in landing order.
    pub fn actions(&self) -> impl Iterator<Item = (&Landing, EntryAction)> {
        self.entries.iter().map(|e| (&e.landing, e.sel.action))
    }

    /// The generation of step 2's commit; `None` when nothing was marked
    /// pending (nothing to write).
    pub fn pending_generation(&self) -> Option<u64> {
        self.pending_generation
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// Record `meta.no_section_hint_shown` in step 4's transaction.
    pub fn mark_no_section_hint_shown(&mut self) {
        self.mark_hint = true;
    }

    /// Write protocol step 4. `inside_current_copy_output` says whether a
    /// root-relative path is still covered by one of the target's current
    /// `copy` outputs: only such a leftover heph wrote is deleted (J5).
    pub fn settle(
        self,
        inside_current_copy_output: &dyn Fn(&[u8]) -> bool,
    ) -> Result<SettleReport> {
        let index = &self.index;
        let fs = index.fs().as_ref();
        let root = index.root();
        let written_at = now_ns();
        let mut report = SettleReport::default();

        // File IO first, outside any transaction (Decision 19).
        let mut recorded: Vec<(&Entry, Record, Option<Vec<u8>>)> = Vec::new();
        for e in self
            .entries
            .iter()
            .filter(|e| e.sel.action == EntryAction::Write)
        {
            let l = &e.landing;
            let abs = root.join(rel_path(&l.path));
            let st = lstat_opt(fs, &abs)?.with_context(|| {
                format!(
                    "settling {}: the file just written is missing",
                    abs.display()
                )
            })?;
            let link = match &l.content {
                LandingContent::File { .. } => {
                    ensure!(
                        st.kind == StatKind::File,
                        "settling {}: not a regular file after the write",
                        abs.display()
                    );
                    None
                }
                LandingContent::Symlink { target } => {
                    let got = read_link_opt(fs, &abs)?;
                    if st.kind != StatKind::Symlink || got.as_deref() != Some(target.as_slice()) {
                        bail!(
                            "settling {}: not the symlink to {} just written",
                            abs.display(),
                            String::from_utf8_lossy(target)
                        );
                    }
                    got
                }
            };
            if e.sel.edited {
                report.overwritten_edits.push(l.path.clone());
            }
            recorded.push((e, record_of(l.hash().into_owned(), &st, written_at), link));
        }

        let mut fates: Vec<(&Row, LeftoverFate)> = Vec::with_capacity(self.leftovers.len());
        for row in &self.leftovers {
            let abs = root.join(rel_path(&row.path));
            let Some(st) = lstat_opt(fs, &abs)? else {
                // Dangling: nothing to unlink, the row goes.
                fates.push((row, LeftoverFate::Delete));
                continue;
            };
            let fresh = fresh_row_hash(fs, &abs, row.kind, Some(&st))?;
            let hash = fresh.as_ref().map(|(h, _)| h.as_str());
            let is_pending = hash.is_some() && hash == row.pending_hash.as_deref();
            let is_record = hash.is_some() && row.record.as_ref().map(|r| r.hash.as_str()) == hash;
            // J5: only a `written` row's file, only on a fresh hash equal to
            // its record (a pending mark the file proves is settled into the
            // record first, under this lock), only inside a current output.
            if row.state == RowState::Written
                && (is_record || is_pending)
                && inside_current_copy_output(&row.path)
            {
                fs.unlink(&abs)
                    .with_context(|| format!("deleting {}", abs.display()))?;
                fates.push((row, LeftoverFate::Delete));
                continue;
            }
            let record = match fresh {
                Some((h, link)) if is_pending => Some((record_of(h, &st, written_at), link)),
                _ => None,
            };
            fates.push((
                row,
                LeftoverFate::Drop {
                    record,
                    clear_pending: is_record,
                },
            ));
        }
        index.fault(Fault::AfterUnlink)?;

        let flags: Vec<&[u8]> = self
            .entries
            .iter()
            .filter(|e| e.sel.action == EntryAction::FlagOnly)
            .map(|e| e.landing.path.as_slice())
            .collect();
        if recorded.is_empty() && fates.is_empty() && flags.is_empty() && !self.mark_hint {
            return Ok(report);
        }

        let store = index.require_store()?;
        let addr = self.addr.as_str();
        let committed = store.write(|txn| {
            for (e, record, link) in recorded {
                let l = &e.landing;
                let mut row = match txn.row(&l.path)? {
                    Some(r) if r.addr == addr => r,
                    _ => Row::new(l.path.clone(), addr, l.kind()),
                };
                row.kind = l.kind();
                row.output = Some(l.output.clone());
                row.record = Some(record);
                row.link_target = link;
                row.pending_hash = None;
                row.state = RowState::Written;
                row.seen_in_section |= e.sel.matched;
                txn.put(&row)?;
            }
            for path in flags {
                if let Some(mut row) = txn.row(path)?.filter(|r| r.addr == addr) {
                    row.seen_in_section = true;
                    txn.put(&row)?;
                }
            }
            for (left, fate) in fates {
                match fate {
                    LeftoverFate::Delete => {
                        txn.delete(&left.path, addr)?;
                        report.deleted.push(left.path.clone());
                    }
                    LeftoverFate::Drop {
                        record,
                        clear_pending,
                    } => {
                        let Some(mut row) = txn.row(&left.path)?.filter(|r| r.addr == addr) else {
                            continue;
                        };
                        row.state = RowState::Dropped;
                        if let Some((record, link)) = record {
                            row.record = Some(record);
                            if row.kind == RowKind::Symlink {
                                row.link_target = link;
                            }
                            row.pending_hash = None;
                        } else if clear_pending {
                            row.pending_hash = None;
                        }
                        txn.put(&row)?;
                        report.dropped.push(left.path.clone());
                    }
                }
            }
            if self.mark_hint {
                txn.set_no_section_hint_shown()?;
            }
            index.fault(Fault::SettleCommit)
        })?;
        index.publish_committed(&committed);
        report.generation = Some(committed.generation);
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen_index::lookup::{LookupMeta, StatusKind, StatusReason};
    use crate::codegen_index::testfs::{Fake, Ws, guard, hash, ws, ws_no_db};

    const ADDR: &str = "//p:gen";

    type Rev<'a> = &'a [(&'a str, &'a [u8])];

    fn landings(rev: Rev<'_>) -> Vec<Landing> {
        rev.iter()
            .map(|(p, b)| Landing {
                path: p.as_bytes().to_vec(),
                output: "out".into(),
                content: LandingContent::File {
                    hash: hash(b),
                    x: false,
                },
            })
            .collect()
    }

    /// Every path under `gen/` is inside the target's current copy output.
    fn inside(p: &[u8]) -> bool {
        p.starts_with(b"gen/")
    }

    fn plan(w: &Ws, addr: &str, rev: Rev<'_>) -> Option<Plan> {
        w.snap().needs_write_back(addr, &landings(rev)).unwrap()
    }

    /// Step 3: the engine writes what step 2 selected.
    fn write_bytes(w: &Ws, wb: &WriteBack, rev: Rev<'_>) {
        for l in wb.writes() {
            let (p, b) = rev
                .iter()
                .find(|(p, _)| p.as_bytes() == l.path.as_slice())
                .unwrap();
            w.write(p, b);
        }
    }

    /// A whole write-back of `rev`; `None` when step 1 found nothing to do.
    fn run(w: &Ws, addr: &str, rev: Rev<'_>) -> Option<SettleReport> {
        let plan = plan(w, addr, rev)?;
        let wb = w.index.begin_write_back(guard(addr), plan).unwrap();
        write_bytes(w, &wb, rev);
        Some(wb.settle(&inside).unwrap())
    }

    fn status(w: &Ws, rel: &str) -> (StatusKind, Option<StatusReason>) {
        let s = w.snap();
        let st = s.status(rel.as_bytes(), &LookupMeta::point(false)).unwrap();
        (st.kind, st.reason)
    }

    fn pending(w: &Ws, rel: &str) -> bool {
        w.db_row(rel).is_some_and(|r| r.pending_hash.is_some())
    }

    fn rows(w: &Ws, addr: &str) -> Vec<(String, RowState)> {
        let mut v: Vec<_> = w
            .index
            .store()
            .unwrap()
            .rows_for_addr(addr)
            .unwrap()
            .into_iter()
            .map(|r| (String::from_utf8(r.path).unwrap(), r.state))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    const REV1: Rev<'static> = &[("gen/a", b"a1"), ("gen/b", b"b1")];
    const REV2: Rev<'static> = &[("gen/a", b"a2")];

    #[test]
    fn first_write_back_records_every_entry() {
        let w = ws();
        let r = run(&w, ADDR, REV1).unwrap();
        assert!(r.generation.is_some());
        for p in ["gen/a", "gen/b"] {
            let row = w.db_row(p).unwrap();
            assert!(row.pending_hash.is_none() && row.record.is_some(), "{p}");
            assert_eq!(row.output.as_deref(), Some("out"));
            assert_eq!(
                status(&w, p),
                (StatusKind::Generated, Some(StatusReason::Recorded))
            );
        }
    }

    #[test]
    fn warm_run_writes_nothing_even_with_dropped_rows() {
        let w = ws();
        // An imported row the target no longer produces ends up `dropped`.
        w.write("gen/i", b"imported");
        let mut imported = w.recorded("gen/i", ADDR);
        imported.state = RowState::Imported;
        w.put(&[imported]);
        run(&w, ADDR, REV1).unwrap();
        assert!(rows(&w, ADDR).contains(&("gen/i".into(), RowState::Dropped)));
        let g = w.generation();
        assert!(plan(&w, ADDR, REV1).is_none(), "a warm run has no plan");
        assert!(run(&w, ADDR, REV1).is_none());
        assert_eq!(w.generation(), g, "no commit, no generation move");
    }

    #[test]
    fn pending_row_always_needs_work() {
        let w = ws();
        run(&w, ADDR, REV1).unwrap();
        let mut row = w.db_row("gen/a").unwrap();
        // The bytes and the record are equal: only the pending mark is left.
        row.pending_hash = row.record.as_ref().map(|r| r.hash.clone());
        w.put(&[row]);
        let wb = w
            .index
            .begin_write_back(guard(ADDR), plan(&w, ADDR, REV1).unwrap())
            .unwrap();
        let writes: Vec<_> = wb.writes().map(|l| l.path.clone()).collect();
        assert_eq!(writes, vec![b"gen/a".to_vec()]);
        write_bytes(&w, &wb, REV1);
        wb.settle(&inside).unwrap();
        assert!(!pending(&w, "gen/a"));
        assert!(plan(&w, ADDR, REV1).is_none());
    }

    #[test]
    fn leftover_deleted_only_after_fresh_hash() {
        let w = ws();
        run(
            &w,
            ADDR,
            &[("gen/a", b"a1"), ("gen/b", b"b1"), ("gen/c", b"c1")],
        )
        .unwrap();
        // gen/b: same stat as recorded, other bytes: a stat match never gates
        // a delete (J5).
        let rec = w.db_row("gen/b").unwrap().record.unwrap();
        w.write("gen/b", b"XX");
        let st = FileStat {
            kind: StatKind::File,
            size: rec.size,
            mtime_ns: rec.mtime_ns,
            ino: rec.ino,
            dev: rec.dev,
            mode: rec.mode,
        };
        w.fs.fake(&w.abs("gen/b"), Fake::Stat(st));
        let r = run(&w, ADDR, REV2).unwrap();
        assert_eq!(r.deleted, vec![b"gen/c".to_vec()]);
        assert_eq!(r.dropped, vec![b"gen/b".to_vec()]);
        assert_eq!(w.read("gen/b").as_deref(), Some(&b"XX"[..]), "kept");
        assert!(w.read("gen/c").is_none(), "heph's own bytes deleted");
        assert!(w.db_row("gen/c").is_none());
        assert_eq!(w.db_row("gen/b").unwrap().state, RowState::Dropped);
    }

    #[test]
    fn leftover_outside_current_copy_output_is_dropped() {
        let w = ws();
        run(&w, ADDR, &[("gen/a", b"a1"), ("old/x", b"x1")]).unwrap();
        let r = run(&w, ADDR, REV2).unwrap();
        assert_eq!(r.dropped, vec![b"old/x".to_vec()]);
        assert!(w.read("old/x").is_some());
    }

    #[test]
    fn imported_leftover_dropped_not_deleted() {
        let w = ws();
        w.write("gen/i", b"imported");
        let mut imported = w.recorded("gen/i", ADDR);
        imported.state = RowState::Imported;
        imported.output = None;
        w.put(&[imported]);
        let r = run(&w, ADDR, REV2).unwrap();
        assert_eq!(r.dropped, vec![b"gen/i".to_vec()]);
        assert!(r.deleted.is_empty());
        assert_eq!(w.read("gen/i").as_deref(), Some(&b"imported"[..]));
    }

    #[test]
    fn imported_row_produced_again_becomes_written() {
        let w = ws();
        w.write("gen/a", b"a2");
        let mut imported = w.recorded("gen/a", ADDR);
        imported.state = RowState::Imported;
        imported.output = None;
        w.put(&[imported]);
        run(&w, ADDR, REV2).unwrap();
        let row = w.db_row("gen/a").unwrap();
        assert_eq!(row.state, RowState::Written);
        assert_eq!(row.output.as_deref(), Some("out"));
    }

    #[test]
    fn interleaved_write_backs_each_write_one_revision() {
        let rev_a: Rev<'_> = &[("gen/a", b"A1"), ("gen/b", b"B1")];
        let rev_b: Rev<'_> = &[("gen/a", b"A2"), ("gen/c", b"C2")];
        for (first, second) in [(rev_a, rev_b), (rev_b, rev_a)] {
            let w = ws();
            run(&w, ADDR, &[("gen/a", b"A0"), ("gen/d", b"D0")]).unwrap();
            // Both plans from one snapshot, before either takes the lock.
            let p1 = plan(&w, ADDR, first).unwrap();
            let p2 = plan(&w, ADDR, second).unwrap();
            for (p, rev) in [(p1, first), (p2, second)] {
                let wb = w.index.begin_write_back(guard(ADDR), p).unwrap();
                write_bytes(&w, &wb, rev);
                wb.settle(&inside).unwrap();
            }
            // Exactly `second`, never a mix.
            let want: Vec<(String, RowState)> = second
                .iter()
                .map(|(p, _)| ((*p).to_owned(), RowState::Written))
                .collect();
            assert_eq!(rows(&w, ADDR), want);
            for (p, b) in second {
                assert_eq!(w.read(p).as_deref(), Some(*b));
                assert_eq!(status(&w, p).0, StatusKind::Generated);
            }
            for p in ["gen/a", "gen/b", "gen/c", "gen/d"] {
                if !second.iter().any(|(q, _)| *q == p) {
                    assert!(w.read(p).is_none(), "{p} left behind");
                }
            }
            assert!(plan(&w, ADDR, second).is_none());
        }
    }

    #[test]
    fn another_addrs_row_never_touched() {
        let w = ws();
        w.write("gen/z", b"z");
        let other = w.recorded("gen/z", "//q:other");
        w.put(std::slice::from_ref(&other));
        run(&w, ADDR, REV1).unwrap();
        run(&w, ADDR, REV2).unwrap();
        let after = w.db_row("gen/z").unwrap();
        assert_eq!((after.addr, after.record), (other.addr, other.record));
        assert_eq!(w.read("gen/z").as_deref(), Some(&b"z"[..]));
    }

    #[test]
    fn new_section_match_flags_once_then_warm() {
        let w = ws();
        run(&w, ADDR, REV2).unwrap();
        assert!(!w.db_row("gen/a").unwrap().seen_in_section);
        w.section(&["/gen"]);
        let wb = w
            .index
            .begin_write_back(guard(ADDR), plan(&w, ADDR, REV2).unwrap())
            .unwrap();
        assert_eq!(wb.writes().count(), 0);
        assert_eq!(wb.pending_generation(), None, "no step-2 commit");
        assert_eq!(
            wb.actions().map(|(_, a)| a).collect::<Vec<_>>(),
            vec![EntryAction::FlagOnly]
        );
        wb.settle(&inside).unwrap();
        assert!(w.db_row("gen/a").unwrap().seen_in_section);
        assert!(plan(&w, ADDR, REV2).is_none());
    }

    #[test]
    fn hand_edit_is_reported_when_overwritten() {
        let w = ws();
        run(&w, ADDR, REV2).unwrap();
        w.write("gen/a", b"hand edit");
        let r = run(&w, ADDR, REV2).unwrap();
        assert_eq!(r.overwritten_edits, vec![b"gen/a".to_vec()]);
        assert_eq!(w.read("gen/a").as_deref(), Some(&b"a2"[..]));
    }

    #[test]
    fn symlink_landing_recorded_by_target() {
        let w = ws();
        let l = vec![Landing {
            path: b"gen/l".to_vec(),
            output: "out".into(),
            content: LandingContent::Symlink {
                target: b"a".to_vec(),
            },
        }];
        let p = w.snap().needs_write_back(ADDR, &l).unwrap().unwrap();
        let wb = w.index.begin_write_back(guard(ADDR), p).unwrap();
        std::fs::create_dir_all(w.abs("gen")).unwrap();
        std::os::unix::fs::symlink("a", w.abs("gen/l")).unwrap();
        wb.settle(&inside).unwrap();
        let row = w.db_row("gen/l").unwrap();
        assert_eq!(row.link_target.as_deref(), Some(&b"a"[..]));
        assert_eq!(status(&w, "gen/l").0, StatusKind::Generated);
        assert!(w.snap().needs_write_back(ADDR, &l).unwrap().is_none());
    }

    #[test]
    fn no_section_hint_recorded_in_settle() {
        let w = ws();
        assert!(!w.index.no_section_hint_shown().unwrap());
        let mut wb = w
            .index
            .begin_write_back(guard(ADDR), plan(&w, ADDR, REV2).unwrap())
            .unwrap();
        wb.mark_no_section_hint_shown();
        write_bytes(&w, &wb, REV2);
        wb.settle(&inside).unwrap();
        assert!(w.index.no_section_hint_shown().unwrap());
    }

    #[test]
    fn write_back_without_db_errors_clearly() {
        let w = ws_no_db();
        let p = w
            .snap()
            .needs_write_back(ADDR, &landings(REV2))
            .unwrap()
            .unwrap();
        let err = format!(
            "{:#}",
            w.index.begin_write_back(guard(ADDR), p).unwrap_err()
        );
        assert!(err.contains("create_db"), "{err}");
    }

    #[test]
    fn write_back_under_another_targets_lock_is_refused() {
        let w = ws();
        let p = plan(&w, ADDR, REV2).unwrap();
        let err = w.index.begin_write_back(guard("//q:other"), p).unwrap_err();
        assert!(format!("{err}").contains("//q:other"), "{err}");
    }

    // Store and crash points.

    #[test]
    fn crash_after_pending_commit_keeps_file_excluded() {
        let w = ws();
        run(&w, ADDR, REV2).unwrap();
        let rev: Rev<'_> = &[("gen/a", b"a3"), ("gen/n", b"n3")];
        let wb = w
            .index
            .begin_write_back(guard(ADDR), plan(&w, ADDR, rev).unwrap())
            .unwrap();
        drop(wb); // crash: no byte moved
        // The old bytes still match the record, but the row is pending.
        assert_eq!(
            status(&w, "gen/a"),
            (StatusKind::Generated, Some(StatusReason::Pending))
        );
        assert!(pending(&w, "gen/a") && pending(&w, "gen/n"));
    }

    #[test]
    fn crash_mid_write_partial_file_excluded() {
        let w = ws();
        let rev: Rev<'_> = &[("gen/a", b"complete bytes")];
        let wb = w
            .index
            .begin_write_back(guard(ADDR), plan(&w, ADDR, rev).unwrap())
            .unwrap();
        w.write("gen/a", b"compl");
        drop(wb);
        assert_eq!(
            status(&w, "gen/a"),
            (StatusKind::Generated, Some(StatusReason::Pending))
        );
        // Another process reads the db, not this one's snapshot.
        let other = CodegenIndex::open(&w.root, &w.root.join(".heph"), w.fs.clone()).unwrap();
        let s = other.snapshot().unwrap();
        let st = s.status(b"gen/a", &LookupMeta::point(false)).unwrap();
        assert_eq!(st.reason, Some(StatusReason::Pending));
    }

    #[test]
    fn crash_before_settle_settled_by_next_write_back() {
        let w = ws();
        let wb = w
            .index
            .begin_write_back(guard(ADDR), plan(&w, ADDR, REV1).unwrap())
            .unwrap();
        write_bytes(&w, &wb, REV1);
        drop(wb); // crash after step 3
        assert!(pending(&w, "gen/a") && pending(&w, "gen/b"));
        run(&w, ADDR, REV1).unwrap();
        assert!(!pending(&w, "gen/a") && !pending(&w, "gen/b"));
        assert_eq!(status(&w, "gen/b").0, StatusKind::Generated);
        assert!(plan(&w, ADDR, REV1).is_none());
    }

    #[test]
    fn crash_after_unlink_leaves_harmless_row() {
        let w = ws();
        run(&w, ADDR, REV1).unwrap();
        w.index.set_fault(|f| {
            if f == Fault::AfterUnlink {
                anyhow::bail!("crash")
            }
            Ok(())
        });
        let wb = w
            .index
            .begin_write_back(guard(ADDR), plan(&w, ADDR, REV2).unwrap())
            .unwrap();
        write_bytes(&w, &wb, REV2);
        wb.settle(&inside).unwrap_err();
        // Unlinked, row still there: a dangling row changes no answer.
        assert!(w.read("gen/b").is_none());
        assert!(w.db_row("gen/b").is_some());
        assert_eq!(status(&w, "gen/b"), (StatusKind::Source, None));
        w.index.set_fault(|_| Ok(()));
        let r = run(&w, ADDR, REV2).unwrap();
        assert_eq!(r.deleted, vec![b"gen/b".to_vec()]);
        assert!(w.db_row("gen/b").is_none());
        assert!(plan(&w, ADDR, REV2).is_none());
    }

    #[test]
    fn failed_pending_commit_aborts_before_bytes_move() {
        let w = ws();
        w.index.set_fault(|f| {
            if f == Fault::PendingCommit {
                anyhow::bail!("disk full")
            }
            Ok(())
        });
        let g = w.generation();
        let err = w
            .index
            .begin_write_back(guard(ADDR), plan(&w, ADDR, REV1).unwrap());
        assert!(err.is_err(), "no WriteBack, so no byte can move");
        assert_eq!(w.generation(), g, "rolled back");
        assert!(w.db_row("gen/a").is_none());
        assert!(w.snap().row(b"gen/a").is_none(), "nothing published");
    }

    #[test]
    fn failed_settle_commit_fails_write_back() {
        let w = ws();
        w.index.set_fault(|f| {
            if f == Fault::SettleCommit {
                anyhow::bail!("disk full")
            }
            Ok(())
        });
        let wb = w
            .index
            .begin_write_back(guard(ADDR), plan(&w, ADDR, REV1).unwrap())
            .unwrap();
        write_bytes(&w, &wb, REV1);
        wb.settle(&inside).unwrap_err();
        // The pending rows stay and keep their files excluded.
        assert!(pending(&w, "gen/a") && pending(&w, "gen/b"));
        assert_eq!(status(&w, "gen/a").1, Some(StatusReason::Pending));
    }

    #[test]
    fn status_settles_pending_rows_under_lock() {
        let w = ws();
        run(&w, ADDR, &[("gen/old", b"old"), ("gen/part", b"p0")]).unwrap();
        let mut rows = Vec::new();
        // The pending bytes landed.
        w.write("gen/landed", b"new");
        let mut landed = Row::new(b"gen/landed".to_vec(), ADDR, RowKind::File);
        landed.pending_hash = Some(hash(b"new"));
        rows.push(landed);
        // The write never landed: the recorded bytes are still there.
        let mut old = w.db_row("gen/old").unwrap();
        old.pending_hash = Some(hash(b"never"));
        rows.push(old);
        // A partial write.
        let mut part = w.db_row("gen/part").unwrap();
        part.pending_hash = Some(hash(b"p1 full"));
        w.write("gen/part", b"p1");
        rows.push(part);
        // No file at all.
        let mut gone = Row::new(b"gen/gone".to_vec(), ADDR, RowKind::File);
        gone.pending_hash = Some(hash(b"g"));
        rows.push(gone);
        w.put(&rows);

        let mut got = w.index.settle_pending(&guard(ADDR)).unwrap();
        got.sort_by(|a, b| a.path.cmp(&b.path));
        let outcomes: Vec<_> = got
            .iter()
            .map(|s| (String::from_utf8_lossy(&s.path).into_owned(), s.outcome))
            .collect();
        assert_eq!(
            outcomes,
            vec![
                ("gen/gone".into(), PendingOutcome::Interrupted),
                ("gen/landed".into(), PendingOutcome::Recorded),
                ("gen/old".into(), PendingOutcome::Cleared),
                ("gen/part".into(), PendingOutcome::Interrupted),
            ]
        );
        let landed = w.db_row("gen/landed").unwrap();
        assert!(landed.pending_hash.is_none());
        assert_eq!(landed.record.map(|r| r.hash), Some(hash(b"new")));
        assert!(!pending(&w, "gen/old"));
        assert!(pending(&w, "gen/part") && pending(&w, "gen/gone"));
        assert_eq!(status(&w, "gen/landed").0, StatusKind::Generated);
        assert_eq!(status(&w, "gen/part").1, Some(StatusReason::Pending));
    }

    #[test]
    fn read_only_index_errors_on_write_back() {
        use std::os::unix::fs::PermissionsExt as _;
        let w = ws();
        let db = w.index.db_path().to_path_buf();
        let p = plan(&w, ADDR, REV1).unwrap();
        for f in [
            db.clone(),
            db.with_extension("db-wal"),
            db.with_extension("db-shm"),
        ] {
            if f.exists() {
                std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o444)).unwrap();
            }
        }
        if std::fs::OpenOptions::new().write(true).open(&db).is_ok() {
            eprintln!("skipping: permissions are not enforced for this user");
            return;
        }
        let name = db.display().to_string();
        let err = match CodegenIndex::open(&w.root, &w.root.join(".heph"), w.fs.clone()) {
            Err(e) => e,
            Ok(idx) => Arc::new(idx).begin_write_back(guard(ADDR), p).unwrap_err(),
        };
        let err = format!("{err:#}");
        assert!(err.contains(&name), "names the file: {err}");
    }
}
