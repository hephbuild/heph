//! `failure_out`: what a failed run leaves behind for the human fixing it.
//!
//! When a managed run fails with a process exit, the managed layer copies each
//! of the target's [`FailureOutput`] matches out of the sandbox's workspace into
//! `<sandbox>/.heph-failure-out/<workspace-relative path>`, while the sandbox
//! (and a FUSE slot) is still live, and attaches that directory to the error as
//! a [`FailureArtifacts`](hplugin::error::FailureArtifacts) entry
//! ([`hplugin::error::failure_out_artifact`]). The engine renames it into the
//! target's failure slot as `out/`.
//!
//! - **Only after a process exit** ([`kept_failure_exit`]) **or the run's own
//!   `timeout`** ([`Cancellable::timed_out`]): never on a request's cancellation,
//!   never under `--shell`, never for a failure no process exit is behind
//!   (sandbox prep, credentials, a runner).
//! - **Never in the write-back.** A path a `failure_out` entry matches is
//!   left out of an `in_place` write-back ([`FailureOutMatch`]), so a
//!   diagnostic inside an `in_place` directory never reaches the tree.
//! - **Inside the workspace.** A path with anything but plain components
//!   (absolute, `..`, `.`) is skipped with a `warn!`; exec never produces one,
//!   and the ABI refuses one at decode.
//! - **Stoppable, and never recreating the sandbox.** The copy polls a stop
//!   between files, and creates the staging directory with `create_dir`: a
//!   job left running after its run was dropped stops, or fails on the
//!   removed sandbox, rather than writing into it again.
//! - **Copied** with `std::fs::copy`, which reflinks where the filesystem can.
//!   Never a hardlink and never a rename of the original: a hardlink would
//!   share an inode with a staged input, and an edit in the slot would change
//!   that input under the same key.
//! - **Lenient.** A path that matches nothing is skipped, and so is a file
//!   that cannot be read. A symlink is followed
//!   only when it resolves inside the sandbox's workspace or the shared stage
//!   (a staged read-only input is a link into it); a dangling one, a loop, or
//!   one leaving both is skipped. Inside a copied directory, a link to a
//!   directory is never followed, so no walk can loop.
//! - **Byte-exact names.** Paths are joined as `OsStr`s; a glob cannot match a
//!   name that is not UTF-8, but a directory copied whole keeps every name.
//! - **Never hides the failure.** A collect error is a `warn!` and a context
//!   line on the target's own error.

use anyhow::Context as _;
use hcore::hasync::Cancellable;
use hplugin::driver::targetdef::path::{CodegenMode, Content};
use hplugin::driver::targetdef::{FailureOutput, TargetDef};
use hplugin::error::Exit;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

/// The directory, directly under the run's sandbox, a failed run's
/// `failure_out` matches are copied into. Outside `ws/`, so the run can never
/// have written it, and nothing the run wrote is mistaken for it.
pub const FAILURE_OUT_STAGING: &str = ".heph-failure-out";

/// How the `failure_out` globs match: `*` stays within one component, and a
/// leading dot is matched like any other character.
const GLOB_OPTS: glob::MatchOptions = glob::MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

/// Whether a failed run keeps its `failure_out`: [`kept_failure_exit`], or a
/// run stopped by its own `timeout` ([`Cancellable::timed_out`]: the engine
/// cancelled its token as timed out), which the engine reports as a failure
/// and whose process was killed like a signalled one. Never under `--shell`.
pub fn keeps_failure_out(
    err: &anyhow::Error,
    shell: bool,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> bool {
    !shell && (ctoken.timed_out() || kept_failure_exit(err, shell, ctoken).is_some())
}

/// Whether every component of `p` is a plain name: no root, no `..`, no `.`.
/// A `failure_out` path is workspace-relative; anything else could reach out
/// of the staging directory, or copy a file onto itself.
pub fn is_plain_relative(p: &Path) -> bool {
    p.components().all(|c| matches!(c, Component::Normal(_)))
}

/// What a target's `failure_out` matches, as a predicate on workspace-relative
/// paths: what the `in_place` write-back leaves out (I1), so a diagnostic
/// written inside an `in_place` directory never reaches the tree.
///
/// The same reading as the collect: a file or directory path matches itself
/// and everything under it; a glob matches a path, or any directory above it
/// (a matched directory is kept whole). A glob that does not compile matches
/// nothing, as it collects nothing.
#[derive(Debug, Default)]
pub struct FailureOutMatch {
    paths: Vec<PathBuf>,
    globs: Vec<glob::Pattern>,
}

impl FailureOutMatch {
    pub fn new(outputs: &[FailureOutput]) -> Self {
        let mut m = Self::default();
        for content in outputs.iter().flat_map(|o| o.paths.iter()) {
            match content {
                Content::FilePath(p) | Content::DirPath(p) => {
                    let p = PathBuf::from(p.trim_end_matches('/'));
                    if !p.as_os_str().is_empty() {
                        m.paths.push(p);
                    }
                }
                Content::Glob(g) => {
                    if let Ok(g) = glob::Pattern::new(g) {
                        m.globs.push(g);
                    }
                }
            }
        }
        m
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.globs.is_empty()
    }

    /// Whether `rel` (workspace-relative) is, or is under, a match.
    pub fn matches(&self, rel: &Path) -> bool {
        if self.paths.iter().any(|p| rel.starts_with(p)) {
            return true;
        }
        !self.globs.is_empty()
            && rel
                .ancestors()
                .filter(|a| !a.as_os_str().is_empty())
                .filter_map(Path::to_str)
                .any(|a| self.globs.iter().any(|g| g.matches_with(a, GLOB_OPTS)))
    }
}

/// The process exit a failed run keeps what it left behind for, or `None`
/// when it keeps nothing.
///
/// The one "may we keep anything" predicate, checked before any path is
/// touched: not `--shell`, not cancelled, and a process exit behind the error
/// (an exit code from 1 to 255, or a terminating signal). A heph-side failure
/// before or around the run carries no process exit.
///
/// The `in_place` write-back on failure narrows it further (not `--frozen`, not
/// hash-only, an exit code from 1 to 127, `InPlace { write_on_failure: true }`)
/// on top of this, never beside it.
pub fn kept_failure_exit(
    err: &anyhow::Error,
    shell: bool,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> Option<Exit> {
    if shell || ctoken.is_cancelled() || hplugin::error::is_cancelled(err) {
        return None;
    }
    hplugin::error::process_exit(err).filter(|e| e.is_failure_status())
}

/// Whether a path in `mode` is written back over the tree after a failed run:
/// `InPlace { write_on_failure: true }` (exec's `in_place`, `go_format`).
pub fn writes_back_on_failure(mode: &CodegenMode) -> bool {
    matches!(
        mode,
        CodegenMode::InPlace {
            write_on_failure: true
        }
    )
}

/// Whether `target` has any path [`writes_back_on_failure`].
pub fn target_writes_back_on_failure(target: &TargetDef) -> bool {
    target
        .outputs
        .iter()
        .flat_map(|o| o.paths.iter())
        .any(|p| writes_back_on_failure(&p.codegen_tree))
}

/// D8: the exits a write-back on failure follows — a code from 1 to 127.
/// A signal, or a code of 128 and above (bash's report of a killed child: 137
/// for an OOM kill, 143 for a timeout), may have cut a fixer off mid-write.
pub fn write_back_exit(exit: Exit) -> bool {
    matches!(exit, Exit::Code(1..=127))
}

/// The managed layer's half of the D16 write-back predicate:
/// [`kept_failure_exit`] narrowed to [`write_back_exit`].
///
/// `--frozen` is not a property of the run request, so the managed layer
/// cannot see it, nor a hash-only request (which never runs a driver at all):
/// the engine re-checks the whole predicate before writing anything.
pub fn write_back_on_failure_exit(
    err: &anyhow::Error,
    shell: bool,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> Option<Exit> {
    kept_failure_exit(err, shell, ctoken).filter(|e| write_back_exit(*e))
}

/// Collect the fixes a failed `in_place` run left behind and attach them to
/// `err` as a [`FailureWriteBack`](hplugin::error::FailureWriteBack), for the
/// engine to write over the tree.
///
/// Only when [`write_back_on_failure_exit`] holds and the target has a
/// [`writes_back_on_failure`] path; a target without one pays nothing. The
/// tars are packed under `tar_parent` (a directory that outlives the run's
/// slot guards), leniently (D18), leaving out every path the target's
/// `failure_out` matches ([`FailureOutMatch`]). Never another error in `err`'s
/// place: a collect error is a `warn!` and one context line (I7); a pack
/// stopped by a cancellation adds nothing.
///
/// Never on a timeout: its token is cancelled, which
/// [`write_back_on_failure_exit`] refuses, so the pack stops with the run.
pub async fn keep_failure_write_back(
    err: anyhow::Error,
    target: &TargetDef,
    hashin: &str,
    shell: bool,
    ctoken: &(dyn Cancellable + Send + Sync),
    ws_dir: &Path,
    tar_parent: &Path,
) -> anyhow::Error {
    if !target_writes_back_on_failure(target)
        || write_back_on_failure_exit(&err, shell, ctoken).is_none()
    {
        return err;
    }
    let outputs = target.outputs.clone();
    let excluded = FailureOutMatch::new(&target.failure_outputs);
    let hashin = hashin.to_owned();
    let (ws_dir, tar_parent) = (ws_dir.to_path_buf(), tar_parent.to_path_buf());
    let collected = crate::stop::run_blocking(ctoken, move |stop| {
        crate::driver_managed::collect_write_back_inner(
            &outputs,
            &excluded,
            &hashin,
            &ws_dir,
            &tar_parent,
            stop,
        )
    })
    .await;
    match collected {
        Ok(artifacts) => hplugin::error::with_failure_write_back(err, artifacts),
        Err(e) if hplugin::error::is_cancelled(&e) => {
            tracing::debug!(error = %format!("{e:#}"), "in_place fixes of a failed run: collect stopped");
            err
        }
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "collecting in_place fixes of a failed run");
            err.context(format!("in_place fixes not written: {e:#}"))
        }
    }
}

/// Keep `outputs`' matches for a failed run: copy them out of `ws_dir` into
/// `<staging_parent>/.heph-failure-out` and attach that directory to `err`.
///
/// `follow_roots` are the directories, besides `ws_dir`, a symlink may resolve
/// into and still be followed (the shared stage). Returns `err` unchanged when
/// the run keeps nothing ([`keeps_failure_out`]), nothing matched, or the
/// copy was stopped; with a `warn!` and one context line when collecting
/// failed — never another error in its place.
///
/// The copy stops when the request is cancelled, or this future dropped. A
/// timed-out run's token is already cancelled, so its copy stops only on the
/// drop: the engine waits out its grace for it.
pub async fn keep_failure_outputs(
    err: anyhow::Error,
    outputs: &[FailureOutput],
    shell: bool,
    ctoken: &(dyn Cancellable + Send + Sync),
    ws_dir: &Path,
    staging_parent: &Path,
    follow_roots: &[PathBuf],
) -> anyhow::Error {
    if outputs.is_empty() || !keeps_failure_out(&err, shell, ctoken) {
        return err;
    }
    let timed_out = ctoken.timed_out();
    let outputs = outputs.to_vec();
    let staging = staging_parent.join(FAILURE_OUT_STAGING);
    let mut roots = Vec::with_capacity(follow_roots.len() + 1);
    roots.push(ws_dir.to_path_buf());
    roots.extend_from_slice(follow_roots);
    let ws_dir = ws_dir.to_path_buf();
    // Off the worker: a Playwright report is thousands of files.
    let job_staging = staging.clone();
    let job = move |stop: &(dyn Fn() -> bool + Sync)| {
        collect_failure_outputs(&outputs, &ws_dir, &job_staging, &roots, stop)
    };
    let copied = if timed_out {
        crate::stop::run_blocking_until_dropped(job).await
    } else {
        crate::stop::run_blocking(ctoken, job).await
    };
    match copied {
        Ok(0) => err,
        Err(e) if hplugin::error::is_cancelled(&e) => {
            tracing::debug!(error = %format!("{e:#}"), "keeping failure_out: copy stopped");
            err
        }
        Ok(_) => match hplugin::error::failure_out_artifact(&staging) {
            Some(a) => hplugin::error::with_failure_artifacts(err, vec![a]),
            None => {
                tracing::warn!(dir = %staging.display(), "failure_out staging dir is not UTF-8; not kept");
                err.context("failure_out not kept: its staging directory is not UTF-8")
            }
        },
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "keeping failure_out");
            err.context(format!("failure_out not kept: {e:#}"))
        }
    }
}

/// The `failure_out` path or glob `content`, when it is workspace-relative
/// ([`is_plain_relative`]); `None`, with a `warn!`, when it is not.
fn plain_content(content: &Content) -> Option<&Content> {
    let raw = match content {
        Content::FilePath(p) | Content::DirPath(p) | Content::Glob(p) => p,
    };
    if is_plain_relative(Path::new(raw)) {
        Some(content)
    } else {
        tracing::warn!(path = %raw, "failure_out path is not workspace-relative; skipped");
        None
    }
}

/// Copy every match of `outputs` under `ws_dir` into `staging`, at its
/// workspace-relative path. Returns how many files were copied; with none,
/// `staging` is left absent.
///
/// `stop` is polled before each file; once it reads true the copy returns
/// [`crate::stop::cancelled`]. `staging` is created with `create_dir` and
/// every directory under it one level at a time, so a parent that is gone (a
/// sandbox removed under a job its run abandoned) fails the copy instead of
/// being created again.
pub fn collect_failure_outputs(
    outputs: &[FailureOutput],
    ws_dir: &Path,
    staging: &Path,
    roots: &[PathBuf],
    stop: &dyn Fn() -> bool,
) -> anyhow::Result<usize> {
    let roots: Vec<PathBuf> = roots
        .iter()
        .filter_map(|r| fs::canonicalize(r).ok())
        .collect();
    let mut matched: BTreeSet<PathBuf> = BTreeSet::new();
    for output in outputs {
        for content in output.paths.iter().filter_map(plain_content) {
            match content {
                Content::FilePath(p) | Content::DirPath(p) => {
                    let rel = PathBuf::from(p.trim_end_matches('/'));
                    if !rel.as_os_str().is_empty() {
                        matched.insert(rel);
                    }
                }
                Content::Glob(pattern) => glob_matches(ws_dir, pattern, &roots, &mut matched)
                    .with_context(|| format!("matching failure_out glob {pattern:?}"))?,
            }
        }
    }

    let mut copier = Copier {
        staging,
        roots: &roots,
        stop,
        staging_made: false,
    };
    let mut copied = 0;
    // Sorted component-wise, a directory's descendants follow it directly:
    // once it is copied whole, they are skipped.
    let mut last_dir: Option<&Path> = None;
    for rel in &matched {
        if last_dir.is_some_and(|d| rel.starts_with(d)) {
            continue;
        }
        let src = ws_dir.join(rel);
        // A link in a parent would carry the lstat below out of the sandbox.
        if !src.parent().is_some_and(|p| inside(p, &roots)) {
            tracing::debug!(path = %rel.display(), "failure_out path is not under the sandbox; skipped");
            continue;
        }
        let n = copier
            .entry(&src, rel, true)
            .with_context(|| format!("copying failure_out {}", rel.display()))?;
        if n.dir {
            last_dir = Some(rel);
        }
        copied += n.files;
    }
    // Only a staging dir this job saw or made, and only while its run still
    // owns the sandbox.
    if copied == 0 && copier.staging_made && !stop() {
        match fs::remove_dir_all(staging) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                tracing::debug!(dir = %staging.display(), error = %e, "removing an empty failure_out staging dir");
            }
            _ => {}
        }
    }
    Ok(copied)
}

/// Whether `path`, every link resolved, is inside one of `roots` (already
/// canonical). A path that does not resolve is not.
fn inside(path: &Path, roots: &[PathBuf]) -> bool {
    fs::canonicalize(path).is_ok_and(|real| roots.iter().any(|r| real.starts_with(r)))
}

/// Add every workspace-relative path under `ws_dir` that `pattern` matches.
///
/// Walks from the pattern's literal prefix without following links, rather
/// than through `glob::glob`, which follows a linked directory on `**`: a link
/// to `/` would walk the host, and a link to an ancestor would recurse until
/// the path is too long. A matched directory is not descended into: it is
/// copied whole.
fn glob_matches(
    ws_dir: &Path,
    pattern: &str,
    roots: &[PathBuf],
    matched: &mut BTreeSet<PathBuf>,
) -> anyhow::Result<()> {
    let compiled = glob::Pattern::new(pattern).context("compiling the pattern")?;
    let prefix: PathBuf = pattern
        .split('/')
        .take_while(|c| !c.contains(['*', '?', '[']))
        .collect();
    let base = ws_dir.join(&prefix);
    if !inside(&base, roots) {
        return Ok(());
    }
    let mut walk = walkdir::WalkDir::new(&base)
        .follow_links(false)
        .min_depth(1)
        .into_iter();
    while let Some(entry) = walk.next() {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(error = %e, "walking for a failure_out glob; entry skipped");
                continue;
            }
        };
        let Ok(rel) = entry.path().strip_prefix(ws_dir) else {
            continue;
        };
        let Some(rel_str) = rel.to_str() else {
            continue;
        };
        if !compiled.matches_with(rel_str, GLOB_OPTS) {
            continue;
        }
        if entry.file_type().is_dir() {
            walk.skip_current_dir();
        }
        matched.insert(rel.to_path_buf());
    }
    Ok(())
}

/// What one [`Copier::entry`] copied.
#[derive(Debug, Default, Clone, Copy)]
struct Copied {
    files: usize,
    /// A directory was copied whole.
    dir: bool,
}

/// Copies matches into the staging directory at their workspace-relative
/// paths, creating it and every directory under it one `create_dir` at a
/// time: never anything above it, so a removed sandbox is not created again.
struct Copier<'a> {
    staging: &'a Path,
    /// Canonical; what a followed link must resolve inside.
    roots: &'a [PathBuf],
    stop: &'a dyn Fn() -> bool,
    /// The staging directory exists: made here, or already there.
    staging_made: bool,
}

/// `create_dir`, a directory already there being fine.
fn create_dir_once(dir: &Path) -> anyhow::Result<()> {
    match fs::create_dir(dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => {
            Err(e).with_context(|| format!("creating {}", dir.display()))
        }
        _ => Ok(()),
    }
}

impl Copier<'_> {
    fn check_stop(&self) -> anyhow::Result<()> {
        if (self.stop)() {
            return Err(crate::stop::cancelled());
        }
        Ok(())
    }

    /// Create `rel` under the staging directory, and every directory between.
    fn mkdir(&mut self, rel: &Path) -> anyhow::Result<()> {
        if !self.staging_made {
            create_dir_once(self.staging)?;
            self.staging_made = true;
        }
        let mut at = self.staging.to_path_buf();
        for c in rel.components() {
            at.push(c);
            create_dir_once(&at)?;
        }
        Ok(())
    }

    /// Copy `src` to `rel`: a file, or a directory whole. A missing `src`
    /// copies nothing. A symlink is copied as what it resolves to when that is
    /// inside the roots: a file always, a directory only when `top` (the
    /// declared path itself, never a link met inside a walk). Anything else — a
    /// socket, a fifo, a dangling link, a loop — is skipped. `top` also means
    /// `rel`'s parents may not exist yet.
    fn entry(&mut self, src: &Path, rel: &Path, top: bool) -> anyhow::Result<Copied> {
        let md = match fs::symlink_metadata(src) {
            Ok(md) => md,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Copied::default()),
            Err(e) => return Err(e).with_context(|| format!("lstat {}", src.display())),
        };
        let ft = md.file_type();
        if ft.is_symlink() {
            let real = match fs::canonicalize(src) {
                Ok(real) if self.roots.iter().any(|r| real.starts_with(r)) => real,
                Ok(real) => {
                    tracing::debug!(path = %src.display(), real = %real.display(), "failure_out link leaves the sandbox; not followed");
                    return Ok(Copied::default());
                }
                Err(e) => {
                    tracing::debug!(path = %src.display(), error = %e, "failure_out link does not resolve; skipped");
                    return Ok(Copied::default());
                }
            };
            let md = fs::metadata(&real).with_context(|| format!("stat {}", real.display()))?;
            return if md.is_file() {
                self.file(&real, rel, top)
            } else if md.is_dir() && top {
                self.dir(&real, rel)
            } else {
                Ok(Copied::default())
            };
        }
        if ft.is_file() {
            self.file(src, rel, top)
        } else if ft.is_dir() {
            self.dir(src, rel)
        } else {
            Ok(Copied::default())
        }
    }

    /// Copy the file `src` to `rel`. One that cannot be read is skipped with a
    /// `warn!`: the rest of the diagnostics are still kept.
    fn file(&mut self, src: &Path, rel: &Path, top: bool) -> anyhow::Result<Copied> {
        self.check_stop()?;
        if top && let Some(parent) = rel.parent() {
            self.mkdir(parent)?;
        }
        let dst = self.staging.join(rel);
        // Reflinks where it can (`clonefile` on APFS, `copy_file_range` on
        // btrfs and XFS); a new inode either way.
        match fs::copy(src, &dst) {
            Ok(_) => Ok(Copied {
                files: 1,
                dir: false,
            }),
            Err(e) if fs::File::open(src).is_err() => {
                tracing::warn!(path = %rel.display(), error = %e, "failure_out file cannot be read; skipped");
                Ok(Copied::default())
            }
            Err(e) => {
                Err(e).with_context(|| format!("copying {} to {}", src.display(), dst.display()))
            }
        }
    }

    /// Copy the directory `src` to `rel`, whole.
    fn dir(&mut self, src: &Path, rel: &Path) -> anyhow::Result<Copied> {
        self.mkdir(rel)?;
        let mut files = 0;
        for entry in walkdir::WalkDir::new(src).follow_links(false).min_depth(1) {
            self.check_stop()?;
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::debug!(error = %e, "walking a failure_out directory; entry skipped");
                    continue;
                }
            };
            let Ok(sub) = entry.path().strip_prefix(src) else {
                continue;
            };
            let to = rel.join(sub);
            if entry.file_type().is_dir() {
                // Walked parent first: one level, its parent already made.
                create_dir_once(&self.staging.join(&to))?;
            } else {
                files += self.entry(entry.path(), &to, false)?.files;
            }
        }
        Ok(Copied { files, dir: true })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt as _, symlink};

    fn out(paths: Vec<Content>) -> Vec<FailureOutput> {
        vec![FailureOutput {
            group: String::new(),
            paths,
        }]
    }

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(p, s).expect("write");
    }

    /// Files and directories land at their workspace-relative path; a glob
    /// matches across directories; a matched directory's files are copied once.
    #[test]
    fn copies_files_dirs_and_globs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        write(&ws.join("app/junit.xml"), "j");
        write(&ws.join("app/test-results/a/b/trace.zip"), "t");
        write(&ws.join("app/shots/x.png"), "x");
        write(&ws.join("app/shots/deep/y.png"), "y");
        write(&ws.join("app/shots/deep/z.txt"), "z");
        let staging = tmp.path().join("stage");
        let n = collect_failure_outputs(
            &out(vec![
                Content::FilePath("app/junit.xml".into()),
                Content::DirPath("app/test-results/".into()),
                Content::Glob("app/**/*.png".into()),
                Content::FilePath("app/missing.txt".into()),
            ]),
            &ws,
            &staging,
            std::slice::from_ref(&ws),
            &|| false,
        )
        .expect("collect");
        assert_eq!(n, 4);
        for (p, s) in [
            ("app/junit.xml", "j"),
            ("app/test-results/a/b/trace.zip", "t"),
            ("app/shots/x.png", "x"),
            ("app/shots/deep/y.png", "y"),
        ] {
            assert_eq!(fs::read_to_string(staging.join(p)).expect(p), s);
        }
        assert!(!staging.join("app/shots/deep/z.txt").exists());
        assert_ne!(
            fs::metadata(ws.join("app/junit.xml")).expect("src").ino(),
            fs::metadata(staging.join("app/junit.xml"))
                .expect("dst")
                .ino(),
            "a copy, never a link"
        );
    }

    /// Nothing matched: nothing copied, and no staging directory is left.
    #[test]
    fn nothing_matched_leaves_no_staging() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).expect("mkdir");
        let staging = tmp.path().join("stage");
        let n = collect_failure_outputs(
            &out(vec![
                Content::DirPath("app/test-results".into()),
                Content::Glob("app/**/*.png".into()),
            ]),
            &ws,
            &staging,
            std::slice::from_ref(&ws),
            &|| false,
        )
        .expect("collect");
        assert_eq!(n, 0);
        assert!(!staging.exists());
    }

    /// A link out of the sandbox, a dangling one, a loop and a link to an
    /// ancestor directory: none hangs, none escapes, none fails the collect.
    #[test]
    fn links_never_escape_or_loop() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        let outside = tmp.path().join("outside");
        write(&outside.join("secret.txt"), "secret");
        write(&ws.join("app/r/real.txt"), "real");
        symlink(outside.join("secret.txt"), ws.join("app/r/out-file")).expect("link");
        symlink(&outside, ws.join("app/r/out-dir")).expect("link");
        symlink(ws.join("app/r/nope"), ws.join("app/r/dangling")).expect("link");
        symlink(ws.join("app/r/loop"), ws.join("app/r/loop")).expect("link");
        symlink(ws.join("app"), ws.join("app/r/ancestor")).expect("link");
        symlink(ws.join("app/r/real.txt"), ws.join("app/r/inside")).expect("link");
        symlink(&outside, ws.join("app/escape")).expect("link");
        let staging = tmp.path().join("stage");
        let n = collect_failure_outputs(
            &out(vec![
                Content::DirPath("app/r".into()),
                Content::Glob("app/**/*.txt".into()),
                Content::FilePath("app/escape/secret.txt".into()),
                Content::DirPath("app/escape".into()),
            ]),
            &ws,
            &staging,
            std::slice::from_ref(&ws),
            &|| false,
        )
        .expect("collect");
        assert_eq!(n, 2, "real.txt, and the link to it as a file");
        assert_eq!(
            fs::read_to_string(staging.join("app/r/inside")).expect("inside"),
            "real"
        );
        for gone in [
            "app/r/out-file",
            "app/r/out-dir",
            "app/r/dangling",
            "app/r/loop",
            "app/r/ancestor",
            "app/escape",
        ] {
            assert!(
                fs::symlink_metadata(staging.join(gone)).is_err(),
                "{gone} must not be copied"
            );
        }
    }

    fn process_failed(exit: Exit) -> anyhow::Error {
        anyhow::Error::new(hplugin::error::ProcessFailed::new(
            format!("{exit:?}"),
            Some(exit),
        ))
    }

    /// D16 and D8: only a clean exit code from 1 to 127, never under
    /// `--shell`, never cancelled, never without a process exit.
    #[test]
    fn write_back_predicate() {
        let live = hcore::hasync::StdCancellationToken::new();
        let cancelled = hcore::hasync::StdCancellationToken::new();
        cancelled.cancel();
        let cases: [(&str, anyhow::Error, bool, bool, bool); 9] = [
            ("exit 1", process_failed(Exit::Code(1)), false, false, true),
            (
                "exit 127",
                process_failed(Exit::Code(127)),
                false,
                false,
                true,
            ),
            (
                "exit 128",
                process_failed(Exit::Code(128)),
                false,
                false,
                false,
            ),
            (
                "exit 137",
                process_failed(Exit::Code(137)),
                false,
                false,
                false,
            ),
            (
                "SIGKILL",
                process_failed(Exit::Signal(9)),
                false,
                false,
                false,
            ),
            ("exit 0", process_failed(Exit::Code(0)), false, false, false),
            (
                "shell exit 1",
                process_failed(Exit::Code(1)),
                true,
                false,
                false,
            ),
            (
                "cancelled exit 1",
                process_failed(Exit::Code(1)),
                false,
                true,
                false,
            ),
            (
                "no process exit",
                anyhow::anyhow!("sandbox prep"),
                false,
                false,
                false,
            ),
        ];
        for (name, err, shell, cancel, want) in cases {
            let ct: &(dyn Cancellable + Send + Sync) = if cancel { &cancelled } else { &live };
            assert_eq!(
                write_back_on_failure_exit(&err, shell, ct).is_some(),
                want,
                "{name}"
            );
        }
        assert!(writes_back_on_failure(&CodegenMode::InPlace {
            write_on_failure: true
        }));
        for mode in [
            CodegenMode::None,
            CodegenMode::Copy,
            CodegenMode::InPlace {
                write_on_failure: false,
            },
        ] {
            assert!(!writes_back_on_failure(&mode), "{mode:?}");
        }
    }

    fn in_place(
        group: &str,
        paths: Vec<(Content, CodegenMode)>,
    ) -> hplugin::driver::targetdef::Output {
        hplugin::driver::targetdef::Output {
            group: group.to_owned(),
            paths: paths
                .into_iter()
                .map(
                    |(content, codegen_tree)| hplugin::driver::targetdef::path::Path {
                        content,
                        codegen_tree,
                        collect: true,
                    },
                )
                .collect(),
        }
    }

    const WRITE: CodegenMode = CodegenMode::InPlace {
        write_on_failure: true,
    };

    /// D18: a declared path the failed run never wrote is skipped, and the
    /// rest of the group is still packed; only `write_on_failure` paths are
    /// packed, under the `writeback-` namespace; a group with nothing present
    /// packs no tar.
    #[test]
    fn write_back_collection_is_lenient() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        write(&ws.join("app/a.go"), "a");
        write(&ws.join("app/gen/x.go"), "x");
        write(&ws.join("app/on_success.go"), "s");
        let sandbox = tmp.path().join("sandbox");
        let outputs = vec![
            in_place(
                "",
                vec![
                    (Content::FilePath("app/a.go".into()), WRITE),
                    (Content::FilePath("app/missing.go".into()), WRITE),
                    (Content::DirPath("app/gen".into()), WRITE),
                    (Content::DirPath("app/nodir".into()), WRITE),
                    (Content::Glob("app/none/*.go".into()), WRITE),
                    (
                        Content::FilePath("app/on_success.go".into()),
                        CodegenMode::InPlace {
                            write_on_failure: false,
                        },
                    ),
                ],
            ),
            in_place(
                "gone",
                vec![(Content::FilePath("app/gone.go".into()), WRITE)],
            ),
        ];
        let arts = crate::driver_managed::collect_write_back_inner(
            &outputs,
            &FailureOutMatch::default(),
            "abc",
            &ws,
            &sandbox,
            &|| false,
        )
        .expect("collect");
        assert_eq!(arts.len(), 1, "the all-missing group packs nothing");
        let art = &arts[0];
        let hplugin::driver::outputartifact::Content::TarPath(tar) = &art.content else {
            panic!("a tar");
        };
        assert!(
            Path::new(&tar.path)
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(crate::driver_managed::WRITE_BACK_TAR_PREFIX)),
            "{}",
            tar.path
        );
        let mut names: Vec<String> = hcore::hartifactcontent::Content::entry_paths(art)
            .expect("entries")
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["app/a.go", "app/gen/x.go"]);
    }

    /// A follow root lets a link into the shared stage be copied — as bytes in
    /// a new inode, not as the link.
    #[test]
    fn link_into_a_follow_root_is_copied() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        let stage = tmp.path().join("stage-root");
        write(&stage.join("e/gen.txt"), "staged");
        fs::create_dir_all(ws.join("app")).expect("mkdir");
        symlink(stage.join("e/gen.txt"), ws.join("app/gen.txt")).expect("link");
        let staging = tmp.path().join("stage");
        let n = collect_failure_outputs(
            &out(vec![Content::FilePath("app/gen.txt".into())]),
            &ws,
            &staging,
            &[ws.clone(), stage.clone()],
            &|| false,
        )
        .expect("collect");
        assert_eq!(n, 1);
        let kept = staging.join("app/gen.txt");
        assert!(!fs::symlink_metadata(&kept).expect("kept").is_symlink());
        assert_ne!(
            fs::metadata(&kept).expect("kept").ino(),
            fs::metadata(stage.join("e/gen.txt")).expect("staged").ino()
        );
    }

    /// I1: a `failure_out` path inside an `in_place` directory, or matched by
    /// a `failure_out` glob or directory, is never packed into the
    /// write-back; the fixes around it still are.
    #[test]
    fn write_back_leaves_out_failure_out_matches() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        write(&ws.join("app/gen/x.go"), "x");
        write(&ws.join("app/gen/report.log"), "r");
        write(&ws.join("app/gen/logs/a.txt"), "a");
        write(&ws.join("app/gen/shots/s.png"), "s");
        write(&ws.join("app/gen/shots/deep/t.png"), "t");
        write(&ws.join("app/report.log"), "top");
        let sandbox = tmp.path().join("sandbox");
        let outputs = vec![in_place(
            "",
            vec![
                (Content::DirPath("app/gen".into()), WRITE),
                (Content::FilePath("app/report.log".into()), WRITE),
                (Content::Glob("app/gen/*.log".into()), WRITE),
            ],
        )];
        let excluded = FailureOutMatch::new(&out(vec![
            Content::FilePath("app/gen/report.log".into()),
            Content::DirPath("app/gen/logs/".into()),
            Content::Glob("app/gen/sh*".into()),
            Content::FilePath("app/report.log".into()),
        ]));
        let arts = crate::driver_managed::collect_write_back_inner(
            &outputs,
            &excluded,
            "abc",
            &ws,
            &sandbox,
            &|| false,
        )
        .expect("collect");
        let mut names: Vec<String> = arts
            .iter()
            .flat_map(|a| hcore::hartifactcontent::Content::entry_paths(a).expect("entries"))
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["app/gen/x.go"]);
    }

    /// A stopped write-back packs nothing and reads as a cancellation.
    #[test]
    fn write_back_collection_stops() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        write(&ws.join("app/a.go"), "a");
        let outputs = vec![in_place(
            "",
            vec![(Content::FilePath("app/a.go".into()), WRITE)],
        )];
        let Err(err) = crate::driver_managed::collect_write_back_inner(
            &outputs,
            &FailureOutMatch::default(),
            "abc",
            &ws,
            &tmp.path().join("sandbox"),
            &|| true,
        ) else {
            panic!("a stopped write-back packs nothing");
        };
        assert!(hplugin::error::is_cancelled(&err), "{err:#}");
    }

    #[test]
    fn failure_out_match() {
        let m = FailureOutMatch::new(&out(vec![
            Content::FilePath("p/report.log".into()),
            Content::DirPath("p/results/".into()),
            Content::Glob("p/**/*.png".into()),
            Content::Glob("p/sh*".into()),
        ]));
        for (rel, want) in [
            ("p/report.log", true),
            ("p/report.logs", false),
            ("p/report.log.bak", false),
            ("p/results", true),
            ("p/results/a/b.txt", true),
            ("p/results2/a.txt", false),
            ("p/x/y/z.png", true),
            ("p/x/y/z.pngx", false),
            ("p/shots/deep/a.txt", true),
            ("p/other.txt", false),
            ("q/report.log", false),
        ] {
            assert_eq!(m.matches(Path::new(rel)), want, "{rel}");
        }
        assert!(FailureOutMatch::default().is_empty());
        assert!(!FailureOutMatch::default().matches(Path::new("p/report.log")));
    }

    /// An absolute path, a `..`, a leading `.`, and a glob through either are
    /// skipped; nothing lands outside the staging directory, nor is a file
    /// copied onto itself.
    #[test]
    fn non_plain_paths_are_skipped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        write(&ws.join("app/ok.txt"), "ok");
        write(&ws.join("app/self.txt"), "self-bytes");
        write(&tmp.path().join("outside.txt"), "outside");
        let staging = tmp.path().join("stage");
        let abs = ws.join("app/self.txt");
        let n = collect_failure_outputs(
            &out(vec![
                Content::FilePath(abs.to_str().expect("utf-8").to_owned()),
                Content::FilePath("../outside.txt".into()),
                Content::FilePath("app/../../outside.txt".into()),
                Content::FilePath("./app/self.txt".into()),
                Content::Glob("../*.txt".into()),
                Content::Glob("app/../../*.txt".into()),
                Content::FilePath("app/ok.txt".into()),
            ]),
            &ws,
            &staging,
            std::slice::from_ref(&ws),
            &|| false,
        )
        .expect("collect");
        assert_eq!(n, 1);
        assert_eq!(read_tree(&staging), ["app", "app/ok.txt"]);
        assert_eq!(fs::read_to_string(&abs).expect("self"), "self-bytes");
        assert!(is_plain_relative(Path::new("a/b/**/*.x")));
        for bad in ["/a", "../a", "a/../b", "./a", ""] {
            assert_eq!(is_plain_relative(Path::new(bad)), bad.is_empty(), "{bad}");
        }
    }

    /// Every path under `dir`, relative, sorted.
    fn read_tree(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = walkdir::WalkDir::new(dir)
            .min_depth(1)
            .into_iter()
            .map(|e| {
                e.expect("entry")
                    .path()
                    .strip_prefix(dir)
                    .expect("under")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        v.sort();
        v
    }

    /// A file that cannot be read is skipped; every other match is kept.
    #[test]
    fn unreadable_file_is_skipped() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        write(&ws.join("app/r/a.txt"), "a");
        write(&ws.join("app/r/locked.txt"), "l");
        write(&ws.join("app/top-locked.txt"), "t");
        write(&ws.join("app/z.txt"), "z");
        for p in ["app/r/locked.txt", "app/top-locked.txt"] {
            fs::set_permissions(ws.join(p), fs::Permissions::from_mode(0o000)).expect("chmod");
        }
        if fs::File::open(ws.join("app/r/locked.txt")).is_ok() {
            eprintln!("unreadable_file_is_skipped: running as root, mode 000 is readable; skipped");
            return;
        }
        let staging = tmp.path().join("stage");
        let n = collect_failure_outputs(
            &out(vec![
                Content::DirPath("app/r".into()),
                Content::FilePath("app/top-locked.txt".into()),
                Content::FilePath("app/z.txt".into()),
            ]),
            &ws,
            &staging,
            std::slice::from_ref(&ws),
            &|| false,
        )
        .expect("one unreadable file does not fail the copy");
        assert_eq!(n, 2);
        assert_eq!(
            fs::read_to_string(staging.join("app/r/a.txt")).expect("a"),
            "a"
        );
        assert_eq!(
            fs::read_to_string(staging.join("app/z.txt")).expect("z"),
            "z"
        );
        assert!(!staging.join("app/r/locked.txt").exists());
        assert!(!staging.join("app/top-locked.txt").exists());
    }

    /// A stopped copy writes nothing and reads as a cancellation; a copy whose
    /// staging parent is gone fails rather than creating it again.
    #[test]
    fn stopped_or_orphaned_copy_writes_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        write(&ws.join("app/a/b.txt"), "b");
        let outputs = out(vec![Content::DirPath("app/a".into())]);
        let staging = tmp.path().join("stage");
        let err =
            collect_failure_outputs(&outputs, &ws, &staging, std::slice::from_ref(&ws), &|| true)
                .expect_err("stopped");
        assert!(hplugin::error::is_cancelled(&err), "{err:#}");
        assert!(!staging.join("app/a/b.txt").exists());

        let gone = tmp.path().join("removed-sandbox");
        let orphan = gone.join(FAILURE_OUT_STAGING);
        let err =
            collect_failure_outputs(&outputs, &ws, &orphan, std::slice::from_ref(&ws), &|| false)
                .expect_err("the sandbox is gone");
        assert!(!hplugin::error::is_cancelled(&err), "{err:#}");
        assert!(!gone.exists(), "the removed sandbox was created again");
    }

    /// A token cancelled as timed out keeps `failure_out` whatever the error;
    /// a request's cancellation, `--shell`, and no process exit keep nothing.
    #[test]
    fn keeps_failure_out_predicate() {
        let live = hcore::hasync::StdCancellationToken::new();
        let cancelled = hcore::hasync::StdCancellationToken::new();
        cancelled.cancel();
        let timed_out = hcore::hasync::StdCancellationToken::new();
        timed_out.cancel_timed_out();
        let bare = || anyhow::anyhow!("cancelled");
        assert!(keeps_failure_out(&bare(), false, &timed_out));
        assert!(!keeps_failure_out(&bare(), true, &timed_out));
        assert!(!keeps_failure_out(&bare(), false, &cancelled));
        assert!(!keeps_failure_out(&bare(), false, &live));
        assert!(keeps_failure_out(
            &process_failed(Exit::Signal(9)),
            false,
            &live
        ));
        assert!(!keeps_failure_out(
            &process_failed(Exit::Code(1)),
            false,
            &cancelled
        ));
    }

    /// The whole keep, on the token's reason alone: a run cancelled as timed
    /// out has its matches copied and attached, though its copy outlives the
    /// cancelled token; one cancelled by its request keeps nothing, with the
    /// same error and the same files.
    #[tokio::test]
    async fn a_timed_out_token_keeps_failure_out_and_a_cancelled_one_does_not() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        write(&ws.join("app/junit.xml"), "j");
        let outputs = out(vec![Content::FilePath("app/junit.xml".into())]);
        let keep = |sandbox: &'static str, token: hcore::hasync::StdCancellationToken| {
            let (ws, outputs) = (ws.clone(), outputs.clone());
            let sandbox = tmp.path().join(sandbox);
            async move {
                fs::create_dir_all(&sandbox).expect("sandbox");
                let err = keep_failure_outputs(
                    anyhow::Error::new(hplugin::error::CancelledError),
                    &outputs,
                    false,
                    &token,
                    &ws,
                    &sandbox,
                    &[],
                )
                .await;
                (err, sandbox.join(FAILURE_OUT_STAGING))
            }
        };

        let timed_out = hcore::hasync::StdCancellationToken::new();
        timed_out.cancel_timed_out();
        let (err, staging) = keep("timed-out", timed_out).await;
        let kept = hplugin::error::failure_artifacts(&err).expect("kept on a timeout");
        assert_eq!(kept.0.len(), 1, "{err:#}");
        assert_eq!(
            fs::read_to_string(staging.join("app/junit.xml")).expect("copied"),
            "j"
        );

        let cancelled = hcore::hasync::StdCancellationToken::new();
        cancelled.cancel();
        let (err, staging) = keep("cancelled", cancelled).await;
        assert!(hplugin::error::failure_artifacts(&err).is_none(), "{err:#}");
        assert!(!staging.exists(), "nothing copied for a request cancel");
    }
}
