//! The upgrade from the xattr: `heph tool codegen import` records the files an
//! older heph stamped with `user.heph.codegen` (spec "Upgrade from the xattr",
//! Decision 23).
//!
//! Only that command runs it, on the blocking pool, under the import lock. It
//! is split so no lock is ever waited on inside a blocking job (J9):
//!
//! 1. [`scan`]: one walk of the tree with the xattr probe, honouring the fs
//!    walk's skip rules (`.heph*` dirs, `fs.skip`). Nothing is written.
//! 2. No db: [`CodegenIndex::import_create`] builds every row into the temp db
//!    and renames it into place ([`store::create_with_rows`]), so the db
//!    appears complete or not at all.
//! 3. A db: [`CodegenIndex::import_merge`], one transaction per target, each
//!    under that target's lock, which the engine takes on the async side.
//!
//! What it records: an `imported` row per stamped file whose path has no row,
//! or whose row's bytes no longer verify; a pending row is never touched. An
//! empty, non-UTF-8 or unparseable value is skipped (counted). A filesystem
//! without xattrs records nothing. A second run records nothing new.

use super::fs::{FileStat, IndexFs, StatKind, lstat_opt};
use super::section::Section;
use super::store::{self, Row, RowKind, RowState};
use super::write::{FsClock, fresh_row_hash, record_of};
use super::{CodegenIndex, ImportLockGuard, TargetLockGuard};
use crate::Ignore;
use anyhow::{Context as _, Result};
use std::collections::BTreeMap;
use std::io;
use std::path::Path;

/// The provenance xattr an older heph stamped on every `copy` output.
pub const CODEGEN_XATTR: &str = "user.heph.codegen";

/// Reads one file's provenance xattr. Injectable, so a test can stand in a
/// filesystem without xattrs or count the probes a code path makes.
pub trait XattrProbe: Send + Sync {
    /// The value of [`CODEGEN_XATTR`] on `path` (never following a symlink),
    /// `None` when unset. An [`io::ErrorKind::Unsupported`] error means the
    /// filesystem has no xattrs.
    fn get(&self, path: &Path) -> io::Result<Option<Vec<u8>>>;
}

/// The real probe.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealXattr;

impl XattrProbe for RealXattr {
    fn get(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        xattr::get(path, CODEGEN_XATTR)
    }
}

/// Remove [`CODEGEN_XATTR`] from `path` (never following a symlink), so a
/// file `codegen keep` made source never comes back as generated after a lost
/// db and an import, or under an older heph (Decision 24). Absent, an absent
/// attribute, or a filesystem without xattrs: nothing to do.
pub fn strip_xattr(path: &Path) -> Result<()> {
    match xattr::get(path, CODEGEN_XATTR) {
        Ok(Some(_)) => {}
        Ok(None) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::Unsupported || super::fs::is_absent(&e) => {
            return Ok(());
        }
        Err(e) => {
            return Err(e)
                .with_context(|| format!("reading {CODEGEN_XATTR} of {}", path.display()));
        }
    }
    xattr::remove(path, CODEGEN_XATTR)
        .with_context(|| format!("removing {CODEGEN_XATTR} from {}", path.display()))
}

/// One stamped file, as the scan found it.
#[derive(Debug, Clone)]
pub struct Stamped {
    /// Root-relative, normalised.
    pub path: Vec<u8>,
    pub kind: RowKind,
    /// The fresh hash, as a row records it.
    pub hash: String,
    pub link_target: Option<Vec<u8>>,
    pub stat: FileStat,
    /// A section line covers the path.
    pub seen_in_section: bool,
}

/// What [`scan`] found.
#[derive(Debug, Default)]
pub struct Scan {
    /// Stamped files by the addr their value names, each list sorted by path.
    pub by_addr: BTreeMap<String, Vec<Stamped>>,
    /// Stamped files whose value is empty, not UTF-8 or not an addr.
    pub skipped: Vec<Vec<u8>>,
    /// `false` when the filesystem reported no xattr support.
    pub supported: bool,
}

impl Scan {
    pub fn files(&self) -> usize {
        self.by_addr.values().map(Vec::len).sum()
    }
}

/// The counts an import reports.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ImportCounts {
    /// Rows written: new paths, or rows whose bytes no longer verified.
    pub recorded: usize,
    /// Stamped files whose row already verifies, or is pending.
    pub unchanged: usize,
}

impl ImportCounts {
    pub fn add(&mut self, other: ImportCounts) {
        self.recorded += other.recorded;
        self.unchanged += other.unchanged;
    }
}

/// The addr a stamp names, normalised as the write-back formats it.
fn stamp_addr(value: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(value).ok()?.trim();
    if s.is_empty() {
        return None;
    }
    hmodel::htaddr::parse_addr(s).ok().map(|a| a.format())
}

/// Walk the tree under `root` once and read the provenance xattr of every
/// file and symlink, skipping what `ignore` prunes (`.heph*` dirs included).
/// Writes nothing.
pub fn scan(
    root: &Path,
    ignore: &Ignore,
    fs: &dyn IndexFs,
    probe: &dyn XattrProbe,
    section: &Section,
) -> Result<Scan> {
    ignore.ensure_compiled()?;
    let mut out = Scan {
        supported: true,
        ..Scan::default()
    };
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if dir != root && super::fs::is_absent(&e) => continue,
            Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
        };
        let mut names = Vec::new();
        for entry in entries {
            names.push(entry.with_context(|| format!("listing {}", dir.display()))?.path());
        }
        names.sort();
        for abs in names {
            let Ok(rel) = abs.strip_prefix(root) else {
                continue;
            };
            let Some(st) = lstat_opt(fs, &abs)? else {
                continue;
            };
            let kind = match st.kind {
                StatKind::Dir => {
                    if !ignore.prune_dir(&abs, rel) {
                        stack.push(abs);
                    }
                    continue;
                }
                StatKind::File => RowKind::File,
                StatKind::Symlink => RowKind::Symlink,
                StatKind::Other => continue,
            };
            if ignore.exclude_file(rel) {
                continue;
            }
            let value = match probe.get(&abs) {
                Ok(Some(v)) => v,
                Ok(None) => continue,
                Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                    out.supported = false;
                    out.by_addr.clear();
                    out.skipped.clear();
                    return Ok(out);
                }
                Err(e) if super::fs::is_absent(&e) => continue,
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("reading the codegen xattr of {}", abs.display()));
                }
            };
            let rel = rel.as_os_str().as_encoded_bytes().to_vec();
            let Some(addr) = stamp_addr(&value) else {
                out.skipped.push(rel);
                continue;
            };
            let Some((hash, link_target)) = fresh_row_hash(fs, &abs, kind, Some(&st))? else {
                continue;
            };
            let seen_in_section = section.match_point(&rel, false).is_territory();
            out.by_addr.entry(addr).or_default().push(Stamped {
                path: rel,
                kind,
                hash,
                link_target,
                stat: st,
                seen_in_section,
            });
        }
    }
    for files in out.by_addr.values_mut() {
        files.sort_by(|a, b| a.path.cmp(&b.path));
    }
    out.skipped.sort();
    Ok(out)
}

/// The `imported` row for `s`, owned by `addr`.
fn imported_row(addr: &str, s: &Stamped, written_at_ns: i64, seen: bool) -> Row {
    let mut row = Row::new(s.path.clone(), addr, s.kind);
    row.state = RowState::Imported;
    row.record = Some(record_of(s.hash.clone(), &s.stat, written_at_ns));
    row.link_target = s.link_target.clone();
    row.seen_in_section = seen;
    row
}

/// Whether `row` still records the bytes `s` found.
fn verifies(row: &Row, s: &Stamped) -> bool {
    row.kind == s.kind && row.record.as_ref().is_some_and(|r| r.hash == s.hash)
}

impl CodegenIndex {
    /// With no db: build every scanned row into a temp db and rename it onto
    /// this checkout's db file, under the import lock. `None` when a db
    /// already exists (open or on disk): the caller merges instead.
    pub fn import_create(
        &self,
        import: &ImportLockGuard,
        scan: &Scan,
    ) -> Result<Option<ImportCounts>> {
        if self.open_store()?.is_some() {
            return Ok(None);
        }
        let dir = self
            .db_path()
            .parent()
            .context("the codegen db path has no parent directory")?;
        std::fs::create_dir_all(dir).with_context(|| {
            format!("creating the codegen index directory {}", dir.display())
        })?;
        let mut clock = FsClock::new(self);
        let mut rows = Vec::with_capacity(scan.files());
        for (addr, files) in &scan.by_addr {
            for s in files {
                let at = clock.written_at(&s.stat)?;
                rows.push(imported_row(addr, s, at, s.seen_in_section));
            }
        }
        if !store::create_with_rows(self.db_path(), import, &rows)? {
            return Ok(None);
        }
        self.open_store()?.with_context(|| {
            format!(
                "the codegen index {} vanished right after the import created it",
                self.db_path().display()
            )
        })?;
        Ok(Some(ImportCounts {
            recorded: rows.len(),
            unchanged: 0,
        }))
    }

    /// Merge one target's stamped files into the existing db, under that
    /// target's lock: one transaction, committed only when something changes.
    /// A pending row is never touched; a row that verifies only gains
    /// `seen_in_section` (ORed, E10).
    pub fn import_merge(&self, guard: &TargetLockGuard, files: &[Stamped]) -> Result<ImportCounts> {
        let store = self.require_store()?;
        let addr = guard.addr();
        let mut counts = ImportCounts::default();
        let mut changes: Vec<(&Stamped, Option<Row>)> = Vec::new();
        for s in files {
            match store.row(&s.path)? {
                Some(r) if r.pending_hash.is_some() => counts.unchanged += 1,
                Some(r) if verifies(&r, s) => {
                    counts.unchanged += 1;
                    if s.seen_in_section && !r.seen_in_section {
                        changes.push((s, Some(r)));
                    }
                }
                other => {
                    counts.recorded += 1;
                    changes.push((s, other));
                }
            }
        }
        if changes.is_empty() {
            return Ok(counts);
        }
        let mut clock = FsClock::new(self);
        let mut stamped = Vec::with_capacity(changes.len());
        for (s, _) in &changes {
            stamped.push(clock.written_at(&s.stat)?);
        }
        let committed = store.write(|txn| {
            for ((s, _), at) in changes.iter().zip(&stamped) {
                // Re-read under the transaction: another writer may have
                // moved the row since the check above.
                let current = txn.row(&s.path)?;
                match current {
                    Some(r) if r.pending_hash.is_some() => {}
                    Some(mut r) if verifies(&r, s) => {
                        if s.seen_in_section && !r.seen_in_section {
                            r.seen_in_section = true;
                            txn.put(&r)?;
                        }
                    }
                    other => {
                        let seen = s.seen_in_section || other.is_some_and(|r| r.seen_in_section);
                        txn.put(&imported_row(addr, s, *at, seen))?;
                    }
                }
            }
            Ok(())
        })?;
        self.publish_committed(&committed);
        Ok(counts)
    }
}

#[cfg(test)]
mod tests {
    use super::super::testfs::{guard, ws, ws_no_db};
    use super::*;
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// An in-memory xattr table, with a probe counter and injectable errors.
    #[derive(Default)]
    struct FakeXattr {
        values: Mutex<HashMap<PathBuf, Vec<u8>>>,
        unsupported: bool,
        fail_on: Option<PathBuf>,
        probes: AtomicU64,
    }

    impl FakeXattr {
        fn stamp(&self, path: PathBuf, v: &[u8]) {
            self.values.lock().insert(path, v.to_vec());
        }
    }

    impl XattrProbe for FakeXattr {
        fn get(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
            self.probes.fetch_add(1, Ordering::Relaxed);
            if self.unsupported {
                return Err(io::Error::from(io::ErrorKind::Unsupported));
            }
            if self.fail_on.as_deref() == Some(path) {
                return Err(io::Error::other("injected fault"));
            }
            Ok(self.values.lock().get(path).cloned())
        }
    }

    fn ignore() -> Ignore {
        Ignore::git_only()
    }

    fn run_scan(w: &super::super::testfs::Ws, x: &FakeXattr) -> Result<Scan> {
        let snap = w.index.snapshot()?;
        scan(&w.root, &ignore(), w.fs.as_ref(), x, snap.section())
    }

    fn lock() -> ImportLockGuard {
        ImportLockGuard::new(Box::new(()))
    }

    /// Import as the engine does: create, else merge target by target.
    fn import(w: &super::super::testfs::Ws, x: &FakeXattr) -> ImportCounts {
        let s = run_scan(w, x).unwrap();
        if let Some(c) = w.index.import_create(&lock(), &s).unwrap() {
            return c;
        }
        let mut total = ImportCounts::default();
        for (addr, files) in &s.by_addr {
            total.add(w.index.import_merge(&guard(addr), files).unwrap());
        }
        total
    }

    #[test]
    fn codegen_import_is_atomic() {
        let w = ws_no_db();
        w.write("p/a.go", b"a");
        w.write("p/b.go", b"b");
        let x = FakeXattr {
            fail_on: Some(w.abs("p/b.go")),
            ..FakeXattr::default()
        };
        x.stamp(w.abs("p/a.go"), b"//p:gen");
        x.stamp(w.abs("p/b.go"), b"//p:gen");
        assert!(run_scan(&w, &x).is_err(), "the fault fails the walk");
        assert!(!w.index.db_path().exists(), "no db after a failed walk");

        let x = FakeXattr::default();
        x.stamp(w.abs("p/a.go"), b"//p:gen");
        x.stamp(w.abs("p/b.go"), b"//p:gen");
        let c = import(&w, &x);
        assert_eq!(c.recorded, 2);
        assert!(w.index.db_path().exists());
        assert_eq!(w.snap().rows_for_addr("//p:gen").len(), 2);
    }

    #[test]
    fn codegen_import_merge_commits_per_target() {
        let w = ws();
        w.write("p/a.go", b"a");
        w.write("q/b.go", b"b");
        let x = FakeXattr::default();
        x.stamp(w.abs("p/a.go"), b"//p:gen");
        x.stamp(w.abs("q/b.go"), b"//q:gen");
        let s = run_scan(&w, &x).unwrap();
        assert!(w.index.import_create(&lock(), &s).unwrap().is_none());
        // The first target commits; the import dies before the second.
        w.index
            .import_merge(&guard("//p:gen"), &s.by_addr["//p:gen"])
            .unwrap();
        assert_eq!(w.snap().rows_for_addr("//p:gen").len(), 1);
        assert!(w.snap().rows_for_addr("//q:gen").is_empty());

        let c = import(&w, &x);
        assert_eq!(c, ImportCounts { recorded: 1, unchanged: 1 });
        assert_eq!(w.snap().rows_for_addr("//q:gen").len(), 1);
    }

    #[test]
    fn import_recheck_runs_under_lock_in_its_job() {
        // Another creator made the db between the scan and the create: the
        // re-check under the lock finds it and the import merges instead.
        let w = ws_no_db();
        w.write("p/a.go", b"a");
        let x = FakeXattr::default();
        x.stamp(w.abs("p/a.go"), b"//p:gen");
        let s = run_scan(&w, &x).unwrap();
        w.index.create_db(&lock()).unwrap();
        let before = w.generation();
        assert!(w.index.import_create(&lock(), &s).unwrap().is_none());
        assert_eq!(w.generation(), before, "the existing db is not replaced");
        let c = w
            .index
            .import_merge(&guard("//p:gen"), &s.by_addr["//p:gen"])
            .unwrap();
        assert_eq!(c.recorded, 1);
    }

    #[test]
    fn stale_tmp_files_deleted_under_lock() {
        let w = ws_no_db();
        let dir = w.index.db_path().parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        let stale = dir.join(format!("{}crash.db", store::TMP_PREFIX));
        std::fs::write(&stale, b"partial").unwrap();
        w.write("p/a.go", b"a");
        let x = FakeXattr::default();
        x.stamp(w.abs("p/a.go"), b"//p:gen");
        import(&w, &x);
        assert!(!stale.exists(), "the crash's temp db is gone");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(
            !names
                .iter()
                .any(|n| n.as_encoded_bytes().starts_with(store::TMP_PREFIX.as_bytes())),
            "{names:?}"
        );
    }

    #[test]
    fn import_skips_garbage_empty_and_non_utf8_values() {
        let w = ws();
        w.section(&["/p"]);
        let x = FakeXattr::default();
        for (name, v) in [
            ("p/empty.go", &b""[..]),
            ("p/bin.go", &b"\xff\xfe"[..]),
            ("p/junk.go", &b"not an addr"[..]),
            ("p/ok.go", &b"//p:gen"[..]),
        ] {
            w.write(name, b"x");
            x.stamp(w.abs(name), v);
        }
        let s = run_scan(&w, &x).unwrap();
        assert_eq!(s.skipped.len(), 3, "{:?}", s.skipped);
        assert_eq!(s.files(), 1);
        import(&w, &x);
        let snap = w.snap();
        assert_eq!(snap.rows().count(), 1);
        // A line still covers the skipped files: unrecorded, excluded.
        let st = snap.status(b"p/junk.go", &super::super::LookupMeta::point(false)).unwrap();
        assert_eq!(st.kind, super::super::StatusKind::Generated);
        assert_eq!(st.reason, Some(super::super::StatusReason::Unrecorded));
    }

    #[test]
    fn import_on_fs_without_xattrs_records_nothing() {
        let w = ws_no_db();
        w.write("p/a.go", b"a");
        w.write("p/b.go", b"b");
        let x = FakeXattr {
            unsupported: true,
            ..FakeXattr::default()
        };
        let s = run_scan(&w, &x).unwrap();
        assert!(!s.supported);
        assert_eq!(s.files(), 0);
        assert_eq!(x.probes.load(Ordering::Relaxed), 1, "stops at the first ENOTSUP");
    }

    #[test]
    fn import_never_touches_a_pending_row_and_is_idempotent() {
        let w = ws();
        w.section(&["/p/a.go"]);
        w.write("p/a.go", b"a");
        w.write("p/b.go", b"b");
        let mut pending = Row::new(b"p/b.go".to_vec(), "//p:gen", RowKind::File);
        pending.pending_hash = Some("h".into());
        w.put(&[pending.clone()]);
        let x = FakeXattr::default();
        x.stamp(w.abs("p/a.go"), b"//p:gen");
        x.stamp(w.abs("p/b.go"), b"//p:gen");
        let c = import(&w, &x);
        assert_eq!(c, ImportCounts { recorded: 1, unchanged: 1 });
        let b = w.db_row("p/b.go").unwrap();
        assert_eq!(b.pending_hash.as_deref(), Some("h"));
        let a = w.db_row("p/a.go").unwrap();
        assert_eq!(a.state, RowState::Imported);
        assert!(a.seen_in_section);
        assert_eq!(a.output, None);

        let g = w.generation();
        let c = import(&w, &x);
        assert_eq!(c, ImportCounts { recorded: 0, unchanged: 2 });
        assert_eq!(w.generation(), g, "a second run commits nothing");
    }

    #[test]
    fn import_skips_heph_dirs_and_fs_skip() {
        let w = ws_no_db();
        w.write(".heph/x.go", b"x");
        w.write("vendor/v.go", b"v");
        w.write("p/a.go", b"a");
        let x = FakeXattr::default();
        for p in [".heph/x.go", "vendor/v.go", "p/a.go"] {
            x.stamp(w.abs(p), b"//p:gen");
        }
        let ig = Ignore::new(&[], &["vendor/**".to_string()]).unwrap();
        let snap = w.index.snapshot().unwrap();
        let s = scan(&w.root, &ig, w.fs.as_ref(), &x, snap.section()).unwrap();
        let paths: Vec<_> = s.by_addr.values().flatten().map(|s| s.path.clone()).collect();
        assert_eq!(paths, vec![b"p/a.go".to_vec()]);
    }
}
