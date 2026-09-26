//! Which tree paths are *generated* rather than *source*.
//!
//! A `codegen = "copy"` target writes a net-new file into the workspace tree.
//! Nothing may source that file as raw input — not a `glob()`, not a `file()`,
//! not a provider's own directory scan — because its content already enters the
//! graph through its generator. Sourcing it again double-sources the bytes and,
//! when the generator's own inputs glob the same directory, feeds a target's
//! output back into its input.
//!
//! # Why not a mark on the file
//!
//! This was previously a `user.heph.codegen` extended attribute stamped on each
//! written-back file. The stamp was written at the instant the file was created,
//! so there was never a moment where the file existed unmarked — that part was
//! right, and it is the bar any replacement has to clear.
//!
//! What was wrong is that xattrs are **inode-scoped**, and the dominant way tools
//! rewrite a file is write-temp-then-`rename(2)` — a new inode. So `gofmt -w`,
//! `prettier`, `sed -i`, every editor save, `git checkout`, `cp` without `-p`,
//! `tar`/`rsync` without their xattr flags, and any filesystem without xattr
//! support all silently erased the mark. A file that lost it looked like source
//! on the very next walk, and nothing re-stamped it in time: a target that merely
//! globs a generated file has no dependency edge on its generator, so there is no
//! ordering guarantee that the write-back runs first.
//!
//! # Where the claims live
//!
//! In heph's own store: a `codegen_claims` table in
//! `<home>/cache/codegen-claims.db`, written by the codegen write-back in the
//! same operation that puts the file on disk. Nothing is attached to the file, so
//! no tool that rewrites it can erase the claim, and no user action is required —
//! a target that generates into the tree is claimed the moment it does so.
//!
//! Deliberately NOT the heph-managed `.gitignore` section, which carries the same
//! paths. That section exists to tell **git** to ignore build outputs; it is a
//! file users own and edit, and deriving build-input classification from it would
//! mean a hand-edit silently changes what heph treats as source. Its failure
//! direction is the bad one, too: a stale section over-claims, and an
//! over-claimed path hides a real source file from every glob with no diagnostic.
//!
//! ## Why a table and not a file
//!
//! The first cut was an append-only text log with last-wins-per-addr on read. It
//! could not represent a target with two outputs: with one `# //addr` header per
//! pattern, `# //a`/`x`/`# //a`/`y` is genuinely ambiguous between "one target,
//! two patterns" and "two blocks, the second superseding the first", and the
//! reader chose the latter — so a protobuf target emitting `foo.pb.go` and
//! `foo_grpc.pb.go` silently kept one claim and let the other be sourced as raw
//! input. A keyed row makes that unrepresentable rather than merely fixed.
//!
//! The rest follows from the same choice: an upsert is atomic across processes,
//! so concurrent generators cannot drop each other's claims; `PRAGMA data_version`
//! is an exact change signal, so there is no mtime granularity to reason about;
//! and a torn write rolls back instead of leaving a half-line that supersedes a
//! good record.
//!
//! # Claim kinds
//!
//! A claim carries the shape it was declared with, and each shape is matched by
//! the cheapest thing that can match it:
//!
//! - [`ClaimKind::File`] — an exact path, matched by hash lookup. Never compiled
//!   as a glob: a literal output path may legitimately contain `[`, `{` or `?`,
//!   and compiling it would both fail to claim the real file and silently claim
//!   whatever the accidental pattern happened to match.
//! - [`ClaimKind::Dir`] — a directory and its subtree, matched by prefix.
//! - [`ClaimKind::Glob`] — the only shape that reaches the glob engine.
//!
//! That split is also what keeps the per-file cost flat. Compiling every claim
//! into one `wax::Any` — and especially adding a `…/**` form per claim — walks the
//! union off the regex crate's lazy-DFA cache somewhere past a few hundred
//! claims, at which point a match costs microseconds instead of nanoseconds. Only
//! genuine glob outputs pay that, and there are few of them.
//!
//! # Releasing a claim
//!
//! A target whose `out` moved releases the old path the next time it generates:
//! [`CodegenClaims::record`] replaces that target's row. A target *deleted* from
//! the tree never generates again, so nothing would ever release its claims — and
//! a claim that outlives its target silently hides a real source file at that
//! path. [`CodegenClaims::rewrite`] reconciles against the live set, driven by
//! `heph tool gen-gitignore` (the command that already resolves every codegen
//! target); `heph validate` reports the discrepancy without repairing it.

use anyhow::{Context as _, Result};
use parking_lot::{Mutex, RwLock};
use rusqlite::{Connection, OpenFlags};
use rustc_hash::FxHashMap;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wax::{Any, Glob, Program as _};

/// How a claimed path was declared. Decides how it is matched, and — for
/// [`ClaimKind::File`] — that it is never handed to the glob engine at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimKind {
    /// An exact workspace-relative path.
    File,
    /// A directory: it and everything beneath it.
    Dir,
    /// A glob pattern over workspace-relative paths.
    Glob,
}

impl ClaimKind {
    fn tag(self) -> char {
        match self {
            Self::File => 'f',
            Self::Dir => 'd',
            Self::Glob => 'g',
        }
    }

    fn from_tag(c: char) -> Option<Self> {
        match c {
            'f' => Some(Self::File),
            'd' => Some(Self::Dir),
            'g' => Some(Self::Glob),
            _ => None,
        }
    }
}

/// One `codegen = "copy"` output path, as declared.
///
/// `path` is workspace-relative with no leading separator — the same shape the
/// glob walk and the provider scans match against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub kind: ClaimKind,
    pub path: String,
}

impl Claim {
    pub fn file(path: impl Into<String>) -> Self {
        Self {
            kind: ClaimKind::File,
            path: normalize(path.into()),
        }
    }

    pub fn dir(path: impl Into<String>) -> Self {
        Self {
            kind: ClaimKind::Dir,
            path: normalize(path.into()),
        }
    }

    pub fn glob(path: impl Into<String>) -> Self {
        Self {
            kind: ClaimKind::Glob,
            path: normalize(path.into()),
        }
    }

    fn encode(&self) -> String {
        format!("{} {}", self.kind.tag(), self.path)
    }

    fn decode(line: &str) -> Option<Self> {
        let mut chars = line.chars();
        let kind = ClaimKind::from_tag(chars.next()?)?;
        Some(Self {
            kind,
            path: line.get(2..)?.to_owned(),
        })
    }
}

/// Workspace-relative, no leading or trailing separator.
fn normalize(p: String) -> String {
    p.trim_start_matches('/').trim_end_matches('/').to_owned()
}

/// An immutable, compiled set of claimed paths — the answer to "is this path
/// generated?" at one point in time.
///
/// Handed out by [`CodegenClaims::snapshot`] so a walk matches thousands of paths
/// against a fixed set without touching a lock or the store per file.
#[derive(Debug, Default)]
pub struct ClaimSet {
    /// Exact paths → owning addr.
    files: FxHashMap<Box<str>, Box<str>>,
    /// Directory roots → owning addr. Small; a linear prefix scan beats hashing
    /// every ancestor of every walked path.
    dirs: Vec<(Box<str>, Box<str>)>,
    /// Genuine glob claims → owning addr, and their union for the fast path.
    globs: Vec<(Glob<'static>, Box<str>)>,
    any: Option<Any<'static>>,
}

impl ClaimSet {
    /// Claims nothing.
    pub fn empty() -> Self {
        Self::default()
    }

    /// True when nothing is claimed — the whole check can be skipped.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.dirs.is_empty() && self.globs.is_empty()
    }

    /// True if the workspace-relative path `rel` is generated by a
    /// `codegen = "copy"` target, and so must never be sourced as raw input.
    pub fn claims(&self, rel: &Path) -> bool {
        self.lookup(rel).is_some()
    }

    /// The addr of the target that emits `rel`, or `None` if it is not claimed.
    pub fn owner(&self, rel: &Path) -> Option<&str> {
        self.lookup(rel)
    }

    fn lookup(&self, rel: &Path) -> Option<&str> {
        if self.is_empty() {
            return None;
        }
        // A claimed path is always valid UTF-8 (it came from a declared `out`),
        // so a path that is not cannot be claimed. `None` is also the safe
        // direction here: it treats the file as source rather than swallowing it.
        let rel_str = rel.to_str()?;
        if let Some(owner) = self.files.get(rel_str) {
            return Some(owner);
        }
        for (dir, owner) in &self.dirs {
            let under = rel_str.len() > dir.len()
                && rel_str.as_bytes().get(dir.len()) == Some(&b'/')
                && rel_str.starts_with(&**dir);
            if rel_str == &**dir || under {
                return Some(owner);
            }
        }
        // The union answers "any glob at all?" in one pass; only then do we pay a
        // linear scan to name the owner.
        if self.any.as_ref().is_some_and(|a| a.is_match(rel)) {
            return self
                .globs
                .iter()
                .find(|(g, _)| g.is_match(rel))
                .map(|(_, o)| &**o);
        }
        None
    }

    /// Compile `entries` (addr → its claims) into a matchable set.
    ///
    /// A `File` or `Dir` claim cannot fail. A malformed `Glob` claim fails
    /// *loudly and alone*: it names its target and does not take the rest of the
    /// workspace's claims down with it, because "nothing is generated" is exactly
    /// the wrong answer to reach by accident.
    pub fn compile(entries: &BTreeMap<String, Vec<Claim>>) -> Result<Self> {
        let mut set = Self::default();
        let mut globs = Vec::new();
        for (addr, claims) in entries {
            for claim in claims {
                let owner: Box<str> = addr.as_str().into();
                match claim.kind {
                    ClaimKind::File => {
                        set.files.insert(claim.path.as_str().into(), owner);
                    }
                    ClaimKind::Dir => set.dirs.push((claim.path.as_str().into(), owner)),
                    ClaimKind::Glob => {
                        let glob =
                            Glob::new(&claim.path)
                                .map(Glob::into_owned)
                                .with_context(|| {
                                    format!(
                                        "invalid codegen output pattern '{}' declared by {addr}",
                                        claim.path
                                    )
                                })?;
                        globs.push(glob.clone());
                        set.globs.push((glob, owner));
                    }
                }
            }
        }
        if !globs.is_empty() {
            set.any = Some(wax::any(globs).context("compiling codegen output patterns")?);
        }
        Ok(set)
    }
}

/// The live set of workspace paths owned by `codegen = "copy"` targets.
///
/// Built once at engine construction and handed to every plugin that walks the
/// tree (a cdylib plugin opens the same store itself). Shared by `Arc`;
/// [`Self::snapshot`] is the read path and [`Self::record`] the write one.
#[derive(Debug)]
pub struct CodegenClaims {
    store: Option<Store>,
    state: RwLock<State>,
}

#[derive(Debug)]
struct State {
    set: Arc<ClaimSet>,
    /// The `PRAGMA data_version` the set was built from. `None` until first read.
    seen: Option<i64>,
}

impl Default for CodegenClaims {
    fn default() -> Self {
        Self::disabled()
    }
}

impl CodegenClaims {
    /// A claim set with no store behind it: claims nothing, reads nothing,
    /// records nothing. For the non-engine call sites (LSP, unit tests).
    pub fn disabled() -> Self {
        Self {
            store: None,
            state: RwLock::new(State {
                set: Arc::new(ClaimSet::empty()),
                seen: None,
            }),
        }
    }

    /// Open (or create) the claim store at `db_path`.
    ///
    /// Fails loudly. The previous design degraded an unreadable store to "nothing
    /// is claimed", which is the one answer guaranteed to be wrong: every
    /// generated file in the workspace re-enters the graph as source, and where a
    /// generator's inputs glob its own output directory, its output feeds its
    /// input. A build that refuses to start is recoverable; a silently wrong one
    /// is not.
    pub fn open(db_path: PathBuf) -> Result<Self> {
        let store = Store::open(&db_path)
            .with_context(|| format!("opening the codegen claim store at {db_path:?}"))?;
        let this = Self {
            store: Some(store),
            state: RwLock::new(State {
                set: Arc::new(ClaimSet::empty()),
                seen: None,
            }),
        };
        this.refresh()?;
        Ok(this)
    }

    /// The current claim set, re-reading the store if another connection has
    /// committed since the last look.
    ///
    /// Call once per walk or scan and match every path against the returned set:
    /// the freshness check is one `PRAGMA data_version`, cheap but not free
    /// enough to run per file, and a walk wants a fixed answer for its duration.
    ///
    /// A failure to refresh keeps serving the last known good set rather than
    /// downgrading to empty — never answer "nothing is generated" by accident.
    pub fn snapshot(&self) -> Arc<ClaimSet> {
        match self.refresh() {
            Ok(set) => set,
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "cannot refresh codegen claims; using the last known set"
                );
                Arc::clone(&self.state.read().set)
            }
        }
    }

    fn refresh(&self) -> Result<Arc<ClaimSet>> {
        let Some(store) = self.store.as_ref() else {
            return Ok(Arc::clone(&self.state.read().set));
        };
        let version = store.data_version()?;
        {
            let state = self.state.read();
            if state.seen == Some(version) {
                return Ok(Arc::clone(&state.set));
            }
        }
        // `load` reads the version alongside the rows, so the set is never
        // labelled with a version older than the data it holds.
        let (version, entries) = store.load()?;
        let set = Arc::new(ClaimSet::compile(&entries)?);
        let mut state = self.state.write();
        state.set = Arc::clone(&set);
        state.seen = Some(version);
        Ok(set)
    }

    /// The store's current contents: `addr -> claims`. For a caller reconciling
    /// against the live set of targets.
    pub fn entries(&self) -> Result<BTreeMap<String, Vec<Claim>>> {
        match self.store.as_ref() {
            Some(store) => Ok(store.load()?.1),
            None => Ok(BTreeMap::new()),
        }
    }

    /// Register `claims` as the `codegen = "copy"` output of `addr`, replacing
    /// whatever that target claimed before.
    ///
    /// Called by the codegen write-back in the same operation that puts the files
    /// on disk, so a generated file is never on disk unclaimed. An upsert, so two
    /// processes generating into one workspace cannot drop each other's rows —
    /// losing a claim for a file already on disk is the failure this exists to
    /// prevent. Recording exactly what is already stored writes nothing.
    pub fn record(&self, addr: &str, claims: &[Claim]) -> Result<()> {
        let Some(store) = self.store.as_ref() else {
            return Ok(());
        };
        if store.upsert(addr, claims)? {
            self.refresh()?;
        }
        Ok(())
    }

    /// Replace the whole store with exactly `entries`.
    ///
    /// [`Self::record`] can only ever add a target's claims or update them in
    /// place — it runs when a target generates, and a target deleted from the
    /// tree never runs again. Its claims would otherwise outlive it forever, and
    /// a stale claim silently hides a real source file at that path. So the full
    /// set has to be reconciled against the live one by a caller that has just
    /// resolved every target: `heph tool gen-gitignore`.
    pub fn rewrite(&self, entries: &BTreeMap<String, Vec<Claim>>) -> Result<()> {
        let Some(store) = self.store.as_ref() else {
            return Ok(());
        };
        store.replace_all(entries)?;
        self.refresh()?;
        Ok(())
    }
}

/// The sqlite-backed claim store.
#[derive(Debug)]
struct Store {
    /// A SINGLE read connection, not a pool. `PRAGMA data_version` is a
    /// per-connection counter — it reports whether *this* connection has seen
    /// another one commit — so a version read on one connection compared against
    /// a version read on another is meaningless, and silently answers "unchanged"
    /// for a store that did change. The read path runs once per walk, not once
    /// per file, so serialising it costs nothing.
    read: Mutex<Connection>,
    write: Mutex<Connection>,
}

impl Store {
    fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating codegen claim dir {parent:?}"))?;
        }
        let write = Connection::open(db_path)
            .with_context(|| format!("opening codegen claim db at {db_path:?}"))?;
        // Unlike the fswalk cache next door, these rows are NOT reconstructable
        // from disk — a lost claim means a generated file is sourced as input. So
        // `synchronous = NORMAL` rather than `OFF`: the rows are small and written
        // rarely, and durability is worth more than the fsync here.
        write
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA busy_timeout = 10000;
                 PRAGMA synchronous = NORMAL;
                 CREATE TABLE IF NOT EXISTS codegen_claims (
                     addr     TEXT PRIMARY KEY,
                     patterns TEXT NOT NULL
                 ) STRICT;",
            )
            .context("initialising the codegen claim schema")?;

        let read = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening codegen claim db for reading at {db_path:?}"))?;
        read.execute_batch("PRAGMA busy_timeout = 10000;")
            .context("configuring the codegen claim read connection")?;

        Ok(Self {
            read: Mutex::new(read),
            write: Mutex::new(write),
        })
    }

    /// Changes whenever *another* connection commits — an exact signal, with no
    /// filesystem timestamp granularity to reason about.
    fn data_version(&self) -> Result<i64> {
        let conn = self.read.lock();
        conn.query_row("PRAGMA data_version", [], |r| r.get(0))
            .context("reading the codegen claim data_version")
    }

    fn load(&self) -> Result<(i64, BTreeMap<String, Vec<Claim>>)> {
        let conn = self.read.lock();
        let version: i64 = conn
            .query_row("PRAGMA data_version", [], |r| r.get(0))
            .context("reading the codegen claim data_version")?;
        let mut stmt = conn
            .prepare_cached("SELECT addr, patterns FROM codegen_claims")
            .context("preparing the codegen claim query")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .context("querying codegen claims")?;
        let mut out: BTreeMap<String, Vec<Claim>> = BTreeMap::new();
        for row in rows {
            let (addr, patterns) = row.context("reading a codegen claim row")?;
            let claims: Vec<Claim> = patterns.lines().filter_map(Claim::decode).collect();
            if !claims.is_empty() {
                out.insert(addr, claims);
            }
        }
        Ok((version, out))
    }

    /// Returns whether anything actually changed.
    fn upsert(&self, addr: &str, claims: &[Claim]) -> Result<bool> {
        let conn = self.write.lock();
        if claims.is_empty() {
            let n = conn
                .execute("DELETE FROM codegen_claims WHERE addr = ?1", [addr])
                .context("releasing codegen claims")?;
            return Ok(n > 0);
        }
        // The `WHERE` makes "record exactly what is already stored" a no-op at the
        // storage layer, so the steady state writes nothing at all.
        let n = conn
            .execute(
                "INSERT INTO codegen_claims (addr, patterns) VALUES (?1, ?2)
                 ON CONFLICT(addr) DO UPDATE SET patterns = excluded.patterns
                 WHERE patterns <> excluded.patterns",
                rusqlite::params![addr, encode(claims)],
            )
            .context("recording codegen claims")?;
        Ok(n > 0)
    }

    fn replace_all(&self, entries: &BTreeMap<String, Vec<Claim>>) -> Result<()> {
        let mut conn = self.write.lock();
        let tx = conn
            .transaction()
            .context("opening the codegen claim reconcile transaction")?;
        tx.execute("DELETE FROM codegen_claims", [])
            .context("clearing codegen claims")?;
        {
            let mut stmt = tx
                .prepare("INSERT INTO codegen_claims (addr, patterns) VALUES (?1, ?2)")
                .context("preparing the codegen claim insert")?;
            for (addr, claims) in entries {
                if claims.is_empty() {
                    continue;
                }
                stmt.execute(rusqlite::params![addr, encode(claims)])
                    .context("inserting a codegen claim")?;
            }
        }
        tx.commit()
            .context("committing the codegen claim reconcile")
    }
}

/// Sorted and deduplicated so that "the same declaration" has exactly one
/// encoding, whoever writes it — otherwise a differently-ordered `out` would
/// never reach the no-op steady state and would rewrite the row on every run.
fn encode(claims: &[Claim]) -> String {
    let mut lines: Vec<String> = claims.iter().map(Claim::encode).collect();
    lines.sort_unstable();
    lines.dedup();
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> &Path {
        Path::new(s)
    }

    /// An empty workspace store, ready for `record`.
    fn workspace() -> (tempfile::TempDir, CodegenClaims) {
        let dir = tempfile::tempdir().expect("tempdir");
        let claims = CodegenClaims::open(dir.path().join("codegen-claims.db")).expect("open");
        (dir, claims)
    }

    fn claiming(claims: &[Claim]) -> (tempfile::TempDir, CodegenClaims) {
        let (dir, store) = workspace();
        store.record("//pkg:gen", claims).expect("record");
        (dir, store)
    }

    // ─── The defects this shape makes unrepresentable ──────────────────────

    /// A target with MORE THAN ONE copy output must keep all of them. Protobuf is
    /// the everyday case: one target emitting `foo.pb.go` and `foo_grpc.pb.go`.
    ///
    /// The append-log format this replaced could not express it — one `# addr`
    /// header per pattern, with the reader superseding on every header, meant the
    /// first output was silently sourced as raw input.
    #[test]
    fn a_target_with_several_outputs_keeps_every_claim() {
        let (_d, c) = claiming(&[
            Claim::file("/pkg/foo.pb.go"),
            Claim::file("/pkg/foo_grpc.pb.go"),
        ]);
        let s = c.snapshot();
        assert!(s.claims(p("pkg/foo.pb.go")), "first output lost");
        assert!(s.claims(p("pkg/foo_grpc.pb.go")), "second output lost");
    }

    /// A literal output path may contain glob metacharacters. Compiling it as a
    /// pattern gets it wrong in *both* directions at once: the real generated
    /// file goes unclaimed, and a hand-written file the accidental pattern
    /// happens to match is silently hidden from every glob.
    #[test]
    fn a_literal_path_with_glob_metacharacters_is_matched_literally() {
        let (_d, c) = claiming(&[Claim::file("/pkg/data[1].go")]);
        let s = c.snapshot();
        assert!(s.claims(p("pkg/data[1].go")), "the real generated file");
        assert!(
            !s.claims(p("pkg/data1.go")),
            "a hand-written file must not be swallowed by an accidental character class"
        );
    }

    /// One target's unusable glob must not take the workspace's claims with it.
    #[test]
    fn a_bad_glob_claim_fails_loudly_and_alone() {
        let mut entries = BTreeMap::new();
        entries.insert("//pkg:ok".to_string(), vec![Claim::file("/pkg/ok.go")]);
        entries.insert("//pkg:bad".to_string(), vec![Claim::glob("/pkg/<bad")]);
        let err = ClaimSet::compile(&entries).expect_err("must not compile silently");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("//pkg:bad"),
            "the error names its target: {msg}"
        );
    }

    /// Concurrent `record`s from separate handles — the shape two `heph`
    /// processes generating into one workspace have — must all survive.
    #[test]
    fn concurrent_records_all_survive() {
        const N: usize = 32;
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("codegen-claims.db");
        let claims = Arc::new(CodegenClaims::open(db).expect("open"));
        let start = Arc::new(std::sync::Barrier::new(N));

        let mut handles = Vec::new();
        for t in 0..N {
            let claims = Arc::clone(&claims);
            let start = Arc::clone(&start);
            handles.push(std::thread::spawn(move || {
                start.wait();
                claims
                    .record(
                        &format!("//t{t}:gen"),
                        &[Claim::file(format!("/t{t}/gen.go"))],
                    )
                    .expect("record");
            }));
        }
        for h in handles {
            h.join().expect("thread");
        }

        let set = claims.snapshot();
        let missing: Vec<usize> = (0..N)
            .filter(|t| !set.claims(Path::new(&format!("t{t}/gen.go"))))
            .collect();
        assert!(missing.is_empty(), "claims lost: {missing:?}");
    }

    // ─── Matching semantics ────────────────────────────────────────────────

    #[test]
    fn file_claim_matches_only_that_path() {
        let (_d, c) = claiming(&[Claim::file("/fmt/generated.txt")]);
        let s = c.snapshot();
        assert!(s.claims(p("fmt/generated.txt")));
        assert!(!s.claims(p("fmt/other.txt")));
        assert!(!s.claims(p("generated.txt")));
    }

    #[test]
    fn dir_claim_matches_the_dir_and_its_subtree() {
        let (_d, c) = claiming(&[Claim::dir("/pkg/gen")]);
        let s = c.snapshot();
        assert!(s.claims(p("pkg/gen")), "the directory itself");
        assert!(s.claims(p("pkg/gen/a.go")), "a file directly inside");
        assert!(s.claims(p("pkg/gen/deep/b.go")), "a file nested inside");
        assert!(
            !s.claims(p("pkg/generated.go")),
            "a sibling sharing a prefix"
        );
        assert!(!s.claims(p("pkg/gen2/a.go")), "a sibling one char longer");
    }

    #[test]
    fn glob_claim_matches_by_pattern() {
        let (_d, c) = claiming(&[Claim::glob("/pkg/**/*.pb.go")]);
        let s = c.snapshot();
        assert!(s.claims(p("pkg/a.pb.go")));
        assert!(s.claims(p("pkg/sub/b.pb.go")));
        assert!(!s.claims(p("pkg/a.go")));
    }

    #[test]
    fn owner_is_the_recording_target() {
        let (_d, c) = claiming(&[Claim::file("/pkg/gen.go"), Claim::dir("/pkg/out")]);
        let s = c.snapshot();
        assert_eq!(s.owner(p("pkg/gen.go")), Some("//pkg:gen"));
        assert_eq!(s.owner(p("pkg/out/deep/x.go")), Some("//pkg:gen"));
        assert_eq!(s.owner(p("src/main.rs")), None);
    }

    #[test]
    fn disabled_never_claims() {
        let c = CodegenClaims::disabled();
        let s = c.snapshot();
        assert!(s.is_empty());
        assert!(!s.claims(p("anything")));
    }

    // ─── Lifecycle ─────────────────────────────────────────────────────────

    /// The property the whole design turns on: a target that generates into the
    /// tree is claimed by the act of generating — no command run first, no file
    /// for anyone to edit, nothing to keep in sync.
    #[test]
    fn generating_is_what_claims_a_path() {
        let (_dir, claims) = workspace();
        assert!(claims.snapshot().is_empty(), "nothing claimed up front");
        claims
            .record("//pkg:gen", &[Claim::file("/pkg/gen.go")])
            .expect("record");
        let s = claims.snapshot();
        assert!(s.claims(p("pkg/gen.go")));
        assert!(!s.claims(p("pkg/hand_written.go")));
    }

    /// A separately-opened handle — the shape a cdylib plugin has — sees a claim
    /// the host recorded.
    #[test]
    fn a_second_handle_sees_a_recorded_claim() {
        let (dir, writer) = workspace();
        let reader = CodegenClaims::open(dir.path().join("codegen-claims.db")).expect("open");
        assert!(!reader.snapshot().claims(p("pkg/gen.go")));

        writer
            .record("//pkg:gen", &[Claim::file("/pkg/gen.go")])
            .expect("record");

        assert!(
            reader.snapshot().claims(p("pkg/gen.go")),
            "an independent handle must notice the change"
        );
    }

    #[test]
    fn re_recording_replaces_that_targets_claims() {
        let (_dir, claims) = workspace();
        claims
            .record("//pkg:gen", &[Claim::file("/pkg/old.go")])
            .expect("record");
        claims
            .record("//pkg:gen", &[Claim::file("/pkg/new.go")])
            .expect("re-record");
        let s = claims.snapshot();
        assert!(s.claims(p("pkg/new.go")));
        assert!(!s.claims(p("pkg/old.go")), "the dropped output is released");
    }

    #[test]
    fn records_from_different_targets_coexist() {
        let (_dir, claims) = workspace();
        claims
            .record("//a:gen", &[Claim::file("/a/gen.go")])
            .expect("record a");
        claims
            .record("//b:gen", &[Claim::file("/b/gen.go")])
            .expect("record b");
        let s = claims.snapshot();
        assert_eq!(s.owner(p("a/gen.go")), Some("//a:gen"));
        assert_eq!(s.owner(p("b/gen.go")), Some("//b:gen"));
    }

    #[test]
    fn recording_no_claims_releases_them() {
        let (_dir, claims) = workspace();
        claims
            .record("//pkg:gen", &[Claim::file("/pkg/gen.go")])
            .expect("record");
        assert!(claims.snapshot().claims(p("pkg/gen.go")));
        claims.record("//pkg:gen", &[]).expect("release");
        assert!(!claims.snapshot().claims(p("pkg/gen.go")));
    }

    /// Declaration order must not matter: a differently-ordered `out` has to
    /// reach the same stored row, or every run rewrites it.
    #[test]
    fn claim_order_does_not_change_what_is_stored() {
        let (_dir, claims) = workspace();
        let a = [Claim::file("/pkg/z.go"), Claim::file("/pkg/a.go")];
        let b = [Claim::file("/pkg/a.go"), Claim::file("/pkg/z.go")];
        claims.record("//pkg:gen", &a).expect("record");
        let first = claims.entries().expect("entries");
        claims.record("//pkg:gen", &b).expect("re-record");
        assert_eq!(first, claims.entries().expect("entries"));
    }

    #[test]
    fn rewrite_releases_a_deleted_targets_claims() {
        let (_dir, claims) = workspace();
        claims
            .record("//pkg:gone", &[Claim::file("/pkg/gone.go")])
            .expect("record");
        claims
            .record("//pkg:live", &[Claim::file("/pkg/live.go")])
            .expect("record");

        let live = BTreeMap::from([("//pkg:live".to_string(), vec![Claim::file("/pkg/live.go")])]);
        claims.rewrite(&live).expect("rewrite");

        let s = claims.snapshot();
        assert!(
            !s.claims(p("pkg/gone.go")),
            "a deleted target's claim is released"
        );
        assert!(s.claims(p("pkg/live.go")), "a live target's claim stays");
    }

    #[test]
    fn entries_round_trips_every_kind() {
        let (_dir, claims) = workspace();
        let declared = vec![
            Claim::file("/pkg/a.go"),
            Claim::dir("/pkg/out"),
            Claim::glob("/pkg/**/*.pb.go"),
        ];
        claims.record("//pkg:gen", &declared).expect("record");
        let round = claims.entries().expect("entries");
        let mut got = round.get("//pkg:gen").cloned().expect("addr present");
        got.sort_by(|a, b| a.path.cmp(&b.path));
        let mut want = declared;
        want.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(got, want);
    }
}
