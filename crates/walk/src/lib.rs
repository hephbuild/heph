//! Filesystem-walk ignore rules shared by every tree-walking plugin (the `fs`
//! driver/provider, and the buildfile/go providers).
//!
//! The core idea: distinguish *pruning* a directory subtree (stop descending —
//! cheap, the whole point of an ignore) from merely *excluding* individual
//! files. A glob prunes a directory when it names directories — its final
//! component is a literal (`foo/**/bar` prunes every `bar` under `foo`), or it
//! ends in a recursive `/**` / `/**/*` whose prefix names the dir
//! (`**/node_modules/**` → `**/node_modules`). A glob whose final component is a
//! wildcard (`some/*.go`) only filters files. No probing or sentinel paths.
//!
//! Every ignore set also prunes a directory named with
//! [`HEPH_DIR_PREFIX`](hconfig::HEPH_DIR_PREFIX) (`.heph`, `.heph3`,
//! `.heph-gocache`, …): heph-owned, never source.
//!
//! Two ignore sources, both workspace-root-relative:
//!  - `dirs`: absolute directories pruned by exact path (heph home + literal
//!    `fs.skip` entries resolved against the root).
//!  - `globs`: wax glob patterns matched against root-relative paths.

#![cfg_attr(
    test,
    expect(
        clippy::assertions_on_constants,
        reason = "restriction/style lints scoped to production code; tests are exempt"
    )
)]

use anyhow::Context as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wax::{Any, Glob, Program as _};

pub mod cached_walker;
pub use cached_walker::{CachedWalker, DirEntry, DirListing, EntryKind, FileHash, file_hashout};

/// Directory + glob ignore rules for a filesystem walk. See the module docs.
///
/// The glob matchers are compiled on first use, not at construction. Compiling
/// is regex construction — several hundred microseconds per ignore set, on the
/// startup path of every command, which built three of them — and a command
/// that never walks a tree (a cached `run`, `version`) never needs one.
/// Construction still *validates* every glob (parse and rule check, no regex),
/// so a bad `fs.skip` entry fails where it always did.
///
/// What validation cannot catch is a set that parses but whose regex exceeds
/// the regex crate's compiled-size limit. That surfaces from
/// [`Self::ensure_compiled`], which every walk entry point calls before its
/// first match, so the walk fails with the error instead of walking with a
/// partial ignore set.
#[derive(Debug, Clone)]
pub struct Ignore {
    /// Absolute directories pruned by exact path.
    dirs: Vec<PathBuf>,
    /// The raw ignore globs, kept so callers can chain them with a target's own
    /// excludes when compiling a per-target matcher.
    globs: Vec<String>,
    /// The compiled matchers, built from `globs` on first use. A failure is
    /// latched (as its rendered chain) so every later caller gets the same
    /// error without recompiling.
    compiled: std::sync::OnceLock<Result<Compiled, String>>,
}

/// The compiled form of an [`Ignore`]'s globs.
#[derive(Debug, Clone)]
struct Compiled {
    /// Compiled `globs` — matches a workspace-relative file path that is excluded.
    file_any: Arc<Any<'static>>,
    /// Compiled directory matchers derived from the recursive (`…/**`, `…/**/*`)
    /// `globs` — matches a workspace-relative directory whose subtree is pruned.
    dir_any: Arc<Any<'static>>,
}

impl Default for Ignore {
    fn default() -> Self {
        Self::new(&[], &[]).expect("empty ignore set is valid")
    }
}

/// The `.git` exclusion the engine always prepends to `fs.skip`. A `.git` dir is
/// never a build input, and it is the one directory that reliably contains names
/// no build tool chose — a ref file is whatever bytes the branch name was, so
/// `git checkout -b $'\xe9'` puts a non-UTF-8 name in the tree that
/// [`CachedWalker::read_dir`] refuses to walk past.
pub const GIT_SKIP_GLOB: &str = "**/.git/**";

pub use hconfig::HEPH_DIR_PREFIX;

/// Whether the last component of `path` names a heph-owned directory.
fn is_heph_dir_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(HEPH_DIR_PREFIX))
}

impl Ignore {
    /// An ignore set for a walker with no user skip configuration: prunes
    /// [`GIT_SKIP_GLOB`] (and, like every set, `.heph*` dirs).
    ///
    /// `Ignore::default()` prunes only `.heph*` dirs, which is wrong for any real
    /// tree — it descends into `.git`. Callers that genuinely have no config (the LSP)
    /// want this instead.
    pub fn git_only() -> Self {
        Self::new(&[], &[GIT_SKIP_GLOB.to_string()]).expect("the .git ignore set is valid")
    }

    /// Builds an ignore set from absolute `dirs` (exact-path prune) and
    /// workspace-relative `globs`.
    ///
    /// Every glob — and every directory prefix derived from one — is validated
    /// here; the matchers themselves are compiled on first use (see the type
    /// docs).
    pub fn new(dirs: &[PathBuf], globs: &[String]) -> anyhow::Result<Self> {
        for g in globs.iter().map(String::as_str).chain(dir_globs(globs)) {
            check_pattern(g).with_context(|| format!("invalid ignore pattern '{g}'"))?;
        }
        Ok(Self {
            dirs: dirs.to_vec(),
            globs: globs.to_vec(),
            compiled: std::sync::OnceLock::new(),
        })
    }

    /// Compile the matchers if they are not yet, and report whether they
    /// compiled. Every walk entry point calls this (directly, or through
    /// [`Self::file_matcher`]) before its first [`Self::prune_dir`] /
    /// [`Self::exclude_file`], and propagates the error.
    ///
    /// On input [`Self::new`] accepted, compilation fails only by exceeding the
    /// regex size limit; the error names the offending pattern, or the set when
    /// it is their union that is too big.
    pub fn ensure_compiled(&self) -> anyhow::Result<()> {
        self.compiled().map(drop)
    }

    /// The compiled matchers, built on the first call; the first failure is
    /// returned to every caller.
    fn compiled(&self) -> anyhow::Result<&Compiled> {
        self.compiled
            .get_or_init(|| {
                let compile = || -> anyhow::Result<Compiled> {
                    Ok(Compiled {
                        file_any: compile_any(self.globs.iter().map(String::as_str))?,
                        dir_any: compile_any(dir_globs(&self.globs))?,
                    })
                };
                compile().map_err(|e| format!("{e:#}"))
            })
            .as_ref()
            .map_err(|e| {
                anyhow::anyhow!(
                    "compiling the {} fs.skip ignore glob(s): {e}",
                    self.globs.len()
                )
            })
    }

    /// Whether the matchers have been compiled yet.
    #[cfg(test)]
    fn is_compiled(&self) -> bool {
        self.compiled.get().is_some()
    }

    /// True if the directory at absolute `abs` (workspace-relative `rel`) must be
    /// pruned — never descended into. Either an exact `dirs` entry, a directory
    /// named with [`HEPH_DIR_PREFIX`], or a directory whose path matches a glob /
    /// a recursive glob's prefix.
    ///
    /// The `.heph*` rule holds for every ignore set, `Ignore::default()` too: a
    /// heph-owned directory is never source. The exact-path rule only catches the
    /// *current* home; this one also catches a leftover home from an older
    /// default (`.heph3`), whose staged `BUILD` files and failed sandboxes would
    /// otherwise surface as packages — and, through `heph.core.packages()`, reach
    /// a def hash.
    ///
    /// The walk root itself (`rel == ""`) is never name- or glob-pruned — you are
    /// explicitly walking it — only exact-`dirs` pruning applies.
    ///
    /// Infallible by signature, so a caller must have checked
    /// [`Self::ensure_compiled`]: if the matchers did not compile, nothing is
    /// glob-pruned (exact-path and `.heph*` pruning still apply).
    pub fn prune_dir(&self, abs: &Path, rel: &Path) -> bool {
        if self.dirs.iter().any(|d| d == abs) {
            return true;
        }
        if rel.as_os_str().is_empty() {
            return false;
        }
        is_heph_dir_name(abs) || self.compiled().is_ok_and(|c| c.dir_any.is_match(rel))
    }

    /// True if the file at workspace-relative `rel` is excluded by an ignore glob.
    ///
    /// Like [`Self::prune_dir`], it matches nothing by glob if the matchers did
    /// not compile — a state a walk that called [`Self::ensure_compiled`] first
    /// never reaches.
    pub fn exclude_file(&self, rel: &Path) -> bool {
        self.compiled().is_ok_and(|c| c.file_any.is_match(rel))
    }

    /// True if the package at workspace-relative `pkg` lies inside a pruned
    /// subtree — `pkg` itself or any ancestor directory is pruned. Used by
    /// `get`/`list` so a directly-addressed target under a skipped dir resolves
    /// to nothing, matching what the package walk would have surfaced. `root`
    /// resolves absolute paths for exact-dir matching.
    pub fn prunes_package(&self, root: &Path, pkg: &Path) -> bool {
        let mut cur = Some(pkg);
        while let Some(c) = cur {
            if !c.as_os_str().is_empty() && self.prune_dir(&root.join(c), c) {
                return true;
            }
            cur = c.parent();
        }
        false
    }

    /// [`Self::prunes_package`] after [`Self::ensure_compiled`]: the form for a
    /// provider's `get`/`list`, so a skip set that cannot compile fails the
    /// lookup instead of pruning nothing by glob.
    pub fn try_prunes_package(&self, root: &Path, pkg: &Path) -> anyhow::Result<bool> {
        self.ensure_compiled()?;
        Ok(self.prunes_package(root, pkg))
    }

    /// The raw ignore globs, for chaining with a target's own exclude patterns.
    pub fn globs(&self) -> &[String] {
        &self.globs
    }

    /// The compiled file-exclude matcher, reused directly as the "no extra
    /// excludes" matcher by callers that compile per-target exclude unions.
    /// Compiles the whole set on first use, so `Ok` also means
    /// [`Self::ensure_compiled`] holds.
    pub fn file_matcher(&self) -> anyhow::Result<&Arc<Any<'static>>> {
        Ok(&self.compiled()?.file_any)
    }
}

/// True if `pattern`'s final path component is a literal (no glob
/// metacharacter): the pattern names directories of that path, so a matching
/// directory is pruned (`foo/**/bar`, `internal`). A wildcard final component
/// (`some/*.go`, `vendor/**`) is not directory-naming.
fn names_directory(pattern: &str) -> bool {
    pattern
        .rsplit('/')
        .next()
        .is_some_and(|last| !last.is_empty() && !last.contains(['*', '?', '[', ']', '{', '}']))
}

/// If `pattern` recursively covers a subtree (ends in `/**` or `/**/*`), returns
/// the directory-matching prefix whose subtree should be pruned. A wax `**`
/// matches zero-or-more components, so the prefix matches the target dir at any
/// depth (`**/node_modules/**` → `**/node_modules`, matching `node_modules` and
/// `a/b/node_modules`). Non-recursive patterns return `None` (file-only).
fn prune_prefix(pattern: &str) -> Option<&str> {
    for suffix in ["/**/*", "/**"] {
        if let Some(prefix) = pattern.strip_suffix(suffix)
            && !prefix.is_empty()
        {
            return Some(prefix);
        }
    }
    None
}

/// The directory-pruning patterns derived from `globs`.
///
/// A directory is pruned when a glob names directories of that path — i.e. its
/// final component is a literal (`foo/**/bar` prunes every `bar` dir under
/// `foo`) — OR when it matches the prefix of a recursive glob
/// (`**/node_modules/**`, which only matches *files* under node_modules, still
/// prunes the dir via its `**/node_modules` prefix). A glob whose last component
/// is a wildcard (`some/*.go`) is file-only and never prunes.
fn dir_globs(globs: &[String]) -> impl Iterator<Item = &str> {
    globs
        .iter()
        .map(String::as_str)
        .filter(|g| names_directory(g))
        .chain(globs.iter().filter_map(|g| prune_prefix(g)))
}

/// Parse and rule-check one glob without compiling it to a regex — the part of
/// [`Glob::new`] that rejects a malformed pattern, at a fraction of the cost.
/// Generic so the conversion `wax::any` uses internally can be reached through
/// the public [`wax::Pattern`] bound without naming wax's private checked-tree
/// type.
fn check_pattern<'t, P: wax::Pattern<'t>>(pattern: P) -> Result<(), wax::BuildError> {
    pattern.try_into().map(drop).map_err(Into::into)
}

/// Compiles glob patterns into one `wax::Any`. An empty set yields an `Any` that
/// matches nothing.
fn compile_any<'a>(patterns: impl Iterator<Item = &'a str>) -> anyhow::Result<Arc<Any<'static>>> {
    let globs = patterns
        .map(|s| {
            // On a pattern `check_pattern` accepted, the only failure is the
            // regex size limit.
            Glob::new(s).map(Glob::into_owned).with_context(|| {
                format!("ignore pattern '{s}' exceeds the regex size limit; shorten or split it")
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(Arc::new(wax::any(globs).context(
        "the union of the ignore patterns exceeds the regex size limit; use fewer or simpler \
         patterns",
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> &Path {
        Path::new(s)
    }

    #[test]
    fn recursive_glob_prunes_dir_at_any_depth() {
        let ig = Ignore::new(&[], &["**/node_modules/**".to_string()]).unwrap();
        // The directory itself is pruned (root + nested), without a probe.
        assert!(ig.prune_dir(p("/ws/node_modules"), p("node_modules")));
        assert!(ig.prune_dir(p("/ws/a/b/node_modules"), p("a/b/node_modules")));
        // A sibling dir is not pruned.
        assert!(!ig.prune_dir(p("/ws/src"), p("src")));
        // Files inside are excluded too.
        assert!(ig.exclude_file(p("a/node_modules/dep/index.js")));
        assert!(!ig.exclude_file(p("a/main.rs")));
    }

    #[test]
    fn trailing_star_star_slash_star_prunes() {
        let ig = Ignore::new(&[], &["vendor/**/*".to_string()]).unwrap();
        assert!(ig.prune_dir(p("/ws/vendor"), p("vendor")));
        assert!(ig.exclude_file(p("vendor/x/y.go")));
    }

    #[test]
    fn bare_name_glob_prunes_matching_dir() {
        // A non-recursive pattern that names a directory still prunes it (the
        // long-standing buildfile/go `skip` semantics).
        let ig = Ignore::new(&[], &["internal".to_string()]).unwrap();
        assert!(ig.prune_dir(p("/ws/internal"), p("internal")));
        assert!(!ig.prune_dir(p("/ws/src"), p("src")));
    }

    #[test]
    fn non_recursive_glob_excludes_files_but_does_not_prune() {
        let ig = Ignore::new(&[], &["**/*.tmp".to_string()]).unwrap();
        assert!(ig.exclude_file(p("a/b/c.tmp")));
        assert!(!ig.exclude_file(p("a/b/c.rs")));
        // `*.tmp` is not a subtree — directories are still walked.
        assert!(!ig.prune_dir(p("/ws/a"), p("a")));
    }

    #[test]
    fn mid_doublestar_with_literal_tail_prunes() {
        // `foo/**/bar` prunes every `bar` dir under `foo` (final component is a
        // literal, so it names directories), root-relative.
        let ig = Ignore::new(&[], &["foo/**/bar".to_string()]).unwrap();
        assert!(ig.prune_dir(p("/ws/foo/bar"), p("foo/bar")));
        assert!(ig.prune_dir(p("/ws/foo/x/y/bar"), p("foo/x/y/bar")));
        assert!(!ig.prune_dir(p("/ws/foo/x"), p("foo/x")));
        // Not under `foo`.
        assert!(!ig.prune_dir(p("/ws/bar"), p("bar")));
    }

    #[test]
    fn file_glob_with_wildcard_tail_does_not_prune() {
        // `some/*.go` is a file pattern (wildcard final component): excludes the
        // matching files, never prunes a directory.
        let ig = Ignore::new(&[], &["some/*.go".to_string()]).unwrap();
        assert!(ig.exclude_file(p("some/a.go")));
        assert!(!ig.prune_dir(p("/ws/some"), p("some")));
    }

    #[test]
    fn heph_named_dirs_are_pruned_by_name() {
        // No configured dirs or globs: a leftover `.heph3`, a nested one, and
        // `.heph-*` tool caches are still never descended.
        let ig = Ignore::default();
        assert!(ig.prune_dir(p("/ws/.heph3"), p(".heph3")));
        assert!(ig.prune_dir(p("/ws/a/.heph"), p("a/.heph")));
        assert!(ig.prune_dir(p("/ws/.heph-anything"), p(".heph-anything")));
        assert!(ig.prunes_package(p("/ws"), p(".heph3/stage/x")));
        assert!(!ig.prune_dir(p("/ws/heph"), p("heph")));
        // The walk root itself is never name-pruned.
        assert!(!ig.prune_dir(p("/ws/.heph/sandbox"), p("")));
    }

    #[test]
    fn exact_dirs_are_pruned() {
        let home = PathBuf::from("/ws/state");
        let ig = Ignore::new(std::slice::from_ref(&home), &[]).unwrap();
        assert!(ig.prune_dir(&home, p("state")));
        assert!(!ig.prune_dir(p("/ws/src"), p("src")));
    }

    #[test]
    fn prunes_package_checks_ancestors() {
        let ig = Ignore::new(
            &[PathBuf::from("/ws/node_modules")],
            &["**/gen/**".to_string()],
        )
        .unwrap();
        // Package directly under an exact skip dir.
        assert!(ig.prunes_package(p("/ws"), p("node_modules/dep")));
        // Package under a glob-pruned dir (any depth).
        assert!(ig.prunes_package(p("/ws"), p("a/gen/proto")));
        // The skip dir / pruned dir itself.
        assert!(ig.prunes_package(p("/ws"), p("node_modules")));
        // Unrelated package.
        assert!(!ig.prunes_package(p("/ws"), p("src/lib")));
        // Root package is never pruned.
        assert!(!ig.prunes_package(p("/ws"), p("")));
    }

    #[test]
    fn exact_dir_with_trailing_slash_matches() {
        // `node_modules/` resolves to a skip dir with a trailing slash; `Path`
        // equality is component-wise, so it still prunes the walked dir.
        let ig = Ignore::new(&[PathBuf::from("/ws/node_modules/")], &[]).unwrap();
        assert!(ig.prune_dir(p("/ws/node_modules"), p("node_modules")));
    }

    #[test]
    fn walk_root_is_never_glob_pruned() {
        // The root's rel is empty; a glob must not prune it (else the whole walk
        // yields nothing). Only an exact `dirs` entry can prune.
        let ig = Ignore::new(&[], &["**/node_modules/**".to_string()]).unwrap();
        assert!(!ig.prune_dir(p("/ws"), p("")));
    }

    #[test]
    fn empty_ignore_matches_nothing() {
        let ig = Ignore::default();
        assert!(!ig.prune_dir(p("/ws/anything"), p("anything")));
        assert!(!ig.exclude_file(p("anything")));
    }

    #[test]
    fn prune_prefix_extraction() {
        assert_eq!(prune_prefix("**/node_modules/**"), Some("**/node_modules"));
        assert_eq!(prune_prefix("vendor/**"), Some("vendor"));
        assert_eq!(prune_prefix("vendor/**/*"), Some("vendor"));
        assert_eq!(prune_prefix("**/*.tmp"), None);
        assert_eq!(prune_prefix("foo/*"), None);
        // Whole-tree patterns have no dir prefix to prune.
        assert_eq!(prune_prefix("**"), None);
    }

    /// Construction validates but does not compile: an ignore set nobody walks
    /// with costs no regex construction. The first match compiles it.
    #[test]
    fn matchers_compile_on_first_use_not_at_construction() {
        let ig = Ignore::new(&[], &["**/node_modules/**".to_string()]).unwrap();
        assert!(!ig.is_compiled(), "new() must not compile the matchers");
        assert!(ig.prune_dir(p("/ws/node_modules"), p("node_modules")));
        assert!(ig.is_compiled());
        // A clone taken after compiling shares the compiled matchers.
        assert!(ig.clone().is_compiled());
    }

    /// Validation stays at construction, so a bad `fs.skip` glob still fails
    /// where it always did — naming the pattern — rather than at first use.
    #[test]
    fn a_malformed_glob_is_rejected_at_construction() {
        for bad in ["a/**b", "{unclosed", "x/{a,**}/y"] {
            assert!(Glob::new(bad).is_err(), "{bad:?} is not actually invalid");
            let err = Ignore::new(&[], &[bad.to_string()])
                .expect_err(&format!("{bad:?} must be rejected"));
            assert!(format!("{err:#}").contains(bad), "{err:#}");
        }
    }

    /// A set that parses but whose regex is over the size limit is not a set
    /// that silently ignores nothing: `ensure_compiled` (and `file_matcher`)
    /// return the error, naming the pattern and the limit, and keep returning it.
    #[test]
    fn an_oversized_glob_set_is_an_error_not_an_empty_set() {
        let huge = "*a".repeat(20_000);
        let ig = Ignore::new(&[], &[GIT_SKIP_GLOB.to_string(), huge]).expect("it parses");
        let msg = format!("{:#}", ig.ensure_compiled().expect_err("must not compile"));
        assert!(msg.contains("2 fs.skip ignore glob(s)"), "{msg}");
        assert!(msg.contains("ignore pattern '*a*a*a"), "{msg}");
        assert!(msg.contains("regex size limit"), "{msg}");
        // Latched: the same error again, and through `file_matcher` too.
        let again = format!("{:#}", ig.ensure_compiled().expect_err("latched"));
        assert_eq!(again, msg);
        ig.file_matcher().expect_err("latched for file_matcher too");
        // And a provider's lookup fails rather than pruning nothing by glob.
        let err = ig
            .try_prunes_package(Path::new("/ws"), Path::new("pkg"))
            .expect_err("a provider lookup must fail too");
        assert_eq!(format!("{err:#}"), msg);
    }

    /// The parse-only check agrees with a full compile on what is valid.
    #[test]
    fn check_pattern_agrees_with_glob_new() {
        for g in [
            "**/.git/**",
            "vendor/**/*",
            "*.{go,rs}",
            "a/**b",
            "{unclosed",
            "[z-a]",
            "",
            "**",
            "a//b",
            "/abs/**",
        ] {
            assert_eq!(
                check_pattern(g).is_ok(),
                Glob::new(g).is_ok(),
                "{g:?}: parse-only check disagrees with Glob::new"
            );
        }
    }
}
