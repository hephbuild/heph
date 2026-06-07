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
//! Two ignore sources, both workspace-root-relative:
//!  - `dirs`: absolute directories pruned by exact path (heph home + literal
//!    `fs.skip` entries resolved against the root).
//!  - `globs`: wax glob patterns matched against root-relative paths.

use anyhow::Context as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wax::{Any, Glob, Program as _};

/// Directory + glob ignore rules for a filesystem walk. See the module docs.
#[derive(Debug, Clone)]
pub struct Ignore {
    /// Absolute directories pruned by exact path.
    dirs: Vec<PathBuf>,
    /// The raw ignore globs, kept so callers can chain them with a target's own
    /// excludes when compiling a per-target matcher.
    globs: Vec<String>,
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

impl Ignore {
    /// Builds an ignore set from absolute `dirs` (exact-path prune) and
    /// workspace-relative `globs`.
    pub fn new(dirs: &[PathBuf], globs: &[String]) -> anyhow::Result<Self> {
        let file_any = compile_any(globs.iter().map(String::as_str))?;
        // A directory is pruned when a glob names directories of that path —
        // i.e. its final component is a literal (`foo/**/bar` prunes every `bar`
        // dir under `foo`) — OR when it matches the prefix of a recursive glob
        // (`**/node_modules/**`, which only matches *files* under node_modules,
        // still prunes the dir via its `**/node_modules` prefix). A glob whose
        // last component is a wildcard (`some/*.go`) is file-only and never prunes.
        let dir_globs: Vec<&str> = globs
            .iter()
            .map(String::as_str)
            .filter(|g| names_directory(g))
            .chain(globs.iter().filter_map(|g| prune_prefix(g)))
            .collect();
        let dir_any = compile_any(dir_globs.into_iter())?;
        Ok(Self {
            dirs: dirs.to_vec(),
            globs: globs.to_vec(),
            file_any,
            dir_any,
        })
    }

    /// True if the directory at absolute `abs` (workspace-relative `rel`) must be
    /// pruned — never descended into. Either an exact `dirs` entry, or a directory
    /// whose path matches a glob / a recursive glob's prefix.
    ///
    /// The walk root itself (`rel == ""`) is never glob-pruned — you are
    /// explicitly walking it — only exact-`dirs` pruning applies.
    pub fn prune_dir(&self, abs: &Path, rel: &Path) -> bool {
        if self.dirs.iter().any(|d| d == abs) {
            return true;
        }
        !rel.as_os_str().is_empty() && self.dir_any.is_match(rel)
    }

    /// True if the file at workspace-relative `rel` is excluded by an ignore glob.
    pub fn exclude_file(&self, rel: &Path) -> bool {
        self.file_any.is_match(rel)
    }

    /// The raw ignore globs, for chaining with a target's own exclude patterns.
    pub fn globs(&self) -> &[String] {
        &self.globs
    }

    /// The compiled file-exclude matcher, reused directly as the "no extra
    /// excludes" matcher by callers that compile per-target exclude unions.
    pub fn file_matcher(&self) -> &Arc<Any<'static>> {
        &self.file_any
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

/// Compiles glob patterns into one `wax::Any`. An empty set yields an `Any` that
/// matches nothing.
fn compile_any<'a>(patterns: impl Iterator<Item = &'a str>) -> anyhow::Result<Arc<Any<'static>>> {
    let globs = patterns
        .map(|s| {
            Glob::new(s)
                .map(Glob::into_owned)
                .with_context(|| format!("invalid ignore pattern '{s}'"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(Arc::new(
        wax::any(globs).context("compiling ignore patterns")?,
    ))
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
    fn exact_dirs_are_pruned() {
        let home = PathBuf::from("/ws/.heph3");
        let ig = Ignore::new(std::slice::from_ref(&home), &[]).unwrap();
        assert!(ig.prune_dir(&home, p(".heph3")));
        assert!(!ig.prune_dir(p("/ws/src"), p("src")));
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
}
