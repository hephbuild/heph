//! The parse-only `load()` walk behind `build-lint-check` (invariant I8).
//!
//! It parses a package's BUILD files and everything they `load()`,
//! transitively, without evaluating any of them, and resolves every edge
//! through evaluation's own resolver ([`resolve_load`]). The provider runs it
//! once per check target `get` (so three times per package when all three
//! checks resolve; it is not cached) and hands the edges to the lint driver
//! in its def, so the
//! driver never resolves a load: the sandbox stages read-only inputs as
//! symlinked subtrees, which [`find_build_files`](super::super::run_file::find_build_files)
//! skips, so resolving again there would give a different answer from the
//! same code.
//!
//! Every path in a [`LoadWalk`] is workspace-relative, `/`-separated UTF-8,
//! and every collection is ordered, so the walk serializes and hashes
//! deterministically.

use crate::pluginbuildfile::run_file::{
    Resolved, find_build_files, load_cycle, parse_build_file, resolve_load, resolve_load_target,
};
use anyhow::Context;
use hmodel::htaddr::Addr;
use hwalk::CachedWalker;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// What one `load()` string in a file resolved to: [`Resolved`] with
/// workspace-relative paths.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum LoadEdge {
    /// One file, evaluated in `pkg`.
    File { path: String, pkg: String },
    /// A directory's BUILD files (sorted, non-empty), evaluated in `pkg` and
    /// merged in this order, as evaluation merges them.
    Dir {
        dir: String,
        pkg: String,
        files: Vec<String>,
    },
}

impl LoadEdge {
    /// The files the load reads, in merge order.
    pub(crate) fn files(&self) -> impl Iterator<Item = &str> {
        let files: &[String] = match self {
            Self::File { path, .. } => std::slice::from_ref(path),
            Self::Dir { files, .. } => files,
        };
        files.iter().map(String::as_str)
    }

    /// The package the loaded files are evaluated in.
    #[cfg(test)]
    pub(crate) fn pkg(&self) -> &str {
        match self {
            Self::File { pkg, .. } | Self::Dir { pkg, .. } => pkg,
        }
    }
}

/// One file the walk reached.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct WalkedFile {
    /// The package the file is evaluated in: the workspace-relative directory
    /// holding it, so a `./` load in it resolves against that directory.
    pub pkg: String,
    /// Each distinct `load()` string in the file, literally as written, and
    /// what it resolved to.
    pub loads: BTreeMap<String, LoadEdge>,
    /// The path is a symbolic link. A linked file is never owned (I1): its
    /// target may be outside the workspace, or the same physical file as
    /// another package's, so neither formatting it nor reporting it is any
    /// one package's. It is still read, and its interface still used.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub symlink: bool,
}

/// A package's load graph: its BUILD files and every file they load,
/// transitively, each recorded once however many paths reach it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct LoadWalk {
    /// The package walked.
    pub pkg: String,
    /// Every file reached, the package's own BUILD files included, keyed by
    /// workspace-relative path.
    pub files: BTreeMap<String, WalkedFile>,
}

impl LoadWalk {
    /// Every file a check of the package reads: its own files plus every file
    /// they load, transitively. The lint target's inputs.
    pub(crate) fn inputs(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// The files the package owns (I1): those evaluated in the package itself,
    /// i.e. held by its directory. That is its BUILD files (`*.BUILD` helpers
    /// included) and any file of its own directory it loads, never a file it
    /// only loads from another package. A file of this directory the package
    /// does not load is not reached, so not owned by anyone (E1); nor is a
    /// symlinked file ([`WalkedFile::symlink`]).
    pub(crate) fn owned(&self) -> impl Iterator<Item = &str> {
        self.files
            .iter()
            .filter(|(_, f)| owns(&self.pkg, f))
            .map(|(path, _)| path.as_str())
    }

    /// The fs dep of every input, `//@heph/fs:file@f=<path>`.
    pub(crate) fn input_addrs(&self) -> impl Iterator<Item = Addr> {
        self.inputs().map(hbuiltins::pluginfs::file_addr)
    }

    /// Every file after all the files it loads, so each file's loaded
    /// interfaces exist before it is checked. Deterministic: roots are taken
    /// in path order and each file's loads in load-string order.
    ///
    /// Fails on an edge naming a file the walk did not record, and on a
    /// cycle, neither of which [`walk_package`] produces; a walk read back from
    /// a def is checked rather than trusted.
    pub(crate) fn dependency_order(&self) -> anyhow::Result<Vec<&str>> {
        fn visit<'a>(
            walk: &'a LoadWalk,
            path: &'a str,
            done: &mut HashSet<&'a str>,
            stack: &mut HashSet<&'a str>,
            order: &mut Vec<&'a str>,
        ) -> anyhow::Result<()> {
            if done.contains(path) {
                return Ok(());
            }
            let file = walk
                .files
                .get(path)
                .with_context(|| format!("load graph names {path}, which it does not record"))?;
            if !stack.insert(path) {
                // Paths here are workspace-relative already: the root is "".
                return Err(load_cycle(Path::new(""), Path::new(path)));
            }
            for edge in file.loads.values() {
                for dep in edge.files() {
                    visit(walk, dep, done, stack, order)?;
                }
            }
            stack.remove(path);
            done.insert(path);
            order.push(path);
            Ok(())
        }

        let mut done = HashSet::with_capacity(self.files.len());
        let mut stack = HashSet::new();
        let mut order = Vec::with_capacity(self.files.len());
        for path in self.files.keys() {
            visit(self, path, &mut done, &mut stack, &mut order)?;
        }
        Ok(order)
    }
}

/// The ownership rule (I1): `pkg` owns `file` exactly when the file is
/// evaluated in `pkg`, which [`resolve_load`] decides from the directory
/// holding it, and is not a symlink.
pub(crate) fn owns(pkg: &str, file: &WalkedFile) -> bool {
    file.pkg == pkg && !file.symlink
}

/// Walk `pkg`'s load graph: parse its BUILD files (those matching `patterns`)
/// and every file they load, transitively, resolving each `load()` once.
///
/// A package directory with no BUILD file, or a `pkg` escaping the workspace,
/// walks to nothing, as evaluation finds no targets there. Every error
/// evaluation would raise on a load, the walk raises with the same text:
/// the resolver's, a parse error, or a `load() cycle`. Symbols are not
/// checked; nothing is evaluated.
pub(crate) fn walk_package(
    root: &Path,
    walker: &CachedWalker,
    pkg: &str,
    patterns: &[glob::Pattern],
) -> anyhow::Result<LoadWalk> {
    let mut walk = LoadWalk {
        pkg: pkg.to_string(),
        files: BTreeMap::new(),
    };
    if hmodel::htpkg::join_rel_checked("", pkg).is_err() {
        return Ok(walk);
    }
    let dir = root.join(pkg);
    if !dir.is_dir() {
        return Ok(walk);
    }
    let files = find_build_files(walker, &dir, patterns)?;
    if files.is_empty() {
        return Ok(walk);
    }
    let mut walker_state = Walker {
        root,
        walker,
        patterns,
        files: &mut walk.files,
        done_dirs: HashSet::new(),
        in_flight: HashSet::new(),
        reads: None,
    };
    // Seeded as evaluation seeds it: the package directory itself is on the
    // chain, so a load back into it is the same cycle evaluation reports.
    walker_state.visit_dir(&dir, pkg, &files)?;
    Ok(walk)
}

/// Walk the loads of one file that is not read from disk (the LSP's open
/// buffer, at workspace path `file`, evaluated in `pkg`): resolve each of
/// `modules` (its `load()` strings) as [`walk_package`] does, through
/// [`resolve_load`], and walk what they load, transitively.
///
/// Best effort: a load that fails to resolve, parse or walk (a cycle back
/// into `file` or its directory included) is left out of the edges, so its
/// names are untyped; the others still walk. Returns each walked load
/// string's edge, the walk of every file they reach, `file` never among
/// them, and what the walk read ([`WalkReads`]), failed loads included.
pub(crate) fn walk_loads<'m>(
    root: &Path,
    walker: &CachedWalker,
    pkg: &str,
    file: &Path,
    modules: impl IntoIterator<Item = &'m str>,
    patterns: &[glob::Pattern],
) -> (BTreeMap<String, LoadEdge>, LoadWalk, WalkReads) {
    let mut walk = LoadWalk {
        pkg: pkg.to_string(),
        files: BTreeMap::new(),
    };
    let mut edges = BTreeMap::new();
    // Seeded as evaluation seeds it: the file and its directory are on the
    // chain, so a load back into either is a cycle, not a second copy.
    let seed: HashSet<PathBuf> = [file.to_path_buf()]
        .into_iter()
        .chain(file.parent().map(Path::to_path_buf))
        .collect();
    let mut state = Walker {
        root,
        walker,
        patterns,
        files: &mut walk.files,
        done_dirs: HashSet::new(),
        in_flight: seed.clone(),
        reads: Some(WalkReads::default()),
    };
    for module in modules {
        if edges.contains_key(module) {
            continue;
        }
        match state.visit_load(pkg, module) {
            Ok(edge) => {
                edges.insert(module.to_string(), edge);
            }
            Err(e) => {
                tracing::debug!(module, error = %e, "lsp: load left untyped");
                // A failed visit leaves its chain behind.
                state.in_flight.clone_from(&seed);
            }
        }
    }
    let reads = state.reads.take().unwrap_or_default();
    (edges, walk, reads)
}

/// What a [`walk_loads`] read from disk: what a caller memoizing the walk
/// watches, since the walk's answer moves only when one of these does.
#[derive(Debug, Default)]
pub(crate) struct WalkReads {
    /// Every path a load probed (as [`resolve_load`] stats it: a file, a
    /// directory whose listing was read, or nothing, for a load that did not
    /// resolve) and every file parsed, whether the load succeeded or not, in
    /// the order reached. A path may repeat.
    pub paths: Vec<PathBuf>,
    /// Files parsed.
    pub parsed: u64,
}

/// One walk's state. It mirrors [`BuildFileLoader`](super::super::run_file::BuildFileLoader):
/// a directory and a file each go on the chain while their loads are being
/// walked, and are skipped once done.
struct Walker<'a> {
    root: &'a Path,
    walker: &'a CachedWalker,
    patterns: &'a [glob::Pattern],
    files: &'a mut BTreeMap<String, WalkedFile>,
    done_dirs: HashSet<PathBuf>,
    in_flight: HashSet<PathBuf>,
    /// `Some` when the caller wants to know what was read ([`walk_loads`]).
    reads: Option<WalkReads>,
}

impl Walker<'_> {
    fn visit_dir(&mut self, dir: &Path, pkg: &str, files: &[PathBuf]) -> anyhow::Result<()> {
        if self.done_dirs.contains(dir) {
            return Ok(());
        }
        if !self.in_flight.insert(dir.to_path_buf()) {
            return Err(load_cycle(self.root, dir));
        }
        for file in files {
            self.visit_file(file, pkg)?;
        }
        self.in_flight.remove(dir);
        self.done_dirs.insert(dir.to_path_buf());
        Ok(())
    }

    fn visit_file(&mut self, path: &Path, pkg: &str) -> anyhow::Result<()> {
        let rel = rel_path(self.root, path)?;
        if self.files.contains_key(&rel) {
            return Ok(());
        }
        if !self.in_flight.insert(path.to_path_buf()) {
            return Err(load_cycle(self.root, path));
        }
        if let Some(reads) = &mut self.reads {
            reads.paths.push(path.to_path_buf());
            reads.parsed += 1;
        }
        let symlink = std::fs::symlink_metadata(path)
            .with_context(|| format!("stat {rel}"))?
            .file_type()
            .is_symlink();
        let ast = parse_build_file(path)?;
        let mut loads = BTreeMap::new();
        for load in ast.loads() {
            let module = load.module_id;
            if loads.contains_key(module) {
                continue;
            }
            let edge = self
                .visit_load(pkg, module)
                .with_context(|| format!("{rel}: load(\"{module}\")"))?;
            loads.insert(module.to_string(), edge);
        }
        self.in_flight.remove(path);
        self.files.insert(
            rel,
            WalkedFile {
                pkg: pkg.to_string(),
                loads,
                symlink,
            },
        );
        Ok(())
    }

    fn visit_load(&mut self, current_pkg: &str, module: &str) -> anyhow::Result<LoadEdge> {
        // The path `resolve_load` is about to stat, recorded before it can
        // fail: a load that does not resolve now may once the path exists.
        if let Some(reads) = &mut self.reads
            && let Ok(candidate) = resolve_load_target(self.root, current_pkg, module)
        {
            reads.paths.push(candidate);
        }
        match resolve_load(self.root, self.walker, current_pkg, module, self.patterns)? {
            Resolved::File { path, pkg } => {
                self.visit_file(&path, &pkg)?;
                Ok(LoadEdge::File {
                    path: rel_path(self.root, &path)?,
                    pkg,
                })
            }
            Resolved::Dir { dir, pkg, files } => {
                self.visit_dir(&dir, &pkg, &files)?;
                Ok(LoadEdge::Dir {
                    dir: rel_path(self.root, &dir)?,
                    files: files
                        .iter()
                        .map(|f| rel_path(self.root, f))
                        .collect::<anyhow::Result<_>>()?,
                    pkg,
                })
            }
        }
    }
}

/// `path` relative to `root`, as the UTF-8 string a def and an fs addr carry.
/// [`resolve_load`] keeps every path under the root, so the prefix always
/// strips; a path that is not UTF-8 cannot name an fs dep and fails.
fn rel_path(root: &Path, path: &Path) -> anyhow::Result<String> {
    let rel = path
        .strip_prefix(root)
        .with_context(|| format!("{} is outside the workspace", path.display()))?;
    rel.to_str()
        .map(str::to_string)
        .with_context(|| format!("{} is not a UTF-8 path", rel.display()))
}
