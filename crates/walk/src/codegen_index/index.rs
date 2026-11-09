//! [`CodegenIndex`]: this checkout's store (if its db exists), the snapshot
//! state, and the compiled section, with the freshness rules that tie them
//! together (spec: Mechanism, Where / Snapshot; Decisions 3, 16, 22).

use super::ImportLockGuard;
use super::fs::{IndexFs, is_absent};
use super::paths::db_path;
use super::section::Section;
use super::snapshot::{Base, LookupEnv, Snapshot, State};
use super::store::{self, Row, Store, StoreOptions};
use anyhow::{Context as _, Result};
use parking_lot::{Mutex, RwLock};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Overlay rows past which a publish folds the overlay into a new base.
pub const FOLD_THRESHOLD: usize = 4096;

/// Directories loaded on their own in one generation past which the next
/// one loads the whole index instead ([`CodegenIndex::snapshot_under`]): a
/// run that globs this many packages likely globs many more, and one full
/// scan costs about as much as a handful of directory reads.
pub const MAX_REGIONS: usize = 16;

/// The slack of the section's racy rule. Git re-reads an index entry whose
/// mtime is not strictly older than the index; here the compiled section is
/// re-read while the `.gitignore`'s mtime is within this of (or after) the
/// time its bytes were last read. Exact comparison is unsafe on a filesystem
/// whose mtime is coarser than the clock (1 s on some, 2 s on FAT): a
/// same-size rewrite a second after the read can carry an mtime *before* it.
/// Two seconds covers every supported filesystem's granularity, and costs a
/// re-read only for two seconds after a `.gitignore` change.
pub const SECTION_RACY_SLACK: Duration = Duration::from_secs(2);

/// Tunables; the defaults are the product's, tests shrink them.
#[derive(Debug, Clone)]
pub struct IndexOptions {
    pub store: StoreOptions,
    pub fold_threshold: usize,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            store: StoreOptions::default(),
            fold_threshold: FOLD_THRESHOLD,
        }
    }
}

/// Counters for tests and the bench: what the snapshot machinery did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexStats {
    /// Full row loads (reloads).
    pub reloads: u64,
    /// One directory's row loads ([`CodegenIndex::snapshot_under`]).
    pub region_loads: u64,
    /// Rows those loads (both kinds) read.
    pub rows_loaded: u64,
    /// Opens of a db that appeared after the index was opened.
    pub late_opens: u64,
    /// Overlay entries copied by publishes: each publish's own rows and
    /// tombstones, never the rest of the overlay (it is persistent).
    pub cloned_overlay_entries: u64,
    /// Overlay folds into a new base.
    pub folds: u64,
    /// Reads of the root `.gitignore`.
    pub section_reads: u64,
}

#[derive(Debug, Default)]
struct Stats {
    reloads: AtomicU64,
    region_loads: AtomicU64,
    rows_loaded: AtomicU64,
    late_opens: AtomicU64,
    cloned_overlay_entries: AtomicU64,
    folds: AtomicU64,
    section_reads: AtomicU64,
}

fn bump(c: &AtomicU64, n: u64) {
    c.fetch_add(n, Ordering::Relaxed);
}

/// Where a test may pause the machinery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookPoint {
    /// Inside the single-flight, before a late open.
    Open,
    /// Inside the single-flight, after a reload read the rows, before install.
    ReloadRead,
    /// Inside the single-flight, after a directory's rows were read, before
    /// install.
    RegionRead,
    /// Inside the single-flight, after a fold copied the pair, before install.
    FoldCopied,
}

#[cfg(test)]
type Hook = Arc<dyn Fn(HookPoint) + Send + Sync>;

/// The `.gitignore` identity the compiled section is keyed by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SectionKey {
    size: u64,
    mtime_ns: i64,
    ino: u64,
}

#[derive(Debug)]
struct SectionCache {
    /// `None`: no `.gitignore`.
    key: Option<SectionKey>,
    /// When `bytes` were last read and found current (ns since the epoch).
    verified_at_ns: i64,
    bytes: Vec<u8>,
    section: Arc<Section>,
}

/// The codegen file index of one checkout. One per engine, shared by `Arc`.
pub struct CodegenIndex {
    root: PathBuf,
    db_path: PathBuf,
    gitignore: PathBuf,
    env: Arc<LookupEnv>,
    opts: IndexOptions,
    store: OnceLock<Store>,
    /// Held only to clone or swap.
    state: RwLock<State>,
    /// The single-flight: a reload, a fold, and a late open run one at a time.
    flight: Mutex<()>,
    section: Mutex<SectionCache>,
    stats: Stats,
    #[cfg(test)]
    hook: Mutex<Option<Hook>>,
    #[cfg(test)]
    fault: Mutex<Option<FaultHook>>,
}

#[cfg(test)]
type FaultHook = Arc<dyn Fn(super::write::Fault) -> Result<()> + Send + Sync>;

impl std::fmt::Debug for CodegenIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodegenIndex")
            .field("root", &self.root)
            .field("db_path", &self.db_path)
            .field("open", &self.store.get().is_some())
            .finish_non_exhaustive()
    }
}

impl CodegenIndex {
    /// Open this checkout's index: its db if the file exists, else none until
    /// a commit creates it (or another process does). Never creates, imports
    /// or walks. A newer, corrupt or unreadable db is an error naming it.
    pub fn open(root: &Path, home: &Path, fs: Arc<dyn IndexFs>) -> Result<Self> {
        Self::open_with(root, home, fs, IndexOptions::default())
    }

    pub fn open_with(
        root: &Path,
        home: &Path,
        fs: Arc<dyn IndexFs>,
        opts: IndexOptions,
    ) -> Result<Self> {
        let index = Self::unopened_with(root, home, fs, opts);
        if exists(&index.db_path)? {
            drop(
                index
                    .store
                    .set(Store::open(&index.db_path, &index.opts.store)?),
            );
        }
        Ok(index)
    }

    /// This checkout's index with nothing opened: no disk access at all. The
    /// engine builds it at construction, so plugins get the `Arc` through
    /// `PluginInit`, and opens the db with [`Self::open_existing`] in the
    /// first request's opening step, before any reader runs.
    pub fn unopened(root: &Path, home: &Path, fs: Arc<dyn IndexFs>) -> Self {
        Self::unopened_with(root, home, fs, IndexOptions::default())
    }

    pub fn unopened_with(
        root: &Path,
        home: &Path,
        fs: Arc<dyn IndexFs>,
        opts: IndexOptions,
    ) -> Self {
        Self {
            root: root.to_path_buf(),
            gitignore: root.join(".gitignore"),
            db_path: db_path(root, home),
            env: Arc::new(LookupEnv::new(root.to_path_buf(), fs)),
            opts,
            store: OnceLock::new(),
            state: RwLock::new(State::default()),
            flight: Mutex::new(()),
            section: Mutex::new(SectionCache {
                key: None,
                verified_at_ns: 0,
                bytes: Vec::new(),
                section: Arc::new(Section::empty()),
            }),
            stats: Stats::default(),
            #[cfg(test)]
            hook: Mutex::new(None),
            #[cfg(test)]
            fault: Mutex::new(None),
        }
    }

    /// Open this checkout's db if it exists (and is not open yet): one stat
    /// when there is none. Never creates, imports or walks. `true` when a db
    /// is open afterwards.
    pub fn open_existing(&self) -> Result<bool> {
        Ok(self.store_or_late_open()?.is_some())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// This checkout's db file, whether or not it exists.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn fs(&self) -> &Arc<dyn IndexFs> {
        &self.env.fs
    }

    /// The open store, if the db has been opened.
    pub fn store(&self) -> Option<&Store> {
        self.store.get()
    }

    pub fn stats(&self) -> IndexStats {
        let s = &self.stats;
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        IndexStats {
            reloads: get(&s.reloads),
            region_loads: get(&s.region_loads),
            rows_loaded: get(&s.rows_loaded),
            late_opens: get(&s.late_opens),
            cloned_overlay_entries: get(&s.cloned_overlay_entries),
            folds: get(&s.folds),
            section_reads: get(&s.section_reads),
        }
    }

    /// The snapshot one glob, `file()` lookup, guard pass or command reads.
    ///
    /// One stat of the root `.gitignore` (recompiling the section on a
    /// change), then one `SELECT generation FROM meta`, reloading when it is
    /// above `loaded_through`. With no db open, one stat of the db path,
    /// opening the db when it has appeared.
    pub fn snapshot(&self) -> Result<Snapshot> {
        let section = self.section()?;
        if let Some(store) = self.store_or_late_open()? {
            let generation = store.generation()?;
            let st = self.state.read();
            let current = st.is_full() && generation <= st.loaded_through();
            drop(st);
            if !current {
                self.reload(store, generation)?;
            }
        }
        Ok(self.state.read().snapshot(section, Arc::clone(&self.env)))
    }

    /// [`Self::snapshot`] for a reader that asks only about paths at or under
    /// the root-relative, normalised directory `dir` (a glob's walk root, a
    /// `file()` path's parent; empty is the whole tree). When nothing loaded
    /// covers `dir`, it loads that directory's rows alone
    /// ([`Store::load_under`]), so a process that reads a few packages never
    /// reads the whole index. After [`MAX_REGIONS`] directories in one
    /// generation it loads everything instead.
    ///
    /// The snapshot is wrong about any other path: its row is not loaded. A
    /// debug build asserts on such a lookup.
    pub fn snapshot_under(&self, dir: &[u8]) -> Result<Snapshot> {
        if dir.is_empty() {
            return self.snapshot();
        }
        let section = self.section()?;
        let env = || Arc::clone(&self.env);
        let Some(store) = self.store_or_late_open()? else {
            return Ok(self.state.read().snapshot(section, env()));
        };
        let generation = store.generation()?;
        {
            let st = self.state.read();
            if generation <= st.loaded_through()
                && let Some(s) = st.snapshot_under(dir, Arc::clone(&section), env())
            {
                return Ok(s);
            }
        }
        self.load_region(store, dir, generation)?;
        let st = self.state.read();
        Ok(match st.snapshot_under(dir, Arc::clone(&section), env()) {
            Some(s) => s,
            // A fold between the install and this read dropped the region:
            // the base alone is right only when it is full.
            None if st.is_full() => st.snapshot(section, env()),
            None => {
                drop(st);
                return self.snapshot_under(dir);
            }
        })
    }

    /// After a glob's walk: the rows committed above the snapshot's
    /// `loaded_through`, through the `files(gen)` index, for the walk to
    /// test against its `Source` paths. Empty when nothing committed since.
    pub fn end_of_walk(&self, snap: &Snapshot) -> Result<Vec<Row>> {
        let Some(store) = self.store_or_late_open()? else {
            return Ok(Vec::new());
        };
        if store.generation()? <= snap.loaded_through() {
            return Ok(Vec::new());
        }
        store.rows_above(snap.loaded_through())
    }

    /// Publish a commit this process made, after it committed, so this process
    /// sees its own write without a reload. Copies only the commit's own
    /// entries, into a persistent overlay; never the base. Runs on
    /// the writer's blocking job: past the fold threshold it folds the
    /// overlay there, inside the single-flight, never on a reader's path.
    pub fn publish(&self, generation: u64, rows: &[Row], tombstones: &[Vec<u8>]) {
        let (cloned, over) = {
            let mut st = self.state.write();
            let cloned = st.publish(generation, rows, tombstones);
            (cloned, st.overlay_len() > self.opts.fold_threshold)
        };
        bump(
            &self.stats.cloned_overlay_entries,
            u64::try_from(cloned).unwrap_or(u64::MAX),
        );
        if over {
            self.fold();
        }
    }

    /// [`Self::publish`] of a [`store::Committed`].
    pub fn publish_committed<T>(&self, c: &store::Committed<T>) {
        self.publish(c.generation, &c.rows, &c.tombstones);
    }

    /// Create this checkout's db if it is still missing, and open it. The
    /// caller holds the import lock; the creation re-checks under it, so an
    /// existing db (another process's) is opened, never replaced.
    pub fn create_db(&self, import: &ImportLockGuard) -> Result<&Store> {
        if let Some(s) = self.store.get() {
            return Ok(s);
        }
        store::create(&self.db_path, import)?;
        self.store_or_late_open()?.with_context(|| {
            format!(
                "the codegen index {} vanished right after it was created",
                self.db_path.display()
            )
        })
    }

    /// The open store, opening a db that appeared since inside the
    /// single-flight, so concurrent callers open it once.
    pub(super) fn open_store(&self) -> Result<Option<&Store>> {
        self.store_or_late_open()
    }

    fn store_or_late_open(&self) -> Result<Option<&Store>> {
        if let Some(s) = self.store.get() {
            return Ok(Some(s));
        }
        if !exists(&self.db_path)? {
            return Ok(None);
        }
        let _flight = self.flight.lock();
        if let Some(s) = self.store.get() {
            return Ok(Some(s));
        }
        self.hook(HookPoint::Open);
        let store = Store::open(&self.db_path, &self.opts.store)?;
        bump(&self.stats.late_opens, 1);
        Ok(Some(self.store.get_or_init(|| store)))
    }

    /// Reload, single-flight: a caller that waited for another's reload
    /// shares it when that one loaded through what this caller saw.
    fn reload(&self, store: &Store, seen: u64) -> Result<()> {
        let _flight = self.flight.lock();
        let st = self.state.read();
        if st.is_full() && st.loaded_through() >= seen {
            return Ok(());
        }
        drop(st);
        self.reload_in_flight(store)
    }

    /// Load and install directory `dir`'s rows, single-flight like a reload;
    /// a caller that waited shares a load that covers `dir` at what it saw.
    fn load_region(&self, store: &Store, dir: &[u8], seen: u64) -> Result<()> {
        let _flight = self.flight.lock();
        let st = self.state.read();
        let current = st.loaded_through() >= seen;
        if current && st.covers(dir) {
            return Ok(());
        }
        let crowded = current && st.region_count() >= MAX_REGIONS;
        drop(st);
        if crowded {
            return self.reload_in_flight(store);
        }
        let (generation, rows) = store.load_under(dir)?;
        bump(&self.stats.region_loads, 1);
        bump(
            &self.stats.rows_loaded,
            u64::try_from(rows.len()).unwrap_or(u64::MAX),
        );
        self.hook(HookPoint::RegionRead);
        let region = Base::from_rows(generation, rows);
        self.state.write().install_region(dir.to_vec(), region);
        Ok(())
    }

    /// The full reload; the caller holds the single-flight.
    fn reload_in_flight(&self, store: &Store) -> Result<()> {
        let (generation, rows) = store.load_all()?;
        bump(&self.stats.reloads, 1);
        bump(
            &self.stats.rows_loaded,
            u64::try_from(rows.len()).unwrap_or(u64::MAX),
        );
        self.hook(HookPoint::ReloadRead);
        let base = Base::from_rows(generation, rows);
        self.state.write().install_reload(base);
        Ok(())
    }

    /// Fold the overlay into a new base, inside the single-flight. A publish
    /// never waits for it: when a reload or another fold holds it, this one
    /// is skipped (a reload keeps only the overlay above its generation, a
    /// fold only what was published after its copy) and the next publish
    /// past the threshold tries again.
    fn fold(&self) {
        let Some(_flight) = self.flight.try_lock() else {
            return;
        };
        let (base, overlay, seq) = {
            let st = self.state.read();
            if st.overlay_len() <= self.opts.fold_threshold {
                return;
            }
            st.fold_copy()
        };
        let folded = State::build_fold(&base, &overlay);
        drop((base, overlay));
        self.hook(HookPoint::FoldCopied);
        self.state.write().install_fold(folded, seq);
        bump(&self.stats.folds, 1);
    }

    /// The compiled section, recompiled when the root `.gitignore` changed.
    pub(super) fn section(&self) -> Result<Arc<Section>> {
        let key = match std::fs::metadata(&self.gitignore) {
            Ok(m) => Some(SectionKey {
                size: m.size(),
                mtime_ns: m
                    .mtime()
                    .saturating_mul(1_000_000_000)
                    .saturating_add(m.mtime_nsec()),
                ino: m.ino(),
            }),
            Err(e) if is_absent(&e) => None,
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", self.gitignore.display()));
            }
        };
        let mut c = self.section.lock();
        let Some(key) = key else {
            if c.key.is_some() {
                *c = SectionCache {
                    key: None,
                    verified_at_ns: 0,
                    bytes: Vec::new(),
                    section: Arc::new(Section::empty()),
                };
            }
            return Ok(Arc::clone(&c.section));
        };
        let slack = i64::try_from(SECTION_RACY_SLACK.as_nanos()).unwrap_or(i64::MAX);
        if c.key == Some(key) && key.mtime_ns < c.verified_at_ns.saturating_sub(slack) {
            return Ok(Arc::clone(&c.section));
        }
        // Changed, or racily clean: read the bytes and compare.
        let now = now_ns();
        let bytes = std::fs::read(&self.gitignore)
            .with_context(|| format!("reading {}", self.gitignore.display()))?;
        bump(&self.stats.section_reads, 1);
        if c.key != Some(key) || bytes != c.bytes {
            let section = Section::compile(&bytes);
            for w in section.warnings() {
                tracing::warn!(file = %self.gitignore.display(), "{w}");
            }
            c.section = Arc::new(section);
            c.bytes = bytes;
            c.key = Some(key);
        }
        c.verified_at_ns = now;
        Ok(Arc::clone(&c.section))
    }

    #[cfg(test)]
    fn set_hook(&self, hook: impl Fn(HookPoint) + Send + Sync + 'static) {
        *self.hook.lock() = Some(Arc::new(hook));
    }

    /// The current state as a snapshot, without the freshness check.
    #[cfg(test)]
    fn peek(&self) -> Snapshot {
        self.state
            .read()
            .snapshot(Arc::new(Section::empty()), Arc::clone(&self.env))
    }

    fn hook(&self, _point: HookPoint) {
        #[cfg(test)]
        {
            let hook = self.hook.lock().clone();
            if let Some(h) = hook {
                h(_point);
            }
        }
    }

    /// Inject a failure at a write-back's crash point (tests only).
    #[cfg(test)]
    pub(super) fn set_fault(
        &self,
        f: impl Fn(super::write::Fault) -> Result<()> + Send + Sync + 'static,
    ) {
        *self.fault.lock() = Some(Arc::new(f));
    }

    /// A write-back passing one of its crash points: an injected failure in
    /// tests, nothing otherwise.
    #[cfg(test)]
    pub(super) fn fault(&self, point: super::write::Fault) -> Result<()> {
        let f = self.fault.lock().clone();
        f.map_or(Ok(()), |f| f(point))
    }

    #[cfg(not(test))]
    pub(super) fn fault(&self, _point: super::write::Fault) -> Result<()> {
        Ok(())
    }
}

/// Whether something is at `path` (one `lstat`); an error other than
/// "nothing there" names the path.
fn exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if is_absent(&e) => Ok(false),
        Err(e) => {
            Err(e).with_context(|| format!("checking for the codegen index {}", path.display()))
        }
    }
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen_index::fs::FileStat;
    use crate::codegen_index::store::{Committed, RowKind};
    use crate::gitignore_section::{BEGIN_MARKER, END_MARKER};
    use std::sync::Barrier;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;
    use std::time::SystemTime;

    struct TestFs;

    impl IndexFs for TestFs {
        fn lstat(&self, path: &Path) -> std::io::Result<FileStat> {
            crate::codegen_index::fs::lstat(path)
        }
        fn read_link(&self, path: &Path) -> std::io::Result<PathBuf> {
            std::fs::read_link(path)
        }
        fn content_hash(&self, _: &Path) -> Result<String> {
            anyhow::bail!("not in these tests")
        }
        fn fresh_hash(&self, _: &Path, _: bool) -> Result<String> {
            anyhow::bail!("not in these tests")
        }
        fn unlink(&self, path: &Path) -> std::io::Result<()> {
            std::fs::remove_file(path)
        }
    }

    struct Ws {
        _dir: tempfile::TempDir,
        root: PathBuf,
        home: PathBuf,
    }

    fn ws() -> Ws {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let home = root.join(".heph");
        Ws {
            _dir: dir,
            root,
            home,
        }
    }

    fn open_opts(w: &Ws, opts: IndexOptions) -> CodegenIndex {
        CodegenIndex::open_with(&w.root, &w.home, Arc::new(TestFs), opts).unwrap()
    }

    fn open(w: &Ws) -> CodegenIndex {
        open_opts(w, IndexOptions::default())
    }

    fn guard() -> ImportLockGuard {
        ImportLockGuard::new(Box::new(()))
    }

    /// An index with its db created, and a second one on the same db: another
    /// process, whose commits are foreign to the first.
    fn pair(w: &Ws, opts: IndexOptions) -> (CodegenIndex, CodegenIndex) {
        let x = open_opts(w, opts);
        x.create_db(&guard()).unwrap();
        (x, open(w))
    }

    fn commit(idx: &CodegenIndex, paths: &[&str]) -> Committed<()> {
        idx.store()
            .unwrap()
            .write(|t| {
                for p in paths {
                    let mut r = Row::new(p.as_bytes().to_vec(), "//p:gen", RowKind::File);
                    r.pending_hash = Some("h".into());
                    t.put(&r)?;
                }
                Ok(())
            })
            .unwrap()
    }

    fn commit_publish(idx: &CodegenIndex, paths: &[&str]) -> u64 {
        let c = commit(idx, paths);
        idx.publish_committed(&c);
        c.generation
    }

    fn has(s: &Snapshot, p: &str) -> bool {
        s.row(p.as_bytes()).is_some()
    }

    #[test]
    fn no_db_reads_empty_and_creates_nothing() {
        let w = ws();
        let x = open(&w);
        let s = x.snapshot().unwrap();
        assert!(s.is_empty());
        assert_eq!(s.loaded_through(), 0);
        assert!(x.end_of_walk(&s).unwrap().is_empty());
        assert!(!x.db_path().exists());
        assert!(!w.home.exists(), "nothing under the home either");
    }

    /// A directory's snapshot loads that directory alone, serves every
    /// directory under it without another load, and a whole snapshot after
    /// it loads everything.
    #[test]
    fn snapshot_under_loads_only_its_dir() {
        let w = ws();
        let (x, y) = pair(&w, IndexOptions::default());
        commit(&y, &["p/a.go", "p/sub/c.go", "q/b.go", "pp/d.go"]);
        let s = x.snapshot_under(b"p").unwrap();
        assert!(has(&s, "p/a.go") && has(&s, "p/sub/c.go"));
        let st = x.stats();
        assert_eq!((st.region_loads, st.reloads, st.rows_loaded), (1, 0, 2));
        assert!(has(&x.snapshot_under(b"p/sub").unwrap(), "p/sub/c.go"));
        assert_eq!(x.stats().region_loads, 1, "under a loaded dir: no load");
        assert!(has(&x.snapshot_under(b"q").unwrap(), "q/b.go"));
        assert_eq!(x.stats().region_loads, 2);
        let all = x.snapshot().unwrap();
        assert_eq!(all.rows().count(), 4);
        assert_eq!(x.stats().reloads, 1, "a whole snapshot loads everything");
        assert!(has(&x.snapshot_under(b"pp").unwrap(), "pp/d.go"));
        assert_eq!(x.stats().region_loads, 2, "the full base covers every dir");
    }

    /// A directory's snapshot sees this process's publishes without a load;
    /// a foreign commit reloads the directory and drops the other loaded
    /// ones, and a publish below a region's read stays out.
    #[test]
    fn snapshot_under_tracks_publishes_and_foreign_commits() {
        let w = ws();
        let (x, y) = pair(&w, IndexOptions::default());
        commit(&y, &["p/a.go", "q/b.go"]);
        x.snapshot_under(b"p").unwrap();
        x.snapshot_under(b"q").unwrap();
        commit_publish(&x, &["p/own.go"]);
        assert!(has(&x.snapshot_under(b"p").unwrap(), "p/own.go"));
        assert_eq!(x.stats().region_loads, 2, "own publish: no load");

        // Committed by x but not yet published, then a foreign delete.
        let late = commit(&x, &["p/late.go"]);
        y.store()
            .unwrap()
            .write(|t| {
                t.delete(b"p/a.go", "//p:gen")?;
                t.delete(b"q/b.go", "//p:gen").map(drop)
            })
            .unwrap();
        let s = x.snapshot_under(b"p").unwrap();
        assert_eq!(x.stats().region_loads, 3, "a foreign commit reloads");
        assert!(!has(&s, "p/a.go") && has(&s, "p/own.go") && has(&s, "p/late.go"));
        x.publish_committed(&late);
        assert!(has(&x.snapshot_under(b"p").unwrap(), "p/late.go"));
        assert!(
            !has(&x.snapshot_under(b"q").unwrap(), "q/b.go"),
            "q, loaded before the foreign commit, is loaded again"
        );
        assert_eq!(x.stats().region_loads, 4);
        assert_eq!(x.stats().reloads, 0);
    }

    /// A fold takes entries out of the overlay: a directory loaded before it
    /// would no longer see them, so it is loaded again.
    #[test]
    fn a_fold_drops_loaded_dirs() {
        let w = ws();
        let (x, _) = pair(
            &w,
            IndexOptions {
                fold_threshold: 2,
                ..IndexOptions::default()
            },
        );
        x.snapshot_under(b"p").unwrap();
        for p in ["p/a.go", "p/b.go", "p/c.go"] {
            commit_publish(&x, &[p]);
        }
        assert_eq!(x.stats().folds, 1);
        let s = x.snapshot_under(b"p").unwrap();
        assert!(["p/a.go", "p/b.go", "p/c.go"].iter().all(|p| has(&s, p)));
        assert_eq!(x.stats().region_loads, 2);
    }

    #[test]
    fn many_dirs_fall_back_to_a_full_load() {
        let w = ws();
        let (x, y) = pair(&w, IndexOptions::default());
        let dirs: Vec<String> = (0..=MAX_REGIONS).map(|i| format!("d{i}")).collect();
        let paths: Vec<String> = dirs.iter().map(|d| format!("{d}/a.go")).collect();
        commit(&y, &paths.iter().map(String::as_str).collect::<Vec<_>>());
        for d in &dirs {
            let s = x.snapshot_under(d.as_bytes()).unwrap();
            assert!(has(&s, &format!("{d}/a.go")));
        }
        let st = x.stats();
        assert_eq!((st.region_loads, st.reloads), (MAX_REGIONS as u64, 1));
    }

    #[test]
    fn region_load_is_single_flight() {
        let w = ws();
        let (x, y) = pair(&w, IndexOptions::default());
        commit(&y, &["p/a.go"]);
        x.set_hook(|p| {
            if p == HookPoint::RegionRead {
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let barrier = Barrier::new(8);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    barrier.wait();
                    assert!(has(&x.snapshot_under(b"p").unwrap(), "p/a.go"));
                });
            }
        });
        assert_eq!(x.stats().region_loads, 1, "{:?}", x.stats());
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "q/b.go looked up in a snapshot of p only")]
    fn a_dir_snapshot_refuses_a_path_outside_it() {
        let w = ws();
        let (x, y) = pair(&w, IndexOptions::default());
        commit(&y, &["p/a.go", "q/b.go"]);
        x.snapshot_under(b"p").unwrap().row(b"q/b.go");
    }

    #[test]
    fn snapshot_reload_is_single_flight() {
        let w = ws();
        let (x, y) = pair(&w, IndexOptions::default());
        x.snapshot().unwrap();
        commit(&y, &["p/a.go"]);
        x.set_hook(|p| {
            if p == HookPoint::ReloadRead {
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let before = x.stats().reloads;
        let barrier = Barrier::new(8);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    barrier.wait();
                    assert!(has(&x.snapshot().unwrap(), "p/a.go"));
                });
            }
        });
        assert_eq!(x.stats().reloads - before, 1, "{:?}", x.stats());
    }

    #[test]
    fn own_publish_does_not_skip_foreign_commit() {
        let w = ws();
        let (x, y) = pair(&w, IndexOptions::default());
        for _ in 0..8 {
            commit_publish(&x, &[]);
        }
        let at8 = x.snapshot().unwrap();
        assert_eq!(at8.loaded_through(), 8);
        assert_eq!(commit(&y, &["p/a.go"]).generation, 9);
        assert_eq!(commit_publish(&x, &["q/b.go"]), 10);
        assert_eq!(x.peek().loaded_through(), 8, "stops below the foreign 9");

        let recheck = x.end_of_walk(&at8).unwrap();
        assert!(recheck.iter().any(|r| r.path == b"p/a.go"), "{recheck:?}");

        let next = x.snapshot().unwrap();
        assert_eq!(next.loaded_through(), 10);
        assert!(next.row(b"p/a.go").unwrap().pending_hash.is_some());
        assert!(has(&next, "q/b.go"));
    }

    #[test]
    fn reload_merges_overlay_rows_above_its_generation() {
        let w = ws();
        let (x, y) = pair(&w, IndexOptions::default());
        let x = Arc::new(x);
        commit(&y, &["p/foreign.go"]);
        let once = AtomicBool::new(false);
        let weak = Arc::downgrade(&x);
        x.set_hook(move |p| {
            if p == HookPoint::ReloadRead && !once.swap(true, Ordering::SeqCst) {
                // A publish lands after the reload read the rows.
                let x = weak.upgrade().unwrap();
                assert_eq!(commit_publish(&x, &["p/own.go"]), 2);
            }
        });
        let s = x.snapshot().unwrap();
        assert!(has(&s, "p/foreign.go"));
        assert!(
            has(&x.peek(), "p/own.go"),
            "the publish survived the install"
        );
        assert_eq!(x.peek().loaded_through(), 2);
        assert_eq!(x.stats().reloads, 1);
        let reloads = x.stats().reloads;
        x.snapshot().unwrap();
        assert_eq!(x.stats().reloads, reloads, "nothing left to load");
    }

    #[test]
    fn publish_copies_overlay_not_base() {
        let w = ws();
        let (x, y) = pair(&w, IndexOptions::default());
        let many: Vec<String> = (0..100).map(|i| format!("p/{i}.go")).collect();
        commit(&y, &many.iter().map(String::as_str).collect::<Vec<_>>());
        x.snapshot().unwrap();
        commit_publish(&x, &["a"]);
        commit_publish(&x, &["b", "c"]);
        commit_publish(&x, &["d"]);
        // Each publish copies its own entries: never the 100-row base, and
        // never the overlay the earlier publishes built.
        assert_eq!(x.stats().cloned_overlay_entries, 4);
        assert_eq!(x.snapshot().unwrap().rows().count(), 104);
        assert_eq!(x.stats().reloads, 1);
    }

    /// Decision 22's bench bar at the product threshold: a run of
    /// write-backs, one of them thousands of outputs wide, copies exactly
    /// each publish's rows (not the overlay, which grows to the fold), and
    /// the overlay folds once, when it passes the threshold.
    #[test]
    fn publish_cost_is_its_own_rows_at_the_default_threshold() {
        let w = ws();
        let (x, _y) = pair(&w, IndexOptions::default());
        x.snapshot().unwrap();
        let reloads = x.stats().reloads;
        let publish = |target: usize, n: usize| -> usize {
            let paths: Vec<String> = (0..n).map(|i| format!("t{target}/{i}.go")).collect();
            let before = x.stats().cloned_overlay_entries;
            commit_publish(&x, &paths.iter().map(String::as_str).collect::<Vec<_>>());
            assert_eq!(
                x.stats().cloned_overlay_entries - before,
                n as u64,
                "publish of target {target}: its {n} rows, nothing else"
            );
            n
        };
        // One write-back of several thousand outputs, under the threshold.
        let mut published = publish(0, 3000);
        assert_eq!(x.stats().folds, 0);
        // Many small ones, until the overlay passes the threshold.
        let mut target = 1;
        while published <= FOLD_THRESHOLD {
            assert_eq!(x.stats().folds, 0, "no fold at {published} rows");
            published += publish(target, 37);
            target += 1;
        }
        assert_eq!(x.stats().folds, 1, "the fold fired at {published} rows");
        assert_eq!(
            x.state.read().overlay_len(),
            0,
            "the fold took the whole overlay"
        );
        // Past the fold, a publish still costs its own rows.
        for _ in 0..10 {
            published += publish(target, 37);
            target += 1;
        }
        assert_eq!(x.stats().folds, 1);
        assert_eq!(x.snapshot().unwrap().rows().count(), published);
        assert_eq!(x.stats().reloads, reloads, "publishes never reload");
    }

    #[test]
    fn fold_during_reload_keeps_published_rows() {
        let w = ws();
        let opts = IndexOptions {
            fold_threshold: 2,
            ..IndexOptions::default()
        };
        let (x, y) = pair(&w, opts);
        let x = Arc::new(x);
        commit_publish(&x, &["r1"]);
        commit_publish(&x, &["r2"]);
        let (copied_tx, copied_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let go_rx = Mutex::new(go_rx);
        x.set_hook(move |p| {
            if p == HookPoint::FoldCopied {
                copied_tx.send(()).unwrap();
                go_rx.lock().recv().unwrap();
            }
        });
        // r3 passes the threshold: its publish folds, and pauses after the copy.
        let folder = std::thread::spawn({
            let x = Arc::clone(&x);
            move || commit_publish(&x, &["r3"])
        });
        copied_rx.recv().unwrap();
        // A foreign commit, and a reload waiting on the single-flight.
        commit(&y, &["f"]);
        let reader = std::thread::spawn({
            let x = Arc::clone(&x);
            move || x.snapshot().unwrap()
        });
        std::thread::sleep(Duration::from_millis(50));
        // Published after the fold's copy, before its install.
        commit_publish(&x, &["r4"]);
        go_tx.send(()).unwrap();
        folder.join().unwrap();
        let read = reader.join().unwrap();

        assert_eq!(x.stats().folds, 1);
        for p in ["r1", "r2", "r3", "r4"] {
            assert!(has(&x.peek(), p), "{p} lost by the fold");
            assert!(has(&read, p), "{p} missing from the reload");
        }
        assert!(has(&read, "f"));
        assert_eq!(x.snapshot().unwrap().loaded_through(), 5);
    }

    #[test]
    fn out_of_order_publishes_advance_without_reload() {
        let w = ws();
        let (x, _y) = pair(&w, IndexOptions::default());
        x.snapshot().unwrap();
        let reloads = x.stats().reloads;
        let c9 = commit(&x, &["a"]);
        let c10 = commit(&x, &["b"]);
        x.publish_committed(&c10);
        assert_eq!(x.peek().loaded_through(), 0);
        x.publish_committed(&c9);
        assert_eq!(x.peek().loaded_through(), c10.generation);
        let s = x.snapshot().unwrap();
        assert!(has(&s, "a") && has(&s, "b"));
        assert_eq!(x.stats().reloads, reloads, "no row load");
    }

    #[test]
    fn late_db_open_shares_reload_single_flight() {
        let w = ws();
        let x = open(&w);
        assert!(x.snapshot().unwrap().is_empty());
        let y = open(&w);
        y.create_db(&guard()).unwrap();
        commit(&y, &["p/a.go"]);
        x.set_hook(|p| {
            if p == HookPoint::Open {
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let barrier = Barrier::new(8);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    barrier.wait();
                    assert!(has(&x.snapshot().unwrap(), "p/a.go"));
                });
            }
        });
        assert_eq!(x.stats().late_opens, 1);
        assert_eq!(x.stats().reloads, 1);
    }

    #[test]
    fn end_of_walk_recheck_tests_only_new_rows() {
        let w = ws();
        let (x, y) = pair(&w, IndexOptions::default());
        commit(&y, &["old"]);
        let s = x.snapshot().unwrap();
        assert!(x.end_of_walk(&s).unwrap().is_empty());
        commit(&y, &["new"]);
        let rows = x.end_of_walk(&s).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, b"new");
    }

    fn write_section(root: &Path, line: &str) {
        std::fs::write(
            root.join(".gitignore"),
            format!("{BEGIN_MARKER}\n{line}\n{END_MARKER}\n"),
        )
        .unwrap();
    }

    fn territory(s: &Snapshot, p: &str) -> bool {
        s.section().match_point(p.as_bytes(), false).is_territory()
    }

    #[test]
    fn gitignore_same_size_rewrite_in_same_tick_recompiles() {
        let w = ws();
        let x = open(&w);
        assert!(!x.snapshot().unwrap().section().is_present());
        write_section(&w.root, "/a");
        let s = x.snapshot().unwrap();
        assert!(territory(&s, "a") && !territory(&s, "b"));

        // Same size, same inode, same mtime: only the bytes differ.
        let gi = w.root.join(".gitignore");
        let before = std::fs::metadata(&gi).unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(&gi).unwrap();
        std::io::Write::write_all(&mut &f, format!("{BEGIN_MARKER}\n/b\n").as_bytes()).unwrap();
        f.set_modified(before.modified().unwrap()).unwrap();
        drop(f);
        let after = std::fs::metadata(&gi).unwrap();
        assert_eq!(
            (after.size(), after.ino(), after.modified().unwrap()),
            (before.size(), before.ino(), before.modified().unwrap())
        );
        let s = x.snapshot().unwrap();
        assert!(territory(&s, "b") && !territory(&s, "a"), "recompiled");

        // Out of the racy window, an unchanged key costs no read.
        let old = SystemTime::now() - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&gi)
            .unwrap()
            .set_modified(old)
            .unwrap();
        x.snapshot().unwrap();
        let reads = x.stats().section_reads;
        x.snapshot().unwrap();
        x.snapshot().unwrap();
        assert_eq!(x.stats().section_reads, reads);

        std::fs::remove_file(&gi).unwrap();
        assert!(!x.snapshot().unwrap().section().is_present());
    }

    #[test]
    fn unreadable_gitignore_errors_naming_file() {
        let w = ws();
        std::fs::create_dir(w.root.join(".gitignore")).unwrap();
        let err = open(&w).snapshot().unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&w.root.join(".gitignore").display().to_string()),
            "{msg}"
        );
    }

    #[test]
    fn corrupt_index_errors_naming_file() {
        let w = ws();
        let x = open(&w);
        x.snapshot().unwrap();
        // A db that appears corrupt is an error, at open and at a late open.
        std::fs::create_dir_all(x.db_path().parent().unwrap()).unwrap();
        std::fs::write(x.db_path(), vec![0x5a_u8; 4096]).unwrap();
        let db = x.db_path().display().to_string();
        let start = std::time::Instant::now();
        let late = format!("{:#}", x.snapshot().unwrap_err());
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "a corrupt db must error at once, not after the pool's retries: {:?}",
            start.elapsed()
        );
        assert!(late.contains(&db), "{late}");
        let opened = format!(
            "{:#}",
            CodegenIndex::open(&w.root, &w.home, Arc::new(TestFs)).unwrap_err()
        );
        assert!(opened.contains(&db), "{opened}");
    }

    #[test]
    fn shared_home_keys_db_by_root() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let (r1, r2) = (dir.path().join("r1"), dir.path().join("r2"));
        std::fs::create_dir_all(&r1).unwrap();
        std::fs::create_dir_all(&r2).unwrap();
        let a = CodegenIndex::open(&r1, &home, Arc::new(TestFs)).unwrap();
        let b = CodegenIndex::open(&r2, &home, Arc::new(TestFs)).unwrap();
        assert_ne!(a.db_path(), b.db_path());
        a.create_db(&guard()).unwrap();
        commit_publish(&a, &["x"]);
        assert!(b.snapshot().unwrap().is_empty(), "a's db is not b's");
        assert!(!b.db_path().exists());
        b.create_db(&guard()).unwrap();
        assert!(b.snapshot().unwrap().is_empty());
        assert!(
            has(&a.snapshot().unwrap(), "x"),
            "b's creation left a's db alone"
        );
    }

    #[test]
    fn create_db_opens_a_db_another_process_created() {
        let w = ws();
        let x = open(&w);
        let y = open(&w);
        y.create_db(&guard()).unwrap();
        commit(&y, &["kept"]);
        x.create_db(&guard()).unwrap();
        assert!(has(&x.snapshot().unwrap(), "kept"), "not replaced");
    }
}
