//! Path-rewriting views over an existing [`Content`].
//!
//! A view answers "take this target's outputs, keep a subset, and put them at
//! different paths" without copying a byte. [`ViewContent`] wraps another
//! `Content` and rewrites entry *paths* as they stream out of
//! [`Content::walk`]; the `Read` handle for each file's data is handed through
//! untouched, so materializing a view costs exactly what materializing the
//! source cost. Nothing is packed, stored, or duplicated.
//!
//! That is the whole point: relocation today means an exec target running `cp`,
//! which buys a sandbox, a subprocess, and a second full copy of the bytes in
//! both the local and the remote cache. A view has none of those. Its producer
//! marks it uncacheable, so no revision is ever written for it — the *source*
//! target stays cached exactly as before, and the view is re-derived (a few
//! string operations over a path list) on each build.
//!
//! # Hashing
//!
//! [`ViewContent::hashout`] is derived from `(source hashout, transform)`
//! rather than read off the rewritten bytes. That is sound because the
//! transform is a pure function of the path set: identical source content plus
//! an identical transform can only produce identical output. It is also what
//! keeps the view free — computing a true content hash would mean reading every
//! byte, which is the copy we are trying to avoid.
//!
//! # Symlinks
//!
//! Entry paths are rewritten; symlink *targets* are not. A relative symlink
//! interior to the tree survives `strip_prefix`/`prefix` (both move every path
//! by the same amount, so relative distances are preserved) but can dangle if
//! `include`/`exclude`/`rename` move or drop only one end of the pair.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;

use super::{Content, WalkEntry, WalkEntryKind};

/// A declarative, hash-stable rewrite of a set of relative artifact paths.
///
/// Field order below is also the order the rules apply, and the docs on
/// [`PathTransform::resolve`] spell out the interactions. The type derives
/// `Hash` over the *pattern strings* (not compiled globs) so a driver can fold
/// it straight into its target-def hash — which is what makes a changed
/// transform invalidate consumers' cache keys.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct PathTransform {
    /// Glob patterns a path must match to be kept. Empty keeps everything.
    pub include: Vec<String>,
    /// Glob patterns that drop a path. Applied after `include`.
    pub exclude: Vec<String>,
    /// Leading directory removed from every path under it, in either the
    /// emitted or the package-relative form. Paths not under it are left alone;
    /// a prefix matching *nothing* is an error (see [`PathTransform::plan`]).
    pub strip_prefix: Option<String>,
    /// Leading directory prepended to every surviving path.
    pub prefix: Option<String>,
    /// Where selected outputs are placed — see [`Rename`].
    pub rename: Rename,
}

/// How a transform places the outputs it selects.
///
/// Two forms, because two different things are being asked for:
///
/// * [`Rename::Sole`] — "put this dep's output *here*". No source is named at
///   all: it renames whatever survives `include`/`exclude`, which must be
///   exactly one file. This is the form that never makes an author know where a
///   dependency emitted anything, since there is no emitted path to spell.
/// * [`Rename::Exact`] — "move precisely these paths". Keys are full emitted
///   paths, matched exactly. More typing and more coupling, but it moves
///   several files at once and leaves the rest to `strip_prefix`/`prefix`.
///
/// Reach for `Sole` by default and `Exact` when you genuinely need per-path
/// control.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Rename {
    /// No renaming; destinations come from `strip_prefix`/`prefix`.
    #[default]
    None,
    /// Destination for the single output that survives `include`/`exclude`.
    Sole(String),
    /// Full emitted path → destination. Entries are kept regardless of
    /// `include`/`exclude` — naming a file explicitly is a stronger signal than
    /// a glob — and are placed verbatim, skipping `strip_prefix`/`prefix`.
    Exact(BTreeMap<String, String>),
}

impl Rename {
    pub fn is_none(&self) -> bool {
        matches!(self, Rename::None)
    }
}

impl PathTransform {
    /// Whether this transform would leave every path untouched. A driver uses
    /// this to keep its cheap path — there is no reason to build a view that
    /// rewrites nothing.
    pub fn is_identity(&self) -> bool {
        self.include.is_empty()
            && self.exclude.is_empty()
            && self.strip_prefix.is_none()
            && self.prefix.is_none()
            && self.rename.is_none()
    }

    /// Whether `strip_prefix`/`prefix` would be dead config.
    ///
    /// [`Rename::Sole`] fixes the destination of the only surviving file, so a
    /// prefix would be computed and then thrown away. Callers reject the
    /// combination rather than silently ignoring half the config.
    pub fn prefixes_are_dead_config(&self) -> bool {
        matches!(self.rename, Rename::Sole(_))
            && (self.strip_prefix.is_some() || self.prefix.is_some())
    }

    /// Whether a path survives `include`/`exclude`. Shared by
    /// [`plan`](Self::plan) and [`resolve`](Self::resolve) so the set `plan`
    /// counts is exactly the set `resolve` emits.
    fn survives(
        &self,
        full: &str,
        rel: Option<&str>,
        include: &[wax::Glob<'static>],
        exclude: &[wax::Glob<'static>],
    ) -> bool {
        (include.is_empty() || matches_either(include, full, rel))
            && !matches_either(exclude, full, rel)
    }

    /// Resolve the transform over one artifact's source paths, producing the
    /// source → destination mapping (paths that are filtered out are absent).
    ///
    /// Errors only on a *collision* — two sources landing on one destination,
    /// which would otherwise mean one output silently clobbering another with
    /// the winner decided by iteration order.
    ///
    /// Takes the `plan` from [`PathTransform::plan`] rather than deciding
    /// `rename` itself: `rename` applies to whatever survives filtering, and
    /// "is that exactly one file?" is only answerable across every artifact the
    /// transform covers. Typo checks live there for the same reason.
    ///
    /// Rules, in order:
    ///
    /// 1. `include` (when non-empty) must match, then `exclude` must not.
    /// 2. The one path the `plan` selected goes exactly where `rename` says.
    /// 3. Otherwise `strip_prefix` is removed if the path is under it,
    /// 4. and `prefix` is prepended.
    pub fn resolve(&self, src: &SourcePaths, plan: &RenamePlan) -> anyhow::Result<PathMapping> {
        let include = compile(&self.include).context("compiling `include` patterns")?;
        let exclude = compile(&self.exclude).context("compiling `exclude` patterns")?;

        let mut map: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
        // Reverse index for the collision message: which source claimed a
        // destination first. Built alongside rather than derived after, so the
        // error can name both sides.
        let mut claimed: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();

        for path in &src.paths {
            let full = path_to_slash(path);
            let rel = src.package_relative(&full);

            // The plan is consulted before filtering: a `Rename::Exact` entry
            // names a file outright, which outranks a glob. (A `Rename::Sole`
            // entry is by construction already a survivor, so the order is
            // immaterial for it.)
            let dst = if let Some(to) = plan.destination(&full) {
                PathBuf::from(to)
            } else {
                if !self.survives(&full, rel, &include, &exclude) {
                    continue;
                }
                let stripped = self.strip(&full, rel);
                match self.prefix.as_deref() {
                    Some(p) => PathBuf::from(format!("{}/{}", p.trim_end_matches('/'), stripped)),
                    None => PathBuf::from(stripped),
                }
            };

            if let Some(prev) = claimed.get(&dst) {
                anyhow::bail!(
                    "path collision: '{}' and '{}' both map to '{}' — \
                     narrow `include`/`exclude`, or give one an explicit `rename`",
                    path_to_slash(prev),
                    full,
                    dst.display(),
                );
            }
            claimed.insert(dst.clone(), path.clone());
            map.insert(path.clone(), dst);
        }

        Ok(PathMapping(map))
    }

    /// Apply `strip_prefix` to one path, in whichever form it was written.
    ///
    /// Stripping the package-relative form drops the package prefix with it, so
    /// `strip_prefix = "build/out"` and `strip_prefix = "app/build/out"` both
    /// land `app/build/out/server` on `server`. Matching by shape rather than
    /// by spelling is what keeps the two consistent.
    fn strip<'a>(&self, full: &'a str, rel: Option<&'a str>) -> &'a str {
        let Some(sp) = self.strip_prefix.as_deref() else {
            return full;
        };
        let sp = sp.trim_end_matches('/');
        if let Some(stripped) = strip_dir_prefix(full, sp) {
            return stripped;
        }
        if let Some(rel) = rel
            && let Some(stripped) = strip_dir_prefix(rel, sp)
        {
            return stripped;
        }
        full
    }

    /// Decide which single path `rename` applies to, across `sources` — the
    /// full set of artifacts this transform covers — and reject patterns that
    /// match nothing.
    ///
    /// Runs once, up front; its output is what [`resolve`](Self::resolve)
    /// consults. The split is not tidiness. `rename` names no source — it
    /// renames *whatever survives* `include`/`exclude* — so "is that exactly
    /// one file?" is a question about every artifact at once. Asked per
    /// artifact, a group over two deps that each emit one file would rename
    /// both to the same destination.
    ///
    /// The union is also the only correct scope for the typo check on
    /// `strip_prefix`: one applying to a single dep and not another is fine,
    /// while one applying to *no* dep is a mistake that would silently pass
    /// every file through untouched.
    ///
    /// An empty set is vacuously fine — there is nothing to have typo'd
    /// against, and a target with no outputs yet is not an error.
    pub fn plan(&self, sources: &[SourcePaths]) -> anyhow::Result<RenamePlan> {
        let all: Vec<PathBuf> = sources.iter().flat_map(|s| s.paths.clone()).collect();
        if all.is_empty() {
            return Ok(RenamePlan::default());
        }

        let mut plan: BTreeMap<String, String> = BTreeMap::new();
        match &self.rename {
            Rename::None => {}
            Rename::Sole(dst) => {
                let include = compile(&self.include).context("compiling `include` patterns")?;
                let exclude = compile(&self.exclude).context("compiling `exclude` patterns")?;

                let mut survivors: Vec<String> = Vec::new();
                for src in sources {
                    for path in &src.paths {
                        let full = path_to_slash(path);
                        let rel = src.package_relative(&full);
                        if self.survives(&full, rel, &include, &exclude) {
                            survivors.push(full);
                        }
                    }
                }
                survivors.sort();

                match survivors.as_slice() {
                    [] => anyhow::bail!(
                        "`rename` has nothing to rename — no output survived \
                         `include`/`exclude`{}",
                        nearest_hint(&all),
                    ),
                    [only] => {
                        plan.insert(only.clone(), dst.clone());
                    }
                    many => anyhow::bail!(
                        "`rename` is a single destination but {} outputs are selected \
                         ({}). Narrow them with `include` so exactly one is left, use a \
                         dict to name each path, or use `strip_prefix`/`prefix` to \
                         relocate them together.",
                        many.len(),
                        many.join(", "),
                    ),
                }
            }
            Rename::Exact(map) => {
                let present: std::collections::BTreeSet<String> =
                    all.iter().map(|p| path_to_slash(p)).collect();
                for (key, dst) in map {
                    if !present.contains(key) {
                        anyhow::bail!(
                            "`rename` key '{key}' matched no output path — dict keys are \
                             matched exactly against the paths a target emits. Use the \
                             string form (`rename = \"{dst}\"`) to place the selected \
                             output without naming its source{}",
                            nearest_hint(&all),
                        );
                    }
                    plan.insert(key.clone(), dst.clone());
                }
            }
        }

        if let Some(sp) = self.strip_prefix.as_deref() {
            let trimmed = sp.trim_end_matches('/');
            let matched = sources.iter().any(|s| {
                s.paths.iter().any(|p| {
                    let full = path_to_slash(p);
                    let rel = s.package_relative(&full);
                    strip_dir_prefix(&full, trimmed).is_some()
                        || rel.is_some_and(|r| strip_dir_prefix(r, trimmed).is_some())
                })
            });
            if !matched {
                anyhow::bail!(
                    "`strip_prefix` '{sp}' matched no output path{}",
                    nearest_hint(&all),
                );
            }
        }
        Ok(RenamePlan(plan))
    }
}

/// Which emitted path each rename resolved to, produced by
/// [`PathTransform::plan`] and consumed by [`PathTransform::resolve`].
///
/// Keyed by the *full emitted path* rather than by anything the author wrote,
/// so applying it is an exact lookup with no matching left to redo — and, for
/// [`Rename::Sole`], the "exactly one survivor" decision happens once,
/// globally, rather than once per artifact.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenamePlan(BTreeMap<String, String>);

impl RenamePlan {
    /// Where this path was sent, if a rename selected it.
    pub fn destination(&self, full_path: &str) -> Option<&str> {
        self.0.get(full_path).map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// One dep's contribution to a transform: the paths it emitted, plus the
/// package that emitted them.
///
/// The package is what lets an author write patterns without knowing where a
/// dependency's outputs land. Heph emits artifact paths workspace-relative, so
/// a target in `//app` declaring `out = "build/out/server"` really emits
/// `app/build/out/server` — a path the author of a *consuming* group has no
/// reason to know, and which changes if the dep ever moves package. Carrying
/// the package here means `strip_prefix = "build/out"` (what the dep's own
/// BUILD file says) works, and so does the full form.
#[derive(Debug, Clone, Default)]
pub struct SourcePaths {
    /// Package of the target that produced `paths`, e.g. `app/sub`. `None`
    /// when unknown, which simply disables the package-relative forms.
    pub package: Option<String>,
    pub paths: Vec<PathBuf>,
}

impl SourcePaths {
    pub fn new(package: Option<String>, paths: Vec<PathBuf>) -> Self {
        Self { package, paths }
    }

    /// `full` with the producing package stripped, when it is under it.
    fn package_relative<'a>(&self, full: &'a str) -> Option<&'a str> {
        let pkg = self.package.as_deref()?.trim_matches('/');
        if pkg.is_empty() {
            return None;
        }
        strip_dir_prefix(full, pkg)
    }
}

/// Whether any glob matches the path in either the emitted or the
/// package-relative form, so `include = ["**/*.so"]` and
/// `include = ["app/**/*.so"]` both work.
fn matches_either(globs: &[wax::Glob<'static>], full: &str, rel: Option<&str>) -> bool {
    matches_any(globs, full) || rel.is_some_and(|r| matches_any(globs, r))
}

/// The resolved source → destination mapping produced by
/// [`PathTransform::resolve`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathMapping(BTreeMap<PathBuf, PathBuf>);

impl PathMapping {
    /// Destination for a source path, or `None` when it was filtered out.
    pub fn get(&self, src: &Path) -> Option<&Path> {
        self.0.get(src).map(PathBuf::as_path)
    }

    /// Every `(source, destination)` pair, in sorted source order. Drives the
    /// `inspect` rendering, so a user can see where each file went.
    pub fn pairs(&self) -> impl Iterator<Item = (&Path, &Path)> {
        self.0.iter().map(|(s, d)| (s.as_path(), d.as_path()))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Compile a pattern list into wax programs. Empty in, empty out — callers
/// treat an empty `include` as "match everything", which is not the same as a
/// program that matches nothing.
fn compile(patterns: &[String]) -> anyhow::Result<Vec<wax::Glob<'static>>> {
    patterns
        .iter()
        .map(|p| {
            wax::Glob::new(p)
                .map(wax::Glob::into_owned)
                .with_context(|| format!("invalid glob pattern '{p}'"))
        })
        .collect()
}

fn matches_any(globs: &[wax::Glob<'static>], path: &str) -> bool {
    use wax::Program as _;
    globs.iter().any(|g| g.is_match(path))
}

/// Strip `prefix` from `path` at a directory boundary, so `build/out` does not
/// match `build/output/x`. Returns `None` when `path` is not under `prefix`.
fn strip_dir_prefix<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix.is_empty() {
        return Some(path);
    }
    path.strip_prefix(prefix)?.strip_prefix('/')
}

/// Artifact paths are always relative and slash-separated on the wire; render
/// them that way so patterns and `rename` keys are written the same on every
/// platform.
fn path_to_slash(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// "did you mean" tail for a miss, listing a few real paths. Cheap and only
/// built on the error path, where the user has already lost more time than this
/// costs.
fn nearest_hint(paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        return String::new();
    }
    let sample: Vec<String> = paths.iter().take(8).map(|p| path_to_slash(p)).collect();
    let more = paths.len().saturating_sub(sample.len());
    let tail = if more > 0 {
        format!(" (and {more} more)")
    } else {
        String::new()
    };
    format!(". Available paths: {}{}", sample.join(", "), tail)
}

/// The compiled form of a dep reference's inline filter list — the
/// `[a.go,pkg/**]` suffix on `//foo:bar[…]`.
///
/// One type so every place that honours filters agrees on what they mean:
/// sandbox unpack, read-only staging, the FUSE slot index, output-collision
/// detection, and the source map. They used to each open-code `Path::new(f) ==
/// rel`, which meant exact paths only and five chances to drift.
///
/// A pattern that does not compile as a glob falls back to an exact
/// string match. That is what makes this a strict superset of the old
/// behaviour: every literal path keeps matching itself, and a filename
/// containing glob metacharacters that a user meant literally still works
/// rather than becoming a hard error.
#[derive(Debug, Default)]
pub struct PathFilter {
    /// Compiled globs, paired with nothing — a pattern that failed to compile
    /// is in `literals` instead.
    globs: Vec<wax::Glob<'static>>,
    literals: Vec<String>,
    empty: bool,
}

impl PathFilter {
    /// Compile a filter list. Never fails: an uncompilable pattern degrades to
    /// an exact match rather than rejecting the build.
    pub fn new(patterns: &[String]) -> Self {
        let mut globs = Vec::new();
        let mut literals = Vec::new();
        for p in patterns {
            match wax::Glob::new(p).map(wax::Glob::into_owned) {
                Ok(g) => globs.push(g),
                Err(_) => literals.push(p.clone()),
            }
        }
        Self {
            empty: patterns.is_empty(),
            globs,
            literals,
        }
    }

    /// True when no filtering was requested — callers use this to keep their
    /// whole-tree fast paths (a single directory symlink instead of a per-file
    /// walk), so it must stay distinct from "a filter that matches everything".
    pub fn is_empty(&self) -> bool {
        self.empty
    }

    /// Whether `rel` (a path relative to the artifact root) is exposed.
    /// An empty filter set exposes everything.
    pub fn matches(&self, rel: &Path) -> bool {
        if self.empty {
            return true;
        }
        let s = path_to_slash(rel);
        matches_any(&self.globs, &s) || self.literals.contains(&s)
    }
}

/// A [`Content`] that presents another `Content` under rewritten paths.
///
/// Construction is cheap and infallible; the transform is resolved lazily on
/// first use (and re-resolved per call, matching how `walk` is already a
/// fresh-stream-per-call API). See the module docs for what this does and does
/// not copy.
pub struct ViewContent {
    source: std::sync::Arc<dyn Content>,
    transform: PathTransform,
    /// Package of the target that produced `source`, enabling the
    /// package-relative pattern forms — see [`SourcePaths`].
    package: Option<String>,
    /// The globally-resolved `rename` bindings from [`PathTransform::plan`].
    /// Held rather than recomputed because key matching is only correct across
    /// every artifact the transform covers, not this one.
    plan: RenamePlan,
}

impl ViewContent {
    pub fn new(
        source: std::sync::Arc<dyn Content>,
        transform: PathTransform,
        package: Option<String>,
        plan: RenamePlan,
    ) -> Self {
        Self {
            source,
            transform,
            package,
            plan,
        }
    }

    pub fn transform(&self) -> &PathTransform {
        &self.transform
    }

    /// The source's paths, tagged with the producing package.
    ///
    /// Uses [`Content::entry_paths`], which a tar-backed cache artifact answers
    /// with a header-only seek scan — no file data is read.
    pub fn source_paths(&self) -> anyhow::Result<SourcePaths> {
        let paths = self
            .source
            .entry_paths()
            .context("listing source artifact paths for view")?;
        Ok(SourcePaths::new(self.package.clone(), paths))
    }

    /// Resolve the transform against the source's current path set.
    pub fn mapping(&self) -> anyhow::Result<PathMapping> {
        self.transform.resolve(&self.source_paths()?, &self.plan)
    }
}

impl Content for ViewContent {
    /// Re-pack the rewritten tree as a tar stream.
    ///
    /// This is the one path that touches file bytes, and it exists only for
    /// consumers that want a raw container rather than a walk. The
    /// materialization path used by staging and sandbox setup goes through
    /// [`Content::walk`], which copies nothing.
    fn reader(&self) -> anyhow::Result<Box<dyn std::io::Read>> {
        let mut buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut buf);
            for entry in self.walk()? {
                let mut entry = entry?;
                match &mut entry.kind {
                    WalkEntryKind::File { data, x } => {
                        let mut bytes = Vec::new();
                        std::io::copy(data, &mut bytes).with_context(|| {
                            format!("read view entry {:?} while packing", entry.path)
                        })?;
                        let mut header = tar::Header::new_gnu();
                        header.set_size(bytes.len() as u64);
                        header.set_mode(if *x { 0o755 } else { 0o644 });
                        header.set_entry_type(tar::EntryType::Regular);
                        header.set_cksum();
                        builder
                            .append_data(&mut header, &entry.path, bytes.as_slice())
                            .with_context(|| format!("pack view entry {:?}", entry.path))?;
                    }
                    WalkEntryKind::Symlink { target } => {
                        let mut header = tar::Header::new_gnu();
                        header.set_size(0);
                        header.set_entry_type(tar::EntryType::Symlink);
                        header.set_cksum();
                        builder
                            .append_link(&mut header, &entry.path, &*target)
                            .with_context(|| format!("pack view symlink {:?}", entry.path))?;
                    }
                }
            }
            builder.finish().context("finish view tar")?;
        }
        Ok(Box::new(std::io::Cursor::new(buf)))
    }

    /// Stream the source's entries, rewriting each path and dropping the ones
    /// the transform filtered out. File data is forwarded by handle — never
    /// read here.
    fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>> {
        let mapping = self.mapping()?;
        let inner = self.source.walk().context("walk view source")?;
        Ok(Box::new(inner.filter_map(move |entry| match entry {
            Err(e) => Some(Err(e)),
            Ok(entry) => mapping.get(&entry.path).map(|dst| {
                Ok(WalkEntry {
                    path: dst.to_path_buf(),
                    kind: entry.kind,
                })
            }),
        })))
    }

    /// Derived from the source hashout and the transform — see the module docs
    /// for why that is sound and why it is not the rewritten bytes' hash.
    ///
    /// It deliberately does *not* fold in the [`RenamePlan`], even though the
    /// plan can in principle shift without this source changing: a `rename` key
    /// binds to the best-tier match across every dep, so a *sibling* dep gaining
    /// or losing a better match rebinds it. That is covered, one level up. The
    /// plan is a function of the transform and the full path set, and any change
    /// to a sibling's paths changes that sibling's own hashout (paths are part
    /// of its content hash) — or, if the sibling appeared or vanished entirely,
    /// changes the group's `deps` and so its def hash. Either way the consumer's
    /// `hashin` moves, because it folds in every dep hashout. Hashing the plan
    /// here would add nothing and would make two artifacts' identities depend on
    /// each other.
    fn hashout(&self) -> anyhow::Result<String> {
        use std::hash::Hash as _;

        let source = self
            .source
            .hashout()
            .context("source hashout for view artifact")?;
        // An empty source hashout means the backing content is not
        // content-addressed; propagate that rather than inventing an identity
        // for it, so callers keying on hashout treat the view the same way they
        // already treat its source.
        if source.is_empty() {
            return Ok(String::new());
        }
        let mut h = xxhash_rust::xxh3::Xxh3::new();
        h.update(b"heph-view-v1\0");
        h.update(source.as_bytes());
        h.update(&[0]);
        let mut fold = FoldHasher(&mut h);
        self.transform.hash(&mut fold);
        Ok(format!("{:x}", h.digest()))
    }

    fn entry_paths(&self) -> anyhow::Result<Vec<PathBuf>> {
        Ok(self.mapping()?.0.into_values().collect())
    }

    /// Deliberately `None` (the trait default) for both this and
    /// [`Content::file_path`]: the backing bytes are a container whose internal
    /// paths are *not* the ones this view presents, so handing a caller the raw
    /// file or a seek handle would hand it the pre-rewrite tree.
    fn byte_size(&self) -> Option<u64> {
        self.source.byte_size()
    }
}

/// Adapts `std::hash::Hasher` onto xxh3 so [`PathTransform`]'s derived `Hash`
/// can fold into the same digest as the literal writes around it.
struct FoldHasher<'a>(&'a mut xxhash_rust::xxh3::Xxh3);

impl std::hash::Hasher for FoldHasher<'_> {
    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    fn finish(&self) -> u64 {
        self.0.digest()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn transform() -> PathTransform {
        PathTransform::default()
    }

    fn src(pkg: Option<&str>, paths: &[&str]) -> SourcePaths {
        SourcePaths::new(
            pkg.map(str::to_string),
            paths.iter().map(|s| p(s)).collect(),
        )
    }

    fn resolved(t: &PathTransform, paths: &[&str]) -> anyhow::Result<Vec<(String, String)>> {
        resolved_in(t, None, paths)
    }

    fn resolved_in(
        t: &PathTransform,
        pkg: Option<&str>,
        paths: &[&str],
    ) -> anyhow::Result<Vec<(String, String)>> {
        let source = src(pkg, paths);
        let plan = t.plan(std::slice::from_ref(&source))?;
        Ok(t.resolve(&source, &plan)?
            .pairs()
            .map(|(s, d)| (path_to_slash(s), path_to_slash(d)))
            .collect())
    }

    #[test]
    fn identity_transform_is_detected() {
        assert!(transform().is_identity());
        assert!(
            !PathTransform {
                prefix: Some("lib".into()),
                ..transform()
            }
            .is_identity()
        );
    }

    #[test]
    fn include_globs_select_a_subset() {
        let t = PathTransform {
            include: vec!["**/*.so".into()],
            ..transform()
        };
        assert_eq!(
            resolved(&t, &["a/x.so", "a/x.txt", "b/y.so"]).unwrap(),
            vec![
                ("a/x.so".into(), "a/x.so".into()),
                ("b/y.so".into(), "b/y.so".into())
            ]
        );
    }

    #[test]
    fn exclude_applies_after_include() {
        let t = PathTransform {
            include: vec!["**/*.so".into()],
            exclude: vec!["**/test_*".into()],
            ..transform()
        };
        assert_eq!(
            resolved(&t, &["a/x.so", "a/test_y.so"]).unwrap(),
            vec![("a/x.so".into(), "a/x.so".into())]
        );
    }

    #[test]
    fn strip_prefix_then_prefix() {
        let t = PathTransform {
            strip_prefix: Some("build/out".into()),
            prefix: Some("lib".into()),
            ..transform()
        };
        assert_eq!(
            resolved(&t, &["build/out/server", "build/out/a/b.so"]).unwrap(),
            vec![
                ("build/out/a/b.so".into(), "lib/a/b.so".into()),
                ("build/out/server".into(), "lib/server".into()),
            ]
        );
    }

    /// `build/out` must not swallow `build/output` — the prefix is a directory,
    /// not a string.
    #[test]
    fn strip_prefix_respects_directory_boundaries() {
        let t = PathTransform {
            strip_prefix: Some("build/out".into()),
            ..transform()
        };
        assert_eq!(
            resolved(&t, &["build/out/x", "build/output/y"]).unwrap(),
            vec![
                ("build/out/x".into(), "x".into()),
                // untouched: not under `build/out`
                ("build/output/y".into(), "build/output/y".into()),
            ]
        );
    }

    #[test]
    fn rename_is_exact_and_bypasses_prefix_rules() {
        let t = PathTransform {
            prefix: Some("lib".into()),
            rename: Rename::Exact(BTreeMap::from([("build/out/server".into(), "bin/myserver".into())])),
            ..transform()
        };
        assert_eq!(
            resolved(&t, &["build/out/server", "other.txt"]).unwrap(),
            vec![
                ("build/out/server".into(), "bin/myserver".into()),
                ("other.txt".into(), "lib/other.txt".into()),
            ]
        );
    }

    /// A renamed path is kept even when `include` would have dropped it —
    /// naming a file explicitly is a stronger signal than a glob.
    #[test]
    fn rename_overrides_include_filtering() {
        let t = PathTransform {
            include: vec!["**/*.so".into()],
            rename: Rename::Exact(BTreeMap::from([("README.md".into(), "docs/README.md".into())])),
            ..transform()
        };
        assert_eq!(
            resolved(&t, &["x.so", "README.md", "ignored.txt"]).unwrap(),
            vec![
                ("README.md".into(), "docs/README.md".into()),
                ("x.so".into(), "x.so".into()),
            ]
        );
    }

    fn validated(t: &PathTransform, paths: &[&str]) -> anyhow::Result<()> {
        t.plan(&[src(None, paths)]).map(|_| ())
    }

    fn validated_in(t: &PathTransform, pkg: Option<&str>, paths: &[&str]) -> anyhow::Result<()> {
        t.plan(&[src(pkg, paths)]).map(|_| ())
    }

    #[test]
    fn unmatched_rename_key_is_an_error_with_available_paths() {
        let t = PathTransform {
            rename: Rename::Exact(BTreeMap::from([("build/out/sever".into(), "bin/s".into())])),
            ..transform()
        };
        let err = validated(&t, &["build/out/server"]).expect_err("must reject typo");
        let msg = format!("{err:#}");
        assert!(msg.contains("build/out/sever"), "{msg}");
        assert!(msg.contains("build/out/server"), "{msg}");
    }

    #[test]
    fn unmatched_strip_prefix_is_an_error() {
        let t = PathTransform {
            strip_prefix: Some("dist".into()),
            ..transform()
        };
        let err = validated(&t, &["build/out/server"]).expect_err("must reject typo");
        assert!(format!("{err:#}").contains("dist"));
    }

    /// An empty source set has nothing to typo against, so a prefix that
    /// matches nothing is vacuous rather than wrong.
    #[test]
    fn unmatched_strip_prefix_on_empty_input_is_ok() {
        let t = PathTransform {
            strip_prefix: Some("dist".into()),
            ..transform()
        };
        assert!(validated(&t, &[]).is_ok());
        assert_eq!(resolved(&t, &[]).unwrap(), vec![]);
    }

    /// The reason validation is split out of `resolve`: a target relocating two
    /// deps has a `strip_prefix` that legitimately covers one and not the
    /// other. Checked against the union it passes; checked per-artifact it
    /// would reject the second dep.
    #[test]
    fn strip_prefix_matching_only_some_paths_is_valid_across_the_union() {
        let t = PathTransform {
            strip_prefix: Some("build/out".into()),
            ..transform()
        };
        assert!(validated(&t, &["build/out/server", "assets/logo.png"]).is_ok());
        // And `resolve` alone never complains about it, whichever dep it sees —
        // which is why the typo check lives in `plan`, over the union.
        assert!(
            t.resolve(&src(None, &["assets/logo.png"]), &RenamePlan::default())
                .is_ok()
        );
    }

    /// `resolve` still catches collisions — that check *is* per-artifact-correct
    /// and must not have moved out with the typo checks.
    #[test]
    fn resolve_still_rejects_collisions_without_validate() {
        let t = PathTransform {
            strip_prefix: Some("a".into()),
            ..transform()
        };
        assert!(resolved(&t, &["a/x", "x"]).is_err());
    }

    // ---- writing patterns without knowing where a dep emitted ----
    //
    // Heph emits `<pkg>/<declared out path>`. An author writing a group must
    // not have to know that prefix, nor a dep's internal build layout.

    /// The string form names no source at all, so however deeply a dep buried
    /// its output — and whatever package it lives in — the author writes only
    /// the destination.
    #[test]
    fn sole_rename_needs_no_knowledge_of_the_source_path() {
        let t = PathTransform {
            rename: Rename::Sole("bin/myserver".into()),
            ..transform()
        };
        assert_eq!(
            resolved_in(&t, Some("app"), &["app/build/out/server"]).unwrap(),
            vec![("app/build/out/server".into(), "bin/myserver".into())]
        );
    }

    /// With more than one output selected there is no single "it" to rename,
    /// so this fails loudly and lists the candidates rather than picking one.
    #[test]
    fn sole_rename_rejects_more_than_one_survivor() {
        let t = PathTransform {
            rename: Rename::Sole("bin/s".into()),
            ..transform()
        };
        let err = validated_in(&t, Some("app"), &["app/a/server", "app/b/server"])
            .expect_err("must reject an ambiguous sole rename");
        let msg = format!("{err:#}");
        assert!(msg.contains("app/a/server"), "{msg}");
        assert!(msg.contains("app/b/server"), "{msg}");
        assert!(msg.contains("include"), "should suggest narrowing: {msg}");
    }

    /// `include` is how you narrow to one — the documented fix for the error
    /// above.
    #[test]
    fn include_narrows_a_sole_rename_to_one_survivor() {
        let t = PathTransform {
            include: vec!["**/*.so".into()],
            rename: Rename::Sole("lib/out.so".into()),
            ..transform()
        };
        assert_eq!(
            resolved_in(&t, Some("app"), &["app/x.so", "app/notes.txt"]).unwrap(),
            vec![("app/x.so".into(), "lib/out.so".into())]
        );
    }

    /// Survivors are counted across every dep, not per artifact — otherwise a
    /// group over two single-file deps would rename both to one destination.
    #[test]
    fn sole_rename_counts_survivors_across_deps() {
        let t = PathTransform {
            rename: Rename::Sole("bin/s".into()),
            ..transform()
        };
        let err = t
            .plan(&[
                src(Some("app"), &["app/server"]),
                src(Some("web"), &["web/server"]),
            ])
            .expect_err("must count survivors across deps");
        assert!(format!("{err:#}").contains("2 outputs"));
    }

    #[test]
    fn sole_rename_with_nothing_selected_is_an_error() {
        let t = PathTransform {
            include: vec!["**/*.so".into()],
            rename: Rename::Sole("lib/out.so".into()),
            ..transform()
        };
        let err = validated_in(&t, Some("app"), &["app/notes.txt"])
            .expect_err("nothing to rename must be an error");
        assert!(format!("{err:#}").contains("nothing to rename"));
    }

    /// The dict form keeps its original contract: keys are exact emitted paths.
    #[test]
    fn exact_rename_keys_are_matched_exactly() {
        let t = PathTransform {
            rename: Rename::Exact(BTreeMap::from([("app/a/server".into(), "bin/s".into())])),
            ..transform()
        };
        assert_eq!(
            resolved_in(&t, Some("app"), &["app/a/server", "app/b/other"]).unwrap(),
            vec![
                ("app/a/server".into(), "bin/s".into()),
                ("app/b/other".into(), "app/b/other".into()),
            ]
        );
        // A file name is not an exact path, and is rejected rather than guessed at.
        let loose = PathTransform {
            rename: Rename::Exact(BTreeMap::from([("server".into(), "bin/s".into())])),
            ..transform()
        };
        let err = validated_in(&loose, Some("app"), &["app/a/server"])
            .expect_err("dict keys must be exact");
        let msg = format!("{err:#}");
        assert!(msg.contains("exactly"), "{msg}");
        assert!(
            msg.contains("string form"),
            "should point at the ergonomic form: {msg}"
        );
    }

    /// A string `rename` makes `strip_prefix`/`prefix` dead config; the driver
    /// rejects the combination rather than silently dropping them.
    #[test]
    fn sole_rename_flags_prefixes_as_dead_config() {
        assert!(
            PathTransform {
                prefix: Some("lib".into()),
                rename: Rename::Sole("bin/s".into()),
                ..transform()
            }
            .prefixes_are_dead_config()
        );
        // The dict form coexists with them: it places what it names, the
        // prefixes place the rest.
        assert!(
            !PathTransform {
                prefix: Some("lib".into()),
                rename: Rename::Exact(BTreeMap::from([("a".into(), "b".into())])),
                ..transform()
            }
            .prefixes_are_dead_config()
        );
    }

    /// `strip_prefix` is written as the producing package declares it — no
    /// package prefix required — and both spellings land on the same result.
    #[test]
    fn strip_prefix_accepts_the_package_relative_form() {
        let pkg_relative = PathTransform {
            strip_prefix: Some("build/out".into()),
            ..transform()
        };
        let full = PathTransform {
            strip_prefix: Some("app/build/out".into()),
            ..transform()
        };
        let paths = ["app/build/out/server"];
        assert_eq!(
            resolved_in(&pkg_relative, Some("app"), &paths).unwrap(),
            vec![("app/build/out/server".into(), "server".into())]
        );
        assert_eq!(
            resolved_in(&full, Some("app"), &paths).unwrap(),
            resolved_in(&pkg_relative, Some("app"), &paths).unwrap(),
            "both spellings must produce the same layout"
        );
    }

    #[test]
    fn include_accepts_the_package_relative_form() {
        let t = PathTransform {
            include: vec!["build/**".into()],
            ..transform()
        };
        assert_eq!(
            resolved_in(&t, Some("app"), &["app/build/x.so", "app/docs/readme"]).unwrap(),
            vec![("app/build/x.so".into(), "app/build/x.so".into())]
        );
    }

    /// A typo'd dict key is caught and the message lists the real paths, so the
    /// fix is a copy-paste rather than a hunt.
    #[test]
    fn unknown_exact_rename_key_errors_with_the_available_paths() {
        let t = PathTransform {
            rename: Rename::Exact(BTreeMap::from([
                ("app/build/out/sever".into(), "bin/s".into()),
            ])),
            ..transform()
        };
        let err = validated_in(&t, Some("app"), &["app/build/out/server"])
            .expect_err("typo must still be caught");
        let msg = format!("{err:#}");
        assert!(msg.contains("sever"), "{msg}");
        assert!(msg.contains("app/build/out/server"), "{msg}");
    }

    /// Dict keys reach across deps: a key naming a second dep's path resolves
    /// against the union, not just the artifact being mapped.
    #[test]
    fn exact_rename_keys_resolve_across_separate_deps() {
        let t = PathTransform {
            rename: Rename::Exact(BTreeMap::from([("web/server".into(), "bin/s".into())])),
            ..transform()
        };
        let sources = [
            src(Some("app"), &["app/server"]),
            src(Some("web"), &["web/server"]),
        ];
        let plan = t.plan(&sources).expect("plan");
        assert_eq!(plan.destination("web/server"), Some("bin/s"));
        assert_eq!(plan.destination("app/server"), None);
    }

    #[test]
    fn collision_is_an_error_naming_both_sources() {
        let t = PathTransform {
            strip_prefix: Some("a".into()),
            ..transform()
        };
        // `a/x` -> `x`, and a literal `x` is already there.
        let err = resolved(&t, &["a/x", "x"]).expect_err("must reject collision");
        let msg = format!("{err:#}");
        assert!(msg.contains("a/x"), "{msg}");
        assert!(msg.contains("collision"), "{msg}");
    }

    #[test]
    fn invalid_glob_is_reported_with_the_pattern() {
        let t = PathTransform {
            include: vec!["a/{unclosed".into()],
            ..transform()
        };
        let err = resolved(&t, &["a/x"]).expect_err("must reject bad glob");
        assert!(format!("{err:#}").contains("a/{unclosed"));
    }

    // ---- PathFilter (inline `//foo:bar[…]` dep filters) ----

    fn filter(patterns: &[&str]) -> PathFilter {
        PathFilter::new(
            &patterns
                .iter()
                .map(|s| (*s).to_string())
                .collect::<Vec<_>>(),
        )
    }

    /// An empty filter list must stay distinguishable from "matches
    /// everything": callers key their whole-tree symlink fast path on it.
    #[test]
    fn empty_filter_is_empty_and_matches_everything() {
        let f = filter(&[]);
        assert!(f.is_empty());
        assert!(f.matches(&p("anything/at/all.go")));
    }

    /// The compatibility guarantee: every filter that worked before this became
    /// glob-aware must still select exactly the same files.
    #[test]
    fn exact_paths_still_match_exactly() {
        let f = filter(&["pkg/a.go", "b.go"]);
        assert!(!f.is_empty());
        assert!(f.matches(&p("pkg/a.go")));
        assert!(f.matches(&p("b.go")));
        assert!(!f.matches(&p("pkg/b.go")));
        assert!(!f.matches(&p("pkg/a.go.bak")));
        // An exact filter must not behave like a prefix.
        assert!(!f.matches(&p("pkg/a.go/nested")));
    }

    #[test]
    fn glob_patterns_select_by_shape() {
        let f = filter(&["**/*.go"]);
        assert!(f.matches(&p("a.go")));
        assert!(f.matches(&p("pkg/sub/a.go")));
        assert!(!f.matches(&p("pkg/a.rs")));
    }

    #[test]
    fn directory_globs_select_a_subtree() {
        let f = filter(&["bin/**"]);
        assert!(f.matches(&p("bin/server")));
        assert!(f.matches(&p("bin/sub/tool")));
        assert!(!f.matches(&p("lib/server")));
    }

    #[test]
    fn mixed_exact_and_glob_patterns_both_apply() {
        let f = filter(&["README.md", "src/**/*.rs"]);
        assert!(f.matches(&p("README.md")));
        assert!(f.matches(&p("src/a/b.rs")));
        assert!(!f.matches(&p("src/a/b.go")));
    }

    /// A pattern that is not a valid glob degrades to an exact match rather
    /// than failing the build — the old behaviour for anything wax rejects.
    #[test]
    fn uncompilable_pattern_falls_back_to_an_exact_match() {
        let f = filter(&["a/{unclosed"]);
        assert!(f.matches(&p("a/{unclosed")));
        assert!(!f.matches(&p("a/other")));
    }

    // ---- ViewContent ----

    struct FakeSource {
        entries: Vec<(String, Vec<u8>)>,
        hashout: String,
    }

    impl Content for FakeSource {
        fn reader(&self) -> anyhow::Result<Box<dyn std::io::Read>> {
            anyhow::bail!("not used")
        }
        fn walk(
            &self,
        ) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>> {
            Ok(Box::new(self.entries.iter().map(|(path, data)| {
                Ok(WalkEntry {
                    path: PathBuf::from(path),
                    kind: WalkEntryKind::File {
                        data: Box::new(std::io::Cursor::new(data.clone())),
                        x: false,
                    },
                })
            })))
        }
        fn hashout(&self) -> anyhow::Result<String> {
            Ok(self.hashout.clone())
        }
        fn entry_paths(&self) -> anyhow::Result<Vec<PathBuf>> {
            Ok(self.entries.iter().map(|(p, _)| PathBuf::from(p)).collect())
        }
    }

    fn source(entries: &[(&str, &str)]) -> Arc<dyn Content> {
        Arc::new(FakeSource {
            entries: entries
                .iter()
                .map(|(p, d)| ((*p).to_string(), d.as_bytes().to_vec()))
                .collect(),
            hashout: "sourcehash".to_string(),
        })
    }

    #[test]
    fn view_walk_rewrites_paths_and_preserves_data() {
        let view = ViewContent::new(source(&[("build/out/server", "elf"), ("build/out/x.txt", "hi")]), PathTransform {
                strip_prefix: Some("build/out".into()),
                prefix: Some("lib".into()),
                ..transform()
            }, None, RenamePlan::default());

        let mut got: Vec<(String, String)> = Vec::new();
        for entry in view.walk().expect("walk") {
            let mut entry = entry.expect("entry");
            let WalkEntryKind::File { ref mut data, .. } = entry.kind else {
                panic!("expected file");
            };
            let mut s = String::new();
            std::io::Read::read_to_string(data, &mut s).expect("read");
            got.push((path_to_slash(&entry.path), s));
        }
        got.sort();
        assert_eq!(
            got,
            vec![
                ("lib/server".to_string(), "elf".to_string()),
                ("lib/x.txt".to_string(), "hi".to_string()),
            ]
        );
    }

    #[test]
    fn view_walk_drops_filtered_entries() {
        let view = ViewContent::new(source(&[("a.so", "x"), ("a.txt", "y")]), PathTransform {
                include: vec!["**/*.so".into()],
                ..transform()
            }, None, RenamePlan::default());
        assert_eq!(view.entry_paths().unwrap(), vec![p("a.so")]);
        assert_eq!(view.walk().unwrap().count(), 1);
    }

    /// The cache-key property the whole design rests on: a different transform
    /// over identical source content must produce a different hashout, or a
    /// consumer would reuse a stale entry after its dep was relocated.
    #[test]
    fn view_hashout_changes_with_the_transform() {
        let src = source(&[("a/x", "data")]);
        let a = ViewContent::new(Arc::clone(&src), PathTransform {
                prefix: Some("lib".into()),
                ..transform()
            }, None, RenamePlan::default())
        .hashout()
        .unwrap();
        let b = ViewContent::new(Arc::clone(&src), PathTransform {
                prefix: Some("bin".into()),
                ..transform()
            }, None, RenamePlan::default())
        .hashout()
        .unwrap();
        let c = ViewContent::new(Arc::clone(&src), PathTransform {
                prefix: Some("lib".into()),
                ..transform()
            }, None, RenamePlan::default())
        .hashout()
        .unwrap();

        assert_ne!(a, b, "transform must be part of the view's identity");
        assert_eq!(a, c, "same transform must be stable across constructions");
        assert_ne!(a, "sourcehash", "view must not inherit the source hashout");
    }

    #[test]
    fn view_hashout_changes_with_the_source() {
        let t = PathTransform {
            prefix: Some("lib".into()),
            ..transform()
        };
        let a = ViewContent::new(source(&[("a/x", "one")]), t.clone(), None, RenamePlan::default())
            .hashout()
            .unwrap();
        let mut other = FakeSource {
            entries: vec![("a/x".to_string(), b"one".to_vec())],
            hashout: "different".to_string(),
        };
        other.hashout = "different".to_string();
        let b = ViewContent::new(Arc::new(other), t, None, RenamePlan::default()).hashout().unwrap();
        assert_ne!(a, b, "source hashout must be part of the view's identity");
    }

    /// A non-content-addressed source (empty hashout) stays non-content-addressed
    /// rather than acquiring a synthetic identity.
    #[test]
    fn view_hashout_propagates_empty_source_hashout() {
        let src: Arc<dyn Content> = Arc::new(FakeSource {
            entries: vec![],
            hashout: String::new(),
        });
        let view = ViewContent::new(
            src,
            PathTransform {
                prefix: Some("lib".into()),
                ..transform()
            },
            None,
            RenamePlan::default(),
        );
        assert_eq!(view.hashout().unwrap(), "");
    }

    #[test]
    fn view_reader_packs_rewritten_paths() {
        let view = ViewContent::new(source(&[("build/out/server", "elf")]), PathTransform {
                strip_prefix: Some("build/out".into()),
                ..transform()
            }, None, RenamePlan::default());
        let mut buf = Vec::new();
        std::io::copy(&mut view.reader().expect("reader"), &mut buf).expect("copy");

        let mut archive = tar::Archive::new(std::io::Cursor::new(buf));
        let names: Vec<String> = archive
            .entries()
            .expect("entries")
            .map(|e| {
                e.expect("entry")
                    .path()
                    .expect("path")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, vec!["server".to_string()]);
    }

    #[test]
    fn view_surfaces_collision_errors_from_walk() {
        let view = ViewContent::new(source(&[("a/x", "d"), ("x", "e")]), PathTransform {
                strip_prefix: Some("a".into()),
                ..transform()
            }, None, RenamePlan::default());
        assert!(view.walk().is_err(), "collision must surface from walk");
    }
}
