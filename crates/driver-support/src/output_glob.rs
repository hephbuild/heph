//! Output collection that refuses to follow a directory symlink.
//!
//! Output globs used to be expanded with `glob::glob`, which treats a link to a
//! directory as a directory: on `**` it walks the host through a link to `/`,
//! recurses through a link to an ancestor until `ENAMETOOLONG`, and packs files
//! from outside the output tree through a link out of the sandbox
//! (hephbuild/heph#563). A declared file or directory output read through a
//! linked ancestor (`out/vendor/x.js` with `out/vendor -> /`) did the same
//! without a glob.
//!
//! [`OutputGlob`] is `glob::glob`'s own walk (the `Paths` iterator and
//! `fill_todo` of glob 0.3.4, MIT/Apache-2.0, The Rust Project Developers)
//! with refusals added. Everywhere else it is the same algorithm, so on a tree
//! it does not refuse it yields **the same paths, in the same order,
//! duplicates included** — the order is the tar member order, which is the
//! artifact's hashout, and nothing in the cache key names the collector. A
//! walk that changed content for an unchanged key would let an old cache
//! entry and a fresh build disagree under one hashin. So this walk never
//! changes what a target outputs: it either matches `glob::glob` or fails the
//! target.
//!
//! # For target authors
//!
//! outputs have no user doc of their own, so the rules live here:
//!
//! - **Output globs and declared paths do not follow symlinked directories.**
//!   `out = "out/**/*.js"` fails when the walk meets a linked directory, and
//!   so does `out = "out/vendor/x.js"` (or the directory output
//!   `out/vendor/lib/`) when `out/vendor` is a link. The one exception is
//!   heph's own stage links (below). Output the link itself, or the real
//!   directory.
//! - **A symlink in an artifact must stay inside its own tar.** It is
//!   relative and resolves to a file or directory packed in the same
//!   artifact. Each output group is its own tar, so a link in one group to a
//!   file in another fails the pack.
//! - **An artifact cached before these rules is still served.** The rules
//!   are checked when outputs are collected, and a cache hit collects
//!   nothing, so a target whose cache entry predates them keeps being served
//!   from it; a cold build of the same target fails.
//! - **A sandbox path holding `[`, `]`, `*` or `?`** (a checkout or heph
//!   home under such a directory) is taken literally. Before, an output glob
//!   there silently matched nothing (see "The root"). The package, though, is
//!   part of the pattern, not of the root: the exec driver turns `out =
//!   "*.txt"` in package `app/[slug]` into the glob `app/[slug]/*.txt`, which
//!   still reads `[slug]` as a character class.
//!
//! # What is refused
//!
//! Patterns are relative to the sandbox's workspace root, and the walk is
//! bounded by it. Where the walk would look inside a path (for a `**`, or for
//! a further pattern component), a symlink there is judged from the link's
//! own text, lexically — never from what it resolves to on this host, so a
//! target fails on every host or on none:
//!
//! - **a link whose target is absolute, or climbs out of the workspace**, is
//!   refused whether or not it resolves here. At a `**` position this holds
//!   even when the link is not a directory on this host: on another host it
//!   may be one.
//! - **a link to a directory inside the workspace** is refused. A relative
//!   in-workspace link that is not a directory has nothing to descend.
//! - **a `..` that climbs out of the workspace** (`../x/*`, or `.*/foo`, whose
//!   `.*` also matches `..`), or back out of a followed link.
//!
//! A link only *matched* by the last component is yielded as before and
//! packed as a link member, which the packer checks stays inside the artifact.
//! A `..` matched by the last component is yielded as before (a directory,
//! which the collector skips) and never read.
//!
//! [`check_declared_path`] holds a declared file or directory output to the
//! same rule for each component above its last.
//!
//! # heph's stage links are followed
//!
//! In the OS sandbox a read-only input is staged once into `<home>/stage` and
//! exposed in the sandbox as an absolute symlink into it
//! (`stage::place_entry`); under FUSE the same input is a real directory. A
//! link whose target is absolute and lies inside the stage directory is
//! therefore walked like a directory, so both modes match the same paths — the
//! ones `glob::glob` matched. The FUSE path passes no stage directory.
//!
//! # The root
//!
//! The walk starts at the root, not at `/`: a link above the root (`/var` on
//! macOS) is not refused, and the root is taken literally. `glob::glob` was
//! handed `root.join(pattern)` and so read the root's own path as pattern
//! text: under a root holding a glob metacharacter (`/src/[team]/repo`)
//! an output glob matched nothing, or matched by accident, and a non-UTF-8
//! root was mangled by the lossy conversion. There the walk matches what the
//! pattern says, which is the one place it packs content `glob::glob` did
//! not under an unchanged key: a deliberate change, accepted because the old
//! result was already wrong.
//!
//! # Kept per-OS behaviour
//!
//! glob tests a literal component (one with no wildcard) by asking the
//! filesystem whether `path.join(component)` exists. On a case-insensitive
//! filesystem (APFS's default on macOS) `OUT/a.txt` therefore matches
//! `out/a.txt`, yielding the path spelled as in the pattern; on Linux it
//! matches nothing. That predates this walk, and changing it would change
//! content under an unchanged key, so it is kept, accepted.

use anyhow::{Context as _, anyhow};
use glob::{MatchOptions, Pattern};
use std::ffi::OsString;
use std::fs::{self, DirEntry};
use std::path::{Component, Path, PathBuf};

/// One `/`-separated component of the pattern, with the two facts
/// `glob::Pattern` keeps private.
struct DirPattern {
    pattern: Pattern,
    /// The component is exactly `**`.
    is_recursive: bool,
    /// The component has a wildcard, so the directory must be read to match it.
    has_metachars: bool,
}

impl DirPattern {
    fn new(component: &str) -> Result<Self, glob::PatternError> {
        Ok(Self {
            pattern: Pattern::new(component)?,
            // A valid single component sets glob's `is_recursive` only when it
            // is `**` itself: `a**`, `**a` and `***` do not compile.
            is_recursive: component == "**",
            // A valid `[` always opens a class (an unclosed one does not
            // compile), so these three are exactly glob's metachars.
            has_metachars: component.contains(['?', '*', '[']),
        })
    }

    /// glob's `tokens[0] == Char('.')`: only a pattern spelled with a leading
    /// literal dot matches the `.` and `..` entries.
    fn leading_dot(&self) -> bool {
        self.pattern.as_str().starts_with('.')
    }
}

#[derive(Debug)]
struct PathWrapper {
    path: PathBuf,
    /// A directory, following a symlink (glob's own `is_directory`).
    is_directory: bool,
    /// Known when the path came from `read_dir`; looked up otherwise.
    is_symlink: Option<bool>,
}

impl PathWrapper {
    fn from_dir_entry(path: PathBuf, e: &DirEntry) -> Self {
        let file_type = e.file_type().ok();
        let is_symlink = file_type.map(|t| t.is_symlink());
        let is_directory = file_type
            .and_then(|t| {
                if t.is_symlink() {
                    None
                } else {
                    Some(t.is_dir())
                }
            })
            .or_else(|| fs::metadata(&path).map(|m| m.is_dir()).ok())
            .unwrap_or(false);
        Self {
            path,
            is_directory,
            is_symlink,
        }
    }

    fn from_path(path: PathBuf) -> Self {
        let is_directory = fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false);
        Self {
            path,
            is_directory,
            is_symlink: None,
        }
    }

    fn is_symlink(&self) -> bool {
        self.is_symlink.unwrap_or_else(|| is_symlink(&self.path))
    }
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// What bounds a walk: the workspace root it may not leave, and heph's stage
/// directory, whose links it follows.
struct Bounds {
    root: PathBuf,
    stage_dir: Option<PathBuf>,
}

type Todo = Vec<anyhow::Result<(PathWrapper, usize)>>;

/// The paths under `root` matched by the relative glob `pattern`, in
/// `glob::glob(root.join(pattern))`'s order. See the module docs for what it
/// refuses, and for `stage_dir`.
pub(crate) struct OutputGlob {
    bounds: Bounds,
    dir_patterns: Vec<DirPattern>,
    require_dir: bool,
    options: MatchOptions,
    todo: Todo,
    scope: Option<PathWrapper>,
}

impl OutputGlob {
    pub(crate) fn new(
        root: &Path,
        pattern: &str,
        stage_dir: Option<&Path>,
    ) -> anyhow::Result<Self> {
        // The root is a path, never pattern text: see the module docs.
        anyhow::ensure!(
            !Path::new(pattern).has_root(),
            "output glob {pattern:?} is absolute; an output glob is relative to the \
             workspace root"
        );
        // Whole-pattern validation first, as glob does, so an invalid pattern
        // fails here with glob's own message.
        Pattern::new(pattern).map_err(|e| anyhow!("invalid output glob {pattern:?}: {e}"))?;
        let dir_patterns = pattern
            .split_terminator(std::path::is_separator)
            .map(DirPattern::new)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow!("invalid output glob {pattern:?}: {e}"))?;
        Ok(Self {
            bounds: Bounds {
                root: root.to_path_buf(),
                stage_dir: stage_dir.map(Path::to_path_buf),
            },
            dir_patterns,
            require_dir: pattern.ends_with(std::path::is_separator),
            options: MatchOptions::new(),
            todo: Vec::new(),
            scope: Some(PathWrapper::from_path(root.to_path_buf())),
        })
    }
}

impl Iterator for OutputGlob {
    type Item = anyhow::Result<PathBuf>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(scope) = self.scope.take() {
            if self.dir_patterns.is_empty() {
                // An empty pattern: `glob::glob("<root>/")` yields the root
                // itself when it is a directory.
                return scope.is_directory.then_some(Ok(scope.path));
            }
            fill_todo(
                &mut self.todo,
                &self.dir_patterns,
                0,
                &scope,
                self.options,
                &self.bounds,
            );
        }

        loop {
            if self.dir_patterns.is_empty() {
                return None;
            }
            let (path, mut idx) = match self.todo.pop()? {
                Ok(pair) => pair,
                Err(e) => return Some(Err(e)),
            };
            let last = self.dir_patterns.len() - 1;

            // Already matched by `fill_todo` (a literal or `.`/`..` last component).
            if idx == usize::MAX {
                if self.require_dir && !path.is_directory {
                    continue;
                }
                return Some(Ok(path.path));
            }

            if self.dir_patterns.get(idx).is_some_and(|p| p.is_recursive) {
                let mut next = idx;
                while self
                    .dir_patterns
                    .get(next + 1)
                    .is_some_and(|p| p.is_recursive)
                {
                    next += 1;
                }
                if path.is_directory {
                    fill_todo(
                        &mut self.todo,
                        &self.dir_patterns,
                        next,
                        &path,
                        self.options,
                        &self.bounds,
                    );
                    if next == last {
                        return Some(Ok(path.path));
                    }
                    idx = next + 1;
                } else {
                    // `**` descends this path wherever it is a directory. A
                    // link absolute or out of the workspace is refused here
                    // too, so the target fails on every host, not only on the
                    // ones where the link resolves to a directory.
                    if let Some(e) = refuse_descent(&path, &self.bounds) {
                        return Some(Err(e));
                    }
                    if next == last {
                        continue;
                    }
                    idx = next + 1;
                }
            }

            let Some(name) = path.path.file_name().and_then(|s| s.to_str()) else {
                // glob skips non-UTF-8 names (its FIXME #9639); so must we.
                continue;
            };
            let Some(pattern) = self.dir_patterns.get(idx) else {
                continue;
            };
            if pattern.pattern.matches_with(name, self.options) {
                if idx == last {
                    if !self.require_dir || path.is_directory {
                        return Some(Ok(path.path));
                    }
                } else {
                    fill_todo(
                        &mut self.todo,
                        &self.dir_patterns,
                        idx + 1,
                        &path,
                        self.options,
                        &self.bounds,
                    );
                }
            }
        }
    }
}

/// Where a symlink points, judged from its text alone.
enum LinkTarget {
    /// Absolute, into heph's stage directory: followed.
    Stage,
    /// Relative, and stays inside the workspace.
    InRoot,
    /// Absolute outside the stage, or climbs out of the workspace.
    Foreign,
}

/// Read the link at `path` (at `rel` under the root) and judge its target.
fn classify_link(
    path: &Path,
    rel: &Path,
    bounds: &Bounds,
) -> anyhow::Result<(PathBuf, LinkTarget)> {
    let to = fs::read_link(path).with_context(|| {
        format!(
            "read the symlink {} met by output collection",
            rel.display()
        )
    })?;
    let class = if to.is_absolute() {
        let in_stage = bounds.stage_dir.as_deref().is_some_and(|stage| {
            to.strip_prefix(stage).is_ok_and(|rest| {
                rest.components()
                    .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
            })
        });
        if in_stage {
            LinkTarget::Stage
        } else {
            LinkTarget::Foreign
        }
    } else if lexical_join(rel.parent().unwrap_or(Path::new("")), &to).is_none() {
        LinkTarget::Foreign
    } else {
        LinkTarget::InRoot
    };
    Ok((to, class))
}

/// `to` resolved lexically from `from`, both root-relative; `None` when it
/// climbs above the root.
fn lexical_join(from: &Path, to: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in from.components().chain(to.components()) {
        match c {
            Component::Normal(n) => out.push(n),
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

/// The refusal for a link at `rel` the walk would go through, `None` if it
/// may (a stage link, or a non-directory link inside the workspace).
fn refuse_link(
    path: &Path,
    rel: &Path,
    is_directory: bool,
    bounds: &Bounds,
) -> Option<anyhow::Error> {
    let (to, class) = match classify_link(path, rel, bounds) {
        Ok(found) => found,
        Err(e) => return Some(e),
    };
    let (rel, to_shown) = (rel.display(), to.display());
    match class {
        LinkTarget::Stage => None,
        LinkTarget::InRoot if !is_directory => None,
        LinkTarget::InRoot => {
            let real = lexical_join(rel_parent(path, bounds), &to).unwrap_or_default();
            Some(anyhow!(
                "output would descend the symlinked directory {rel} -> {to_shown}, which heph \
                 does not follow; in `out`, list {rel} itself (the link) or the real \
                 directory {}",
                real.display()
            ))
        }
        LinkTarget::Foreign => {
            let why = if to.is_absolute() {
                "is an absolute path"
            } else {
                "climbs out of the workspace"
            };
            Some(anyhow!(
                "output would descend the symlink {rel} -> {to_shown}, which {why}; heph does \
                 not follow it, whether or not it resolves on this host. In `out`, list {rel} \
                 itself (the link), or declare what you need from its target as an input and \
                 output that"
            ))
        }
    }
}

/// The root-relative directory holding `path`.
fn rel_parent<'a>(path: &'a Path, bounds: &Bounds) -> &'a Path {
    path.strip_prefix(&bounds.root)
        .unwrap_or(path)
        .parent()
        .unwrap_or(Path::new(""))
}

/// Refuse a `..` in the root-relative `rel` that climbs out of the root, or
/// back out of a followed link (where it would climb from the link's target,
/// outside the workspace).
fn refuse_parent_dirs(root: &Path, rel: &Path, what: &str) -> Option<anyhow::Error> {
    let mut at = root.to_path_buf();
    let mut depth = 0usize;
    for c in rel.components() {
        match c {
            Component::Normal(n) => {
                at.push(n);
                depth += 1;
            }
            Component::ParentDir if depth == 0 => {
                return Some(anyhow!(
                    "{what} climbs out of the workspace through {}: outputs must stay \
                     inside the target's sandbox. In a glob, `.*` also matches `..`; use \
                     `.[!.]*` to match dotfiles only",
                    rel.display()
                ));
            }
            Component::ParentDir => {
                if is_symlink(&at) {
                    return Some(anyhow!(
                        "{what} climbs back out of the symlinked directory {} with `..` \
                         (in {}), which would leave the workspace from where the link \
                         points; spell the path without the `..`",
                        at.strip_prefix(root).unwrap_or(&at).display(),
                        rel.display()
                    ));
                }
                at.pop();
                depth -= 1;
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    None
}

/// Why the walk may not look inside `path`, if it may not.
fn refuse_descent(path: &PathWrapper, bounds: &Bounds) -> Option<anyhow::Error> {
    if path.path == bounds.root {
        return None;
    }
    let rel = path.path.strip_prefix(&bounds.root).unwrap_or(&path.path);
    if path.path.ends_with("..")
        && let Some(e) = refuse_parent_dirs(&bounds.root, rel, "output glob")
    {
        return Some(e);
    }
    if !path.is_symlink() {
        return None;
    }
    refuse_link(&path.path, rel, path.is_directory, bounds)
}

/// Refuse a declared file or directory output `rel` (relative to `root`)
/// that is read through a symlink: a `..` climbing out of the workspace, or a
/// link in any component above the last — unless it is one of heph's stage
/// links (see the module docs). The last component is the caller's: a file
/// output may be a link (packed as one, and checked by the packer), a
/// directory output must be a real directory.
pub(crate) fn check_declared_path(
    root: &Path,
    rel: &str,
    stage_dir: Option<&Path>,
) -> anyhow::Result<()> {
    let rel = Path::new(rel);
    anyhow::ensure!(
        !rel.has_root(),
        "declared output {} is absolute; an output is relative to the workspace root",
        rel.display()
    );
    if let Some(e) = refuse_parent_dirs(root, rel, "declared output") {
        return Err(e);
    }
    let bounds = Bounds {
        root: root.to_path_buf(),
        stage_dir: stage_dir.map(Path::to_path_buf),
    };
    let mut at = root.to_path_buf();
    let mut components = rel.components().peekable();
    while let Some(c) = components.next() {
        match c {
            Component::Normal(n) => at.push(n),
            Component::ParentDir => {
                at.pop();
                continue;
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => continue,
        }
        if components.peek().is_none() {
            break;
        }
        if !is_symlink(&at) {
            continue;
        }
        let is_directory = fs::metadata(&at).is_ok_and(|m| m.is_dir());
        let ancestor = at.strip_prefix(root).unwrap_or(&at).to_path_buf();
        if let Some(e) = refuse_link(&at, &ancestor, is_directory, &bounds) {
            return Err(e.context(format!("declared output {}", rel.display())));
        }
    }
    Ok(())
}

/// glob's `fill_todo`: queue what under `path` `patterns[idx]` matches,
/// refusing first if `path` may not be looked inside.
fn fill_todo(
    todo: &mut Todo,
    patterns: &[DirPattern],
    idx: usize,
    path: &PathWrapper,
    options: MatchOptions,
    bounds: &Bounds,
) {
    if let Some(e) = refuse_descent(path, bounds) {
        todo.push(Err(e));
        return;
    }
    let add = |todo: &mut Todo, next_path: PathWrapper| {
        if idx + 1 == patterns.len() {
            todo.push(Ok((next_path, usize::MAX)));
        } else {
            fill_todo(todo, patterns, idx + 1, &next_path, options, bounds);
        }
    };

    let Some(pattern) = patterns.get(idx) else {
        return;
    };
    let is_dir = path.is_directory;
    match (pattern.has_metachars, is_dir) {
        (false, _) => {
            let s = pattern.pattern.as_str();
            let special = s == "." || s == "..";
            let next_path = PathWrapper::from_path(path.path.join(s));
            if (special && is_dir)
                || (!special
                    && (fs::metadata(&next_path.path).is_ok()
                        || fs::symlink_metadata(&next_path.path).is_ok()))
            {
                add(todo, next_path);
            }
        }
        (true, true) => {
            let dirs = fs::read_dir(&path.path).and_then(|d| {
                d.map(|e| {
                    e.map(|e| {
                        let filename = e.file_name();
                        (PathWrapper::from_dir_entry(e.path(), &e), filename)
                    })
                })
                .collect::<Result<Vec<(PathWrapper, OsString)>, _>>()
            });
            match dirs {
                Ok(mut children) => {
                    // `MatchOptions::new()` never sets `require_literal_leading_dot`.
                    children.sort_by(|p1, p2| p2.1.cmp(&p1.1));
                    todo.extend(children.into_iter().map(|x| Ok((x.0, idx))));
                    if pattern.leading_dot() {
                        for special in [".", ".."] {
                            if pattern.pattern.matches_with(special, options) {
                                add(todo, PathWrapper::from_path(path.path.join(special)));
                            }
                        }
                    }
                }
                Err(e) => {
                    todo.push(Err(anyhow::Error::new(e).context(format!(
                        "read directory {} for an output glob",
                        path.path.display()
                    ))));
                }
            }
        }
        (true, false) => {}
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn touch(root: &Path, rel: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(p, b"x").expect("write");
    }

    fn ours(root: &Path, pattern: &str) -> anyhow::Result<Vec<PathBuf>> {
        OutputGlob::new(root, pattern, None)?.collect()
    }

    fn theirs(root: &Path, pattern: &str) -> Vec<PathBuf> {
        glob::glob(&root.join(pattern).to_string_lossy())
            .expect("pattern")
            .collect::<Result<Vec<_>, _>>()
            .expect("glob")
    }

    /// On a tree with no links, the walk is `glob::glob` exactly: the same
    /// paths in the same order, duplicates included, because that order is
    /// the tar's member order and so the artifact's hashout.
    #[test]
    fn matches_glob_on_a_symlink_free_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        for f in [
            "a",
            "a-b",
            "a.txt",
            "ab",
            ".hidden.txt",
            "out/a.txt",
            "out/.dot.txt",
            "out/b/a.txt",
            "out/b/x/c.txt",
            "out/x/d.txt",
            "out/x/y/x/e.txt",
            "out/x/y/z.js",
            "out/a-b/f.txt",
            "out/ab/g.txt",
            "out/a.d/h.txt",
            "pkg/[x]/lit.txt",
        ] {
            touch(root, f);
        }
        fs::create_dir_all(root.join("emptydir")).expect("mkdir");

        for pattern in [
            "*",
            "a*",
            "*.txt",
            "**/*.txt",
            "**/*",
            "**",
            "out/**",
            "out/**/*.txt",
            "out/**/x/*.txt",
            "**/x/**/*.txt",
            "out/*/x/*.txt",
            "out/a.txt",
            "out/missing.txt",
            "out/**/.*",
            "**/.*",
            ".*",
            ".[!.]*",
            "out/.[!.]*",
            "**/.[!.]*",
            "out/.*/a.txt",
            "out/./a.txt",
            "out//a.txt",
            "out/../a.txt",
            "out/",
            "out/*/",
            "out/[ab]*/*.txt",
            "out/[!a]/*.txt",
            "out/?/*.txt",
            "pkg/[[]x]/*.txt",
            "",
        ] {
            let ours = ours(root, pattern).unwrap_or_else(|e| panic!("{pattern:?}: {e:#}"));
            assert_eq!(ours, theirs(root, pattern), "pattern {pattern:?}");
        }
    }

    /// A pattern glob does not compile is refused by both.
    #[test]
    fn invalid_patterns_are_refused_like_glob() {
        let dir = tempfile::tempdir().expect("tempdir");
        for pattern in ["***", "a**", "[", "out/a**/x"] {
            assert!(
                glob::glob(&dir.path().join(pattern).to_string_lossy()).is_err(),
                "{pattern:?}: glob accepts it"
            );
            let err = OutputGlob::new(dir.path(), pattern, None)
                .err()
                .unwrap_or_else(|| panic!("{pattern:?}: accepted"));
            assert!(
                format!("{err:#}").contains("invalid output glob"),
                "{err:#}"
            );
        }
    }

    /// A dangling link inside the workspace is matched, by a wildcard or a
    /// literal, exactly as glob matches it; and a link matched (not crossed)
    /// by a literal last component is yielded like glob yields it.
    #[test]
    fn links_matched_last_are_yielded_like_glob() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        touch(root, "out/real/b.txt");
        symlink("missing", root.join("out/dangling")).expect("link");
        symlink("real", root.join("out/inside")).expect("link");

        for pattern in ["out/*", "out/dangling", "out/inside"] {
            assert_eq!(
                ours(root, pattern).unwrap_or_else(|e| panic!("{pattern:?}: {e:#}")),
                theirs(root, pattern),
                "{pattern:?}"
            );
        }
        assert_eq!(
            ours(root, "out/*").expect("glob"),
            vec![
                root.join("out/dangling"),
                root.join("out/inside"),
                root.join("out/real")
            ]
        );
        let err = ours(root, "out/inside/b.txt").expect_err("a literal crossing a link");
        assert!(
            format!("{err:#}").contains("symlinked directory out/inside"),
            "{err:#}"
        );
    }

    /// A walk that would cross a directory symlink fails instead, whatever
    /// the link points at.
    #[test]
    fn refuses_to_descend_a_directory_symlink() {
        let outer = tempfile::tempdir().expect("tempdir");
        let root = outer.path().join("ws");
        touch(&root, "out/a.txt");
        touch(outer.path(), "third_party/x/lib.js");
        touch(&root, "out/real/b.txt");
        symlink("/", root.join("out/host")).expect("link to /");
        symlink("..", root.join("out/real/up")).expect("link to ancestor");
        symlink("../../third_party/x", root.join("out/vendor")).expect("link out");
        symlink("real", root.join("out/inside")).expect("link inside");

        for (pattern, link) in [
            ("out/**/*.txt", "out/host"),
            ("out/**", "out/host"),
            ("out/host/*", "out/host"),
            ("out/vendor/*.js", "out/vendor"),
            ("out/*/*.js", "out/host"),
            ("out/real/**/*.txt", "out/real/up"),
            ("out/inside/*.txt", "out/inside"),
        ] {
            let err = ours(&root, pattern).expect_err(pattern);
            let msg = format!("{err:#}");
            assert!(
                msg.contains("heph does not follow")
                    && msg.contains(&format!(" {link} ->"))
                    && msg.contains("`out`, list"),
                "{pattern:?}: want a refusal naming {link}, got {msg}"
            );
        }
        // The fix names the real directory a link inside the workspace points at.
        let err = ours(&root, "out/inside/*.txt").expect_err("inside");
        assert!(
            format!("{err:#}").contains("the real directory out/real"),
            "{err:#}"
        );
    }

    /// A link that is absolute or climbs out is refused where it would be
    /// descended even when it resolves to nothing on this host: on another
    /// host it may be a directory.
    #[test]
    fn refuses_foreign_links_whether_or_not_they_resolve() {
        let outer = tempfile::tempdir().expect("tempdir");
        let root = outer.path().join("ws");
        touch(&root, "out/a.txt");
        symlink("/heph-no-such-dir/x", root.join("out/abs")).expect("link");
        for pattern in ["out/**/*.txt", "out/**", "out/abs/*", "out/abs/x.txt"] {
            let err = ours(&root, pattern).expect_err(pattern);
            assert!(
                format!("{err:#}")
                    .contains("out/abs -> /heph-no-such-dir/x, which is an absolute path"),
                "{pattern:?}: {err:#}"
            );
        }
        fs::remove_file(root.join("out/abs")).expect("rm");

        symlink("../../nowhere", root.join("out/up")).expect("link");
        let err = ours(&root, "out/**/*.txt").expect_err("climbs out");
        assert!(
            format!("{err:#}").contains("climbs out of the workspace"),
            "{err:#}"
        );
        fs::remove_file(root.join("out/up")).expect("rm");

        // A dangling link that stays inside is not descended anywhere: as glob.
        symlink("missing", root.join("out/dangling")).expect("link");
        for pattern in ["out/**/*.txt", "out/**", "out/dangling/*"] {
            assert_eq!(
                ours(&root, pattern).unwrap_or_else(|e| panic!("{pattern:?}: {e:#}")),
                theirs(&root, pattern),
                "{pattern:?}"
            );
        }
    }

    /// heph's own stage links are walked as directories, matching what
    /// `glob::glob` matched (and what FUSE, which presents them as real
    /// directories, matches); other links are not.
    #[test]
    fn descends_stage_links() {
        let outer = tempfile::tempdir().expect("tempdir");
        let stage = outer.path().join("stage");
        let root = outer.path().join("ws");
        touch(&stage, "h1/go/src/a.txt");
        touch(&stage, "h1/go/b.txt");
        touch(outer.path(), "secret/s.txt");
        touch(&root, "app/own.txt");
        symlink(stage.join("h1/go"), root.join("go")).expect("stage link");

        // (Not `**` or `**/*`: those match the link itself, which the packer
        // then refuses as absolute — as it did under glob.)
        for pattern in ["**/*.txt", "go/**/*.txt", "go/*", "go/src/a.txt"] {
            let got = OutputGlob::new(&root, pattern, Some(&stage))
                .expect("pattern")
                .collect::<anyhow::Result<Vec<_>>>()
                .unwrap_or_else(|e| panic!("{pattern:?}: {e:#}"));
            assert_eq!(got, theirs(&root, pattern), "{pattern:?}");
        }
        assert!(
            OutputGlob::new(&root, "**/*.txt", Some(&stage))
                .expect("pattern")
                .any(|p| p.is_ok_and(|p| p == root.join("go/src/a.txt"))),
            "the walk went through the stage link"
        );

        // Without the stage dir (or for a link not lexically inside it) it is
        // an absolute link like any other.
        let err = ours(&root, "**/*.txt").expect_err("no stage dir");
        assert!(format!("{err:#}").contains("symlink go ->"), "{err:#}");
        symlink(stage.join("../secret"), root.join("app/sneaky")).expect("link");
        let err = OutputGlob::new(&root, "app/**/*.txt", Some(&stage))
            .expect("pattern")
            .collect::<anyhow::Result<Vec<_>>>()
            .expect_err("a link through the stage dir's parent");
        assert!(format!("{err:#}").contains("app/sneaky"), "{err:#}");

        // A `..` after a followed stage link would climb from the stage.
        let err = OutputGlob::new(&root, "go/../*", Some(&stage))
            .expect("pattern")
            .collect::<anyhow::Result<Vec<_>>>()
            .expect_err("climbs back out of the stage link");
        assert!(
            format!("{err:#}").contains("climbs back out of the symlinked directory go"),
            "{err:#}"
        );
    }

    /// A `..` that climbs above the root is refused before it is read; one
    /// that stays inside, or is only matched last, is as before.
    #[test]
    fn refuses_to_climb_out_of_the_root() {
        let outer = tempfile::tempdir().expect("tempdir");
        let root = outer.path().join("ws");
        touch(&root, "out/a.txt");
        touch(outer.path(), "secret.txt");

        for pattern in ["../*.txt", ".*/secret.txt", "out/../../*.txt"] {
            let err = ours(&root, pattern).expect_err(pattern);
            let msg = format!("{err:#}");
            assert!(
                msg.contains("climbs out") && msg.contains("`.[!.]*`"),
                "{pattern:?}: {msg}"
            );
        }
        for pattern in ["out/../out/*.txt", ".*", "**/.*", ".[!.]*/secret.txt"] {
            assert_eq!(
                ours(&root, pattern).unwrap_or_else(|e| panic!("{pattern:?}: {e:#}")),
                theirs(&root, pattern),
                "{pattern:?}"
            );
        }
    }

    #[test]
    fn refuses_an_absolute_pattern() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = ours(dir.path(), "/etc/*").expect_err("absolute");
        assert!(format!("{err:#}").contains("absolute"), "{err:#}");
    }

    /// The root is a path, not pattern text: under a root holding a glob
    /// metacharacter the walk matches what the pattern says, where glob
    /// matched nothing (`src/[team]` reads as one of `t`, `e`, `a`, `m`).
    #[test]
    fn takes_a_root_holding_metacharacters_literally() {
        let outer = tempfile::tempdir().expect("tempdir");
        for name in ["src/[team]", "ws*", "ws?", "ws]"] {
            let root = outer.path().join(name);
            touch(&root, "a.txt");
            touch(&root, "d/b.txt");
            assert_eq!(
                ours(&root, "*.txt").unwrap_or_else(|e| panic!("{name}: {e:#}")),
                vec![root.join("a.txt")],
                "{name}"
            );
            assert_eq!(
                ours(&root, "**/*.txt").unwrap_or_else(|e| panic!("{name}: {e:#}")),
                vec![root.join("a.txt"), root.join("d/b.txt")],
                "{name}"
            );
        }
        assert!(
            theirs(&outer.path().join("src/[team]"), "*.txt").is_empty(),
            "glob read the root as a pattern"
        );
    }

    /// A non-UTF-8 root is walked like any other; only entry names below it
    /// are matched as text. macOS refuses to create one, so it is checked
    /// where the filesystem allows it.
    #[test]
    fn walks_a_non_utf8_root() {
        use std::os::unix::ffi::OsStrExt as _;
        let outer = tempfile::tempdir().expect("tempdir");
        let root = outer.path().join(std::ffi::OsStr::from_bytes(b"ws\xff"));
        if fs::create_dir(&root).is_err() {
            return;
        }
        touch(&root, "a.txt");
        assert_eq!(
            ours(&root, "*.txt").expect("glob"),
            vec![root.join("a.txt")]
        );
    }

    /// A declared file or directory output read through a linked ancestor is
    /// refused like a glob crossing it; the last component is the caller's,
    /// and a stage link is followed.
    #[test]
    fn declared_paths_refuse_linked_ancestors() {
        let outer = tempfile::tempdir().expect("tempdir");
        let stage = outer.path().join("stage");
        let root = outer.path().join("ws");
        touch(&root, "out/real/x.js");
        touch(&stage, "h/go/a.txt");
        symlink("/", root.join("out/host")).expect("link");
        symlink("real", root.join("out/inside")).expect("link");
        symlink("/heph-no-such-dir", root.join("out/abs")).expect("link");
        symlink(stage.join("h/go"), root.join("go")).expect("stage link");

        for (rel, want) in [
            (
                "out/host/etc/hosts",
                "symlink out/host -> /, which is an absolute path",
            ),
            ("out/inside/x.js", "symlinked directory out/inside -> real"),
            ("out/abs/x", "symlink out/abs -> /heph-no-such-dir"),
            ("../x", "climbs out of the workspace"),
            ("out/../../x", "climbs out of the workspace"),
            (
                "go/../out/real/x.js",
                "climbs back out of the symlinked directory go",
            ),
            ("/etc/hosts", "is absolute"),
        ] {
            let err = check_declared_path(&root, rel, Some(&stage)).expect_err(rel);
            let msg = format!("{err:#}");
            assert!(msg.contains(want), "{rel}: want {want:?} in {msg}");
        }
        for rel in [
            "out/real/x.js",
            "out/inside",
            "out/host",
            "out/real",
            "go/a.txt",
            "go",
            "out/./real/x.js",
            "out/missing/x",
        ] {
            check_declared_path(&root, rel, Some(&stage))
                .unwrap_or_else(|e| panic!("{rel}: {e:#}"));
        }
        let err = check_declared_path(&root, "go/a.txt", None).expect_err("no stage dir");
        assert!(format!("{err:#}").contains("symlink go ->"), "{err:#}");
    }
}
