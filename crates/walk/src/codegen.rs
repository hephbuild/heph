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
//! # The ledger
//!
//! [`CodegenClaims`] answers from a ledger at `<home>/codegen-claims`, written by
//! the codegen write-back itself in the same operation that puts the file on
//! disk. That keeps the xattr's guarantee — registered atomically with the file,
//! so a generated file never exists unclaimed — while living somewhere no tool
//! that rewrites the file can reach. It needs no user action, which is the whole
//! point: a target that generates into the tree is claimed the moment it does so,
//! not once someone remembers to run a command.
//!
//! The set is re-read when the ledger changes, so a claim registered by a
//! write-back is visible to a glob later in the *same* run — again matching what
//! reading an xattr off the filesystem gave for free.
//!
//! Deliberately NOT the heph-managed `.gitignore` section, which carries the same
//! paths. That section exists to tell **git** to ignore build outputs; it is a
//! file users own and edit, and deriving build-input classification from it would
//! mean a hand-edit silently changes what heph treats as source. Its failure
//! direction is the bad one, too: a stale section over-claims, and an
//! over-claimed path hides a real source file from every glob with no diagnostic.
//! heph's own state answers this question.
//!
//! # Releasing a claim
//!
//! A target whose `out` moved releases the old path the next time it generates:
//! [`CodegenClaims::record`] replaces that target's whole block. A target
//! *deleted* from the tree never generates again, so nothing would ever release
//! its claims — and a claim that outlives its target silently hides a real source
//! file at that path. [`CodegenClaims::rewrite`] reconciles the ledger against the
//! live set, driven by `heph tool gen-gitignore` (the command that already
//! resolves every codegen target); `heph validate` reports the discrepancy
//! without repairing it.
//!
//! This lives next to [`Ignore`](crate::Ignore) because the same set has to reach
//! every tree-walking plugin and this crate is the one they all already depend
//! on.

use anyhow::Context as _;
use parking_lot::RwLock;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wax::{Any, Glob, Program as _};

/// Preamble on the ledger, so someone who finds the file knows what it is.
const LEDGER_HEADER: &str = "\
# heph-owned: which tree paths are `codegen = \"copy\"` output.\n\
# Written by the codegen write-back; safe to delete (it is rebuilt).\n";

/// One claimed path pattern and the target that emits it.
#[derive(Debug)]
struct Claim {
    /// Compiled matcher for workspace-relative paths.
    glob: Glob<'static>,
    /// The emitting target's addr, from the attribution comment above the
    /// pattern. `None` for an un-attributed (hand-written or legacy) line.
    owner: Option<String>,
}

/// An immutable, compiled set of claimed paths — the answer to "is this path
/// generated?" at one point in time.
///
/// Handed out by [`CodegenClaims::snapshot`] so a walk matches thousands of paths
/// against a fixed set without touching a lock or the filesystem per file.
#[derive(Debug)]
pub struct ClaimSet {
    claims: Vec<Claim>,
    /// Union of every claim's glob — the fast path, one match for the common
    /// "is this path generated?" question asked once per walked file.
    any: Arc<Any<'static>>,
}

impl ClaimSet {
    /// Claims nothing.
    pub fn empty() -> Self {
        Self {
            claims: Vec::new(),
            any: Arc::new(wax::any(Vec::<Glob<'static>>::new()).expect("empty any is valid")),
        }
    }

    /// True when nothing is claimed — the whole check can be skipped.
    pub fn is_empty(&self) -> bool {
        self.claims.is_empty()
    }

    /// True if the workspace-relative path `rel` is generated by a
    /// `codegen = "copy"` target, and so must never be sourced as raw input.
    pub fn claims(&self, rel: &Path) -> bool {
        !self.claims.is_empty() && self.any.is_match(rel)
    }

    /// The addr of the target that emits `rel`, when the claim carries an
    /// attribution. Diagnostics only — [`Self::claims`] is the decision.
    pub fn owner(&self, rel: &Path) -> Option<&str> {
        self.claims
            .iter()
            .find(|c| c.glob.is_match(rel))
            .and_then(|c| c.owner.as_deref())
    }

    /// Build from attributed pattern lines: a `# //pkg:target` comment applies to
    /// the pattern line under it — the shape [`CodegenClaims::record`] writes.
    fn from_lines<'a>(lines: impl Iterator<Item = &'a str>) -> anyhow::Result<Self> {
        let mut claims = Vec::new();
        let mut pending: Option<String> = None;
        for line in lines.map(str::trim).filter(|l| !l.is_empty()) {
            if let Some(rest) = line.strip_prefix('#') {
                // Attribution comments name the emitting target; anything else in
                // the section is a comment we don't own.
                let rest = rest.trim();
                pending = rest.starts_with("//").then(|| rest.to_string());
                continue;
            }
            // A negation un-ignores a path — nothing heph writes emits one, and
            // treating it as a claim would invert its meaning.
            if line.starts_with('!') {
                pending = None;
                continue;
            }
            let owner = pending.take();
            for pattern in expand_pattern(line) {
                let glob = Glob::new(&pattern)
                    .map(Glob::into_owned)
                    .with_context(|| format!("invalid codegen claim pattern '{pattern}'"))?;
                claims.push(Claim {
                    glob,
                    owner: owner.clone(),
                });
            }
        }
        let any = Arc::new(
            wax::any(claims.iter().map(|c| c.glob.clone()).collect::<Vec<_>>())
                .context("compiling codegen claim patterns")?,
        );
        Ok(Self { claims, any })
    }
}

/// Identifies a version of the ledger file without reading it. Every target we
/// support has sub-second mtime resolution (APFS and ext4 both store
/// nanoseconds), and the length is carried too so an in-place rewrite that
/// somehow lands in the same tick is still noticed.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct Marker {
    mtime: Option<std::time::SystemTime>,
    len: u64,
}

impl Marker {
    /// The marker for a missing file, distinct from any real one.
    const ABSENT: Self = Self {
        mtime: None,
        len: u64::MAX,
    };

    fn of(path: &Path) -> Self {
        match std::fs::metadata(path) {
            Ok(m) => Self {
                mtime: m.modified().ok(),
                len: m.len(),
            },
            Err(_) => Self::ABSENT,
        }
    }
}

#[derive(Debug)]
struct State {
    set: Arc<ClaimSet>,
    /// The ledger version `set` was built from.
    seen: Marker,
}

/// The live set of workspace paths owned by `codegen = "copy"` targets.
///
/// Built once at engine construction and handed to every plugin that walks the
/// tree (a cdylib plugin builds its own from the same two paths). Shared by
/// `Arc`; [`Self::snapshot`] is the read path and [`Self::record`] the write one.
#[derive(Debug)]
pub struct CodegenClaims {
    /// `<home>/codegen-claims` — the ledger the write-back maintains, and the
    /// only thing that decides whether a path is generated. `None` for a claim
    /// set with no workspace behind it.
    ledger: Option<PathBuf>,
    state: RwLock<State>,
}

impl Default for CodegenClaims {
    fn default() -> Self {
        Self::disabled()
    }
}

impl CodegenClaims {
    /// A claim set with no workspace behind it: claims nothing, reads nothing,
    /// records nothing. For the non-engine call sites (LSP, unit tests).
    pub fn disabled() -> Self {
        Self {
            ledger: None,
            state: RwLock::new(State {
                set: Arc::new(ClaimSet::empty()),
                seen: Marker::ABSENT,
            }),
        }
    }

    /// Read the claim set from the ledger at `ledger` (`<home>/codegen-claims`).
    ///
    /// Never fails: an unreadable file or a malformed pattern yields an empty set
    /// with a warning rather than refusing to build. The cost of an empty set is
    /// that generated files get sourced as raw input — noisy and wrong, but a
    /// build that will not start is worse, and the ledger repairs itself on the
    /// next write-back.
    pub fn load(ledger: PathBuf) -> Self {
        let this = Self {
            ledger: Some(ledger),
            state: RwLock::new(State {
                set: Arc::new(ClaimSet::empty()),
                // Never equal to a real marker, so the first `snapshot` builds.
                seen: Marker {
                    mtime: None,
                    len: u64::MAX - 1,
                },
            }),
        };
        this.snapshot();
        this
    }

    /// The current claim set, re-reading the sources if the ledger changed since
    /// the last look.
    ///
    /// Call once per walk or scan and match every path against the returned set:
    /// the freshness check is a single `stat`, but it is not free enough to run
    /// per file, and a walk wants a fixed answer for its duration anyway.
    pub fn snapshot(&self) -> Arc<ClaimSet> {
        let Some(ledger) = self.ledger.as_deref() else {
            return Arc::clone(&self.state.read().set);
        };
        let marker = Marker::of(ledger);
        {
            let state = self.state.read();
            if state.seen == marker {
                return Arc::clone(&state.set);
            }
        }
        let set = Arc::new(self.build());
        let mut state = self.state.write();
        // Another thread may have rebuilt from a newer ledger while we were
        // reading; keep the newer one rather than moving the marker backwards.
        if state.seen != marker {
            state.set = Arc::clone(&set);
            state.seen = marker;
        }
        Arc::clone(&state.set)
    }

    /// Re-read the ledger and compile it.
    fn build(&self) -> ClaimSet {
        let Some(path) = self.ledger.as_deref() else {
            return ClaimSet::empty();
        };
        // Through the parsed map, not the raw text: the file is append-only, so a
        // target that re-recorded still has its older block in it, and only the
        // latest is a live claim.
        let entries = match read_ledger(path) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %format!("{e:#}"),
                    "read codegen claim ledger; generated files will be sourced as raw input"
                );
                return ClaimSet::empty();
            }
        };
        let mut lines = String::new();
        for (addr, patterns) in &entries {
            for pattern in patterns {
                lines.push_str("# ");
                lines.push_str(addr);
                lines.push('\n');
                lines.push_str(pattern);
                lines.push('\n');
            }
        }
        match ClaimSet::from_lines(lines.lines()) {
            Ok(set) => set,
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "cannot compile the codegen claim set; generated files will be \
                     sourced as raw input"
                );
                ClaimSet::empty()
            }
        }
    }

    /// The ledger's current contents: `addr -> patterns`, with the append-only
    /// history already resolved. For a caller reconciling it against the live set
    /// of targets.
    pub fn entries(&self) -> anyhow::Result<BTreeMap<String, Vec<String>>> {
        match self.ledger.as_deref() {
            Some(path) => read_ledger(path),
            None => Ok(BTreeMap::new()),
        }
    }

    /// Replace the ledger with exactly `entries`.
    ///
    /// [`Self::record`] can only ever *add* a target's claims or update them in
    /// place — it runs when a target generates, and a target that was deleted from
    /// the tree never runs again. Its claims would otherwise outlive it forever,
    /// and a stale claim silently hides a real source file at that path. So the
    /// full set has to be reconciled against the live one somewhere, and that
    /// somewhere is a caller that has just resolved every target: `heph tool
    /// gen-gitignore`.
    ///
    /// Also compacts: `record` appends, so a long-lived workspace accumulates
    /// superseded blocks, and this collapses them.
    pub fn rewrite(&self, entries: &BTreeMap<String, Vec<String>>) -> anyhow::Result<()> {
        let Some(ledger) = self.ledger.as_deref() else {
            return Ok(());
        };
        write_ledger(ledger, entries)?;
        let set = Arc::new(self.build());
        let mut state = self.state.write();
        state.set = set;
        state.seen = Marker::of(ledger);
        Ok(())
    }

    /// Register `patterns` as the root-anchored `codegen = "copy"` output of
    /// `addr`, replacing whatever that target claimed before.
    ///
    /// Called by the codegen write-back in the same operation that puts the files
    /// on disk, so a generated file is never on disk unclaimed. A no-op — no
    /// write, no rebuild — when the ledger already says exactly this, which is
    /// the steady state after the first run.
    ///
    /// The write is an `O_APPEND` of this target's block, and a read resolves
    /// duplicates last-wins. That is what makes two `heph` processes generating
    /// into one workspace safe: a read-modify-write of a whole-file map would let
    /// each one drop the other's target, losing a claim for a file that is
    /// already on disk — the very failure this mechanism exists to prevent.
    /// Appends never interleave, and since an unchanged claim writes nothing, the
    /// file does not grow in the steady state.
    pub fn record(&self, addr: &str, patterns: &[String]) -> anyhow::Result<()> {
        let Some(ledger) = self.ledger.as_deref() else {
            return Ok(());
        };
        if read_ledger(ledger)?.get(addr).map(Vec::as_slice) == Some(patterns) {
            return Ok(());
        }
        append_entry(ledger, addr, patterns)?;

        // Install the new set directly rather than waiting for a `snapshot` to
        // notice: this process just wrote the file, so there is no reason to
        // round-trip through the filesystem's mtime resolution to learn it.
        let set = Arc::new(self.build());
        let mut state = self.state.write();
        state.set = set;
        state.seen = Marker::of(ledger);
        Ok(())
    }
}

/// Parse the ledger into `addr -> patterns`.
///
/// The file is append-only, so a target that re-recorded appears more than once;
/// the LAST block for an addr is its current claim, and earlier ones are history.
/// An empty block (an addr with no patterns under it) is how a target that no
/// longer emits `copy` output releases its paths.
fn read_ledger(path: &Path) -> anyhow::Result<BTreeMap<String, Vec<String>>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => {
            return Err(e).with_context(|| format!("read codegen claim ledger {}", path.display()));
        }
    };
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut pending: Option<&str> = None;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(rest) = line.strip_prefix('#') {
            let rest = rest.trim();
            pending = rest.starts_with("//").then_some(rest);
            // A new block for this addr supersedes whatever it claimed earlier.
            if let Some(addr) = pending {
                out.insert(addr.to_owned(), Vec::new());
            }
            continue;
        }
        // A pattern with no attribution above it belongs to no target, so nothing
        // could ever replace or release it. Drop it rather than let it accumulate
        // as an unowned claim.
        if let Some(addr) = pending {
            out.entry(addr.to_owned())
                .or_default()
                .push(line.to_owned());
        }
    }
    out.retain(|_, patterns| !patterns.is_empty());
    Ok(out)
}

/// Write the ledger as exactly `entries`, atomically: a reader sees either the
/// whole previous version or the whole new one, never a torn file.
fn write_ledger(path: &Path, entries: &BTreeMap<String, Vec<String>>) -> anyhow::Result<()> {
    let mut body = String::from(LEDGER_HEADER);
    for (addr, patterns) in entries {
        for pattern in patterns {
            body.push_str("# ");
            body.push_str(addr);
            body.push('\n');
            body.push_str(pattern);
            body.push('\n');
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create dir for codegen claim ledger {}", parent.display()))?;
    }
    // Same directory as the target so the rename stays within one filesystem, and
    // pid-tagged so two processes rewriting at once cannot share a temp name.
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, body)
        .with_context(|| format!("write codegen claim ledger {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("install codegen claim ledger {}", path.display()))?;
    Ok(())
}

/// Append one target's block. `O_APPEND` positions the write atomically, so two
/// processes recording different targets at the same time cannot lose either.
///
/// A block with no pattern lines releases that target's claims — the header alone
/// supersedes its previous block.
fn append_entry(path: &Path, addr: &str, patterns: &[String]) -> anyhow::Result<()> {
    use std::io::Write as _;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create dir for codegen claim ledger {}", parent.display()))?;
    }
    let mut block = String::new();
    if patterns.is_empty() {
        block.push_str("# ");
        block.push_str(addr);
        block.push('\n');
    }
    for pattern in patterns {
        block.push_str("# ");
        block.push_str(addr);
        block.push('\n');
        block.push_str(pattern);
        block.push('\n');
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open codegen claim ledger {}", path.display()))?;
    f.write_all(block.as_bytes())
        .with_context(|| format!("append to codegen claim ledger {}", path.display()))?;
    Ok(())
}

/// Translate one root-anchored pattern into the wax globs that match the same
/// workspace-relative paths.
///
/// Two differences from gitignore syntax matter here:
///  - a leading `/` anchors to the repo root, which is already what a
///    workspace-relative match means, so it is stripped;
///  - a directory pattern ignores the whole subtree implicitly, while wax
///    matches only the named path — so every pattern also gets a `…/**` form.
///    A file pattern's `…/**` form matches nothing, which costs one extra glob
///    and never a wrong answer.
fn expand_pattern(pattern: &str) -> impl Iterator<Item = String> {
    let base = pattern
        .trim_start_matches('/')
        .trim_end_matches('/')
        .to_string();
    let subtree = format!("{base}/**");
    [base, subtree].into_iter().filter(|p| !p.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> &Path {
        Path::new(s)
    }

    /// A claim set holding `patterns`, attributed to `//pkg:gen`, as if a codegen
    /// target had generated them.
    fn claiming(patterns: &[&str]) -> (tempfile::TempDir, CodegenClaims) {
        let dir = tempfile::tempdir().expect("tempdir");
        let claims = CodegenClaims::load(dir.path().join("codegen-claims"));
        let owned: Vec<String> = patterns.iter().map(|p| (*p).to_string()).collect();
        claims.record("//pkg:gen", &owned).expect("record");
        (dir, claims)
    }

    /// An empty workspace, ready for `record`.
    fn workspace() -> (tempfile::TempDir, CodegenClaims) {
        let dir = tempfile::tempdir().expect("tempdir");
        let claims = CodegenClaims::load(dir.path().join("codegen-claims"));
        (dir, claims)
    }

    // ─── Pattern semantics ─────────────────────────────────────────────────

    #[test]
    fn file_claim_matches_only_that_path() {
        let (_d, c) = claiming(&["/fmt/generated.txt"]);
        let s = c.snapshot();
        assert!(s.claims(p("fmt/generated.txt")));
        assert!(!s.claims(p("fmt/other.txt")));
        assert!(!s.claims(p("generated.txt")));
    }

    #[test]
    fn dir_claim_matches_the_dir_and_its_subtree() {
        let (_d, c) = claiming(&["/pkg/gen"]);
        let s = c.snapshot();
        assert!(s.claims(p("pkg/gen")), "the directory itself");
        assert!(s.claims(p("pkg/gen/a.go")), "a file directly inside");
        assert!(s.claims(p("pkg/gen/deep/b.go")), "a file nested inside");
        assert!(!s.claims(p("pkg/generated.go")), "a sibling with a prefix");
    }

    #[test]
    fn glob_claim_matches_by_pattern() {
        let (_d, c) = claiming(&["/pkg/**/*.pb.go"]);
        let s = c.snapshot();
        assert!(s.claims(p("pkg/a.pb.go")));
        assert!(s.claims(p("pkg/sub/b.pb.go")));
        assert!(!s.claims(p("pkg/a.go")));
    }

    #[test]
    fn owner_is_the_recording_target() {
        let (_d, c) = claiming(&["/pkg/gen"]);
        let s = c.snapshot();
        assert_eq!(s.owner(p("pkg/gen/a.go")), Some("//pkg:gen"));
        assert_eq!(s.owner(p("src/main.rs")), None);
    }

    /// A `.gitignore` carrying the very same paths is not a claim source. It
    /// exists to tell git to ignore build outputs, it is a file users edit, and a
    /// hand-edit must not change what heph treats as source.
    #[test]
    fn a_gitignore_beside_the_workspace_claims_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(".gitignore"),
            "# BEGIN heph-generated\n# //pkg:gen\n/pkg/gen.go\n# END heph-generated\n",
        )
        .expect("write .gitignore");
        let claims = CodegenClaims::load(dir.path().join("codegen-claims"));
        assert!(claims.snapshot().is_empty());
        assert!(!claims.snapshot().claims(p("pkg/gen.go")));
    }

    #[test]
    fn malformed_pattern_is_an_error_not_a_silent_drop() {
        assert!(
            ClaimSet::from_lines(["# //pkg:gen", "/pkg/<bad"].into_iter()).is_err(),
            "an unparseable claim must not be dropped"
        );
    }

    #[test]
    fn disabled_never_claims() {
        let c = CodegenClaims::disabled();
        let s = c.snapshot();
        assert!(s.is_empty());
        assert!(!s.claims(p("anything")));
    }

    // ─── The ledger ────────────────────────────────────────────────────────

    /// The property the whole design turns on: a target that generates into the
    /// tree is claimed by the act of generating — no command run first, no file
    /// for anyone to edit, nothing to keep in sync.
    #[test]
    fn generating_is_what_claims_a_path() {
        let (_dir, claims) = workspace();
        assert!(claims.snapshot().is_empty(), "nothing claimed up front");

        claims
            .record("//pkg:gen", &["/pkg/gen.go".to_string()])
            .expect("record");

        let s = claims.snapshot();
        assert!(s.claims(p("pkg/gen.go")));
        assert_eq!(s.owner(p("pkg/gen.go")), Some("//pkg:gen"));
        assert!(!s.claims(p("pkg/hand_written.go")));
    }

    /// A claim recorded mid-run is visible to a walk later in the same run,
    /// through the same shared handle — what re-reading an xattr off the
    /// filesystem gave for free, and what a load-once set would not.
    #[test]
    fn a_recorded_claim_is_visible_to_a_later_snapshot() {
        let (_dir, claims) = workspace();
        let before = claims.snapshot();
        claims
            .record("//pkg:gen", &["/pkg/gen.go".to_string()])
            .expect("record");
        let after = claims.snapshot();

        assert!(
            !before.claims(p("pkg/gen.go")),
            "the earlier walk's set is fixed"
        );
        assert!(after.claims(p("pkg/gen.go")), "a later walk sees it");
    }

    /// A separately-constructed reader — the shape a cdylib plugin has, holding
    /// its own handle over the same paths — picks the claim up too.
    #[test]
    fn a_second_reader_over_the_same_paths_sees_a_recorded_claim() {
        let (dir, writer) = workspace();
        let ledger = dir.path().join("codegen-claims");
        let reader = CodegenClaims::load(ledger);
        assert!(!reader.snapshot().claims(p("pkg/gen.go")));

        writer
            .record("//pkg:gen", &["/pkg/gen.go".to_string()])
            .expect("record");

        assert!(
            reader.snapshot().claims(p("pkg/gen.go")),
            "an independent handle must notice the ledger change"
        );
    }

    /// Re-recording replaces a target's claims rather than accumulating them, so
    /// an output path that a target no longer emits stops being claimed — a stale
    /// claim would hide a real source file.
    #[test]
    fn re_recording_replaces_that_targets_claims() {
        let (_dir, claims) = workspace();
        claims
            .record("//pkg:gen", &["/pkg/old.go".to_string()])
            .expect("record");
        claims
            .record("//pkg:gen", &["/pkg/new.go".to_string()])
            .expect("re-record");

        let s = claims.snapshot();
        assert!(s.claims(p("pkg/new.go")));
        assert!(!s.claims(p("pkg/old.go")), "the dropped output is released");
    }

    /// One target's record must not disturb another's.
    #[test]
    fn records_from_different_targets_coexist() {
        let (_dir, claims) = workspace();
        claims
            .record("//a:gen", &["/a/gen.go".to_string()])
            .expect("record a");
        claims
            .record("//b:gen", &["/b/gen.go".to_string()])
            .expect("record b");

        let s = claims.snapshot();
        assert_eq!(s.owner(p("a/gen.go")), Some("//a:gen"));
        assert_eq!(s.owner(p("b/gen.go")), Some("//b:gen"));
    }

    /// Two `heph` processes generating into one workspace record through separate
    /// handles that never see each other's in-memory state. Both claims must
    /// survive: losing one leaves a file already on disk unclaimed, which is the
    /// exact failure this mechanism exists to prevent.
    ///
    /// This is why the ledger is appended to rather than rewritten — a
    /// read-modify-write of a whole-file map lets each writer drop the other's
    /// target.
    #[test]
    fn concurrent_writers_do_not_drop_each_others_claims() {
        let (dir, a) = workspace();
        let ledger = dir.path().join("codegen-claims");
        let b = CodegenClaims::load(ledger);

        // Interleaved the damaging way: both read the ledger before either writes.
        assert!(a.snapshot().is_empty());
        assert!(b.snapshot().is_empty());
        a.record("//a:gen", &["/a/gen.go".to_string()])
            .expect("record a");
        b.record("//b:gen", &["/b/gen.go".to_string()])
            .expect("record b");

        for (label, claims) in [("a", &a), ("b", &b)] {
            let s = claims.snapshot();
            assert!(s.claims(p("a/gen.go")), "{label} lost //a:gen");
            assert!(s.claims(p("b/gen.go")), "{label} lost //b:gen");
        }
    }

    /// The append-only file accumulates a target's history; only its latest block
    /// is a live claim. Without last-wins on read, a path a target stopped
    /// emitting would stay claimed forever and hide a real source file.
    #[test]
    fn only_the_latest_block_for_a_target_counts() {
        let (dir, claims) = workspace();
        let ledger = dir.path().join("codegen-claims");
        claims
            .record("//pkg:gen", &["/pkg/old.go".to_string()])
            .expect("record");
        claims
            .record("//pkg:gen", &["/pkg/new.go".to_string()])
            .expect("re-record");

        // Both blocks are still in the file — nothing rewrites it.
        let raw = std::fs::read_to_string(&ledger).expect("read ledger");
        assert!(
            raw.contains("/pkg/old.go"),
            "history is kept, not rewritten"
        );

        // A reader starting cold resolves the same way as the writer's own view.
        let cold = CodegenClaims::load(ledger);
        let s = cold.snapshot();
        assert!(s.claims(p("pkg/new.go")));
        assert!(!s.claims(p("pkg/old.go")), "superseded claim is released");
    }

    /// Recording no patterns releases a target's claims outright — the path a
    /// target used to generate becomes ordinary source again.
    #[test]
    fn recording_no_patterns_releases_the_claim() {
        let (_dir, claims) = workspace();
        claims
            .record("//pkg:gen", &["/pkg/gen.go".to_string()])
            .expect("record");
        assert!(claims.snapshot().claims(p("pkg/gen.go")));

        claims.record("//pkg:gen", &[]).expect("release");
        assert!(!claims.snapshot().claims(p("pkg/gen.go")));
    }

    /// A target deleted from the tree never runs again, so `record` can never
    /// release its claims — they would hide a real source file at that path
    /// forever. `rewrite` against the live set is what releases them.
    #[test]
    fn rewrite_releases_a_deleted_targets_claims() {
        let (_dir, claims) = workspace();
        claims
            .record("//pkg:gone", &["/pkg/gone.go".to_string()])
            .expect("record");
        claims
            .record("//pkg:live", &["/pkg/live.go".to_string()])
            .expect("record");
        assert!(claims.snapshot().claims(p("pkg/gone.go")));

        // The live set no longer contains //pkg:gone.
        let live = BTreeMap::from([("//pkg:live".to_string(), vec!["/pkg/live.go".to_string()])]);
        claims.rewrite(&live).expect("rewrite");

        let s = claims.snapshot();
        assert!(
            !s.claims(p("pkg/gone.go")),
            "a deleted target's claim must be released"
        );
        assert!(s.claims(p("pkg/live.go")), "a live target's claim stays");
    }

    /// `rewrite` also compacts: `record` appends, so a workspace that has been
    /// building for a while accumulates superseded blocks.
    #[test]
    fn rewrite_compacts_the_append_history() {
        let (dir, claims) = workspace();
        let ledger = dir.path().join("codegen-claims");
        for out in ["/pkg/a.go", "/pkg/b.go", "/pkg/c.go"] {
            claims
                .record("//pkg:gen", &[out.to_string()])
                .expect("record");
        }
        let before = std::fs::read_to_string(&ledger).expect("read");
        assert!(before.contains("/pkg/a.go"), "history accumulated");

        let live = BTreeMap::from([("//pkg:gen".to_string(), vec!["/pkg/c.go".to_string()])]);
        claims.rewrite(&live).expect("rewrite");

        let after = std::fs::read_to_string(&ledger).expect("read");
        assert!(!after.contains("/pkg/a.go"), "superseded blocks collapsed");
        assert!(claims.snapshot().claims(p("pkg/c.go")));
    }

    /// `entries` resolves the append history the same way a read does, so a
    /// caller reconciling against the live set compares like with like.
    #[test]
    fn entries_reports_the_resolved_history() {
        let (_dir, claims) = workspace();
        claims
            .record("//pkg:gen", &["/pkg/old.go".to_string()])
            .expect("record");
        claims
            .record("//pkg:gen", &["/pkg/new.go".to_string()])
            .expect("re-record");

        assert_eq!(
            claims.entries().expect("entries"),
            BTreeMap::from([("//pkg:gen".to_string(), vec!["/pkg/new.go".to_string()])])
        );
    }

    /// Recording the same thing twice must not rewrite the file — the steady
    /// state after the first run is every target re-recording what it already
    /// claimed, and that should touch nothing.
    #[test]
    fn re_recording_identical_claims_does_not_rewrite_the_ledger() {
        let (dir, claims) = workspace();
        let ledger = dir.path().join("codegen-claims");
        claims
            .record("//pkg:gen", &["/pkg/gen.go".to_string()])
            .expect("record");
        let first = Marker::of(&ledger);

        claims
            .record("//pkg:gen", &["/pkg/gen.go".to_string()])
            .expect("re-record");
        assert_eq!(Marker::of(&ledger), first, "identical record is a no-op");
    }
}
