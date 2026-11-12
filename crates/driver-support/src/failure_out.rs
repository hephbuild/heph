//! `failure_out`: what a failed run leaves behind for the human fixing it.
//!
//! When a managed run fails with a process exit, the managed layer packs each
//! of the target's [`FailureOutput`] matches, while the sandbox (and a FUSE
//! slot) is still live, into one tar under
//! `<sandbox>/heph-collect-artifacts/` (named under
//! [`FAILURE_OUT_TAR_PREFIX`]), its members workspace-relative paths, and
//! attaches it to the error as a
//! [`FailureArtifacts`](hplugin::error::FailureArtifacts) entry
//! ([`hplugin::error::failure_out_artifact`]). The engine unpacks it into the
//! target's failure slot as `out/`.
//!
//! The walk and the pack are the ones outputs use
//! ([`add_content`](crate::driver_managed::add_content) under
//! `Reading::Diagnostics`, and the shared tar packer).
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
//! - **Stoppable, and never recreating the sandbox.** The walk and the pack
//!   poll a stop, and the tar's directory is created with `create_dir`: a job
//!   left running after its run was dropped stops, or fails on the removed
//!   sandbox, rather than writing into it again.
//! - **Packed, never linked.** A tar member is bytes; the slot's file is a new
//!   inode, so an edit in the slot can never change a staged input.
//! - **Lenient.** A path that matches nothing is skipped, and so is a file
//!   that cannot be read. A symlink is kept as its content only when it
//!   resolves inside the sandbox's workspace or the shared stage (a staged
//!   read-only input is a link into it); a dangling one, a loop, or one leaving
//!   both is skipped. Inside a packed directory, a link to a directory is never
//!   followed, so no walk can loop.
//! - **UTF-8 names.** A tar member names a UTF-8 path; a file whose name is not
//!   UTF-8 is skipped.
//! - **Never hides the failure.** A collect error is a `warn!` and a context
//!   line on the target's own error.

use crate::driver_managed::{GLOB_OPTS, Reading, Walk, add_content, pack_to_artifact_tar};
use hcore::hartifactcontent::tar::TarPacker;
use hcore::hasync::Cancellable;
use hplugin::driver::outputartifact::OutputArtifact;
use hplugin::driver::targetdef::path::{CodegenMode, Content};
use hplugin::driver::targetdef::{FailureOutput, TargetDef};
use hplugin::error::Exit;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// The name prefix of a `failure_out` tar (`failureout-<hashin>-out-….tar`)
/// under the sandbox's `heph-collect-artifacts/`. An `out` tar's name starts
/// with its hex `hashin`, and a write-back's with `writeback-`, so none meet.
pub const FAILURE_OUT_TAR_PREFIX: &str = "failureout-";

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
/// of the sandbox, or out of the failure slot it is unpacked into.
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

/// Keep `outputs`' matches for a failed run: pack them out of `ws_dir` into a
/// tar under `<sandbox_dir>/heph-collect-artifacts/` and attach it to `err`.
///
/// `follow_roots` are the directories, besides `ws_dir`, a symlink may resolve
/// into and still be followed (the shared stage). Returns `err` unchanged when
/// the run keeps nothing ([`keeps_failure_out`]), nothing matched, or the
/// pack was stopped; with a `warn!` and one context line when collecting
/// failed — never another error in its place.
///
/// The pack stops when the request is cancelled, or this future dropped. A
/// timed-out run's token is already cancelled, so its pack stops only on the
/// drop: the engine waits out its grace for it.
#[expect(
    clippy::too_many_arguments,
    reason = "the managed layer's run state, passed through as its sibling keep_failure_write_back takes it"
)]
pub async fn keep_failure_outputs(
    err: anyhow::Error,
    outputs: &[FailureOutput],
    hashin: &str,
    shell: bool,
    ctoken: &(dyn Cancellable + Send + Sync),
    ws_dir: &Path,
    sandbox_dir: &Path,
    follow_roots: &[PathBuf],
) -> anyhow::Error {
    if outputs.is_empty() || !keeps_failure_out(&err, shell, ctoken) {
        return err;
    }
    let timed_out = ctoken.timed_out();
    let outputs = outputs.to_vec();
    let hashin = hashin.to_owned();
    let mut roots = Vec::with_capacity(follow_roots.len() + 1);
    roots.push(ws_dir.to_path_buf());
    roots.extend_from_slice(follow_roots);
    let (ws_dir, sandbox_dir) = (ws_dir.to_path_buf(), sandbox_dir.to_path_buf());
    // Off the worker: a Playwright report is thousands of files.
    let job = move |stop: &(dyn Fn() -> bool + Sync)| {
        collect_failure_outputs(&outputs, &ws_dir, &sandbox_dir, &hashin, &roots, stop)
    };
    let collected = if timed_out {
        crate::stop::run_blocking_until_dropped(job).await
    } else {
        crate::stop::run_blocking(ctoken, job).await
    };
    match collected {
        Ok(None) => err,
        Ok(Some(a)) => hplugin::error::with_failure_artifacts(err, vec![a]),
        Err(e) if hplugin::error::is_cancelled(&e) => {
            tracing::debug!(error = %format!("{e:#}"), "keeping failure_out: pack stopped");
            err
        }
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

/// Pack every match of `outputs` under `ws_dir` into one tar under
/// `<sandbox_dir>/heph-collect-artifacts/`, each at its workspace-relative
/// path, and return it as the [`hplugin::error::failure_out_artifact`]. With
/// nothing matched, `None` and no tar.
///
/// `roots` (the workspace and the shared stage) are what a symlink may resolve
/// inside and still be kept. A member two entries name (a file inside a
/// declared directory and also matched by a glob) is packed once.
///
/// `stop` is polled before each file and while packing; once it reads true
/// the collect returns [`crate::stop::cancelled`]. The tar's directory is
/// created with `create_dir`, so a sandbox removed under a job its run
/// abandoned fails the collect instead of being created again.
pub fn collect_failure_outputs(
    outputs: &[FailureOutput],
    ws_dir: &Path,
    sandbox_dir: &Path,
    hashin: &str,
    roots: &[PathBuf],
    stop: &dyn Fn() -> bool,
) -> anyhow::Result<Option<OutputArtifact>> {
    let roots: Vec<PathBuf> = roots
        .iter()
        .filter_map(|r| fs::canonicalize(r).ok())
        .collect();
    let walk = Walk {
        missing_ok: true,
        skip: &|_: &Path| false,
        reading: Reading::Diagnostics { roots: &roots },
    };
    let mut members: BTreeSet<String> = BTreeSet::new();
    let mut tar = TarPacker::new();
    for content in outputs
        .iter()
        .flat_map(|o| o.paths.iter())
        .filter_map(plain_content)
    {
        add_content(
            &mut |source, member| {
                if members.insert(member.clone()) {
                    tar.create_file(source, member);
                }
            },
            ws_dir,
            content,
            "",
            stop,
            &walk,
        )?;
    }
    if members.is_empty() {
        return Ok(None);
    }
    let (tar_path, hashout) = pack_to_artifact_tar(
        sandbox_dir,
        &format!("{FAILURE_OUT_TAR_PREFIX}{hashin}"),
        "out",
        tar,
        stop,
    )?;
    Ok(Some(hplugin::error::failure_out_artifact(
        tar_path, hashout,
    )))
}

/// Every file member of a `failure_out` tar, and its contents.
#[cfg(test)]
pub(crate) fn tar_files(a: &OutputArtifact) -> std::collections::BTreeMap<String, String> {
    use hcore::hartifactcontent::{Content as _, WalkEntryKind};
    use std::io::Read as _;
    a.walk()
        .expect("walk")
        .map(|e| {
            let e = e.expect("entry");
            let WalkEntryKind::File { mut data, .. } = e.kind else {
                panic!("{} is not a file", e.path.display());
            };
            let mut s = String::new();
            data.read_to_string(&mut s).expect("read");
            (e.path.to_string_lossy().into_owned(), s)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

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

    /// A new sandbox dir under `tmp`, as the run's would be.
    fn sandbox(tmp: &Path, name: &str) -> PathBuf {
        let s = tmp.join(name);
        fs::create_dir_all(&s).expect("sandbox");
        s
    }

    /// [`collect_failure_outputs`] with the sandbox's own workspace as the only
    /// root, never stopped.
    fn collect(outputs: &[FailureOutput], ws: &Path, sandbox: &Path) -> Option<OutputArtifact> {
        collect_failure_outputs(
            outputs,
            ws,
            sandbox,
            "abc",
            std::slice::from_ref(&ws.to_path_buf()),
            &|| false,
        )
        .expect("collect")
    }

    /// The `failure_out` tars packed under `sandbox`.
    fn packed(sandbox: &Path) -> Vec<String> {
        let dir = sandbox.join(crate::driver_managed::COLLECT_ARTIFACTS_DIR);
        let mut v: Vec<String> = fs::read_dir(dir)
            .into_iter()
            .flatten()
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(FAILURE_OUT_TAR_PREFIX))
            .collect();
        v.sort();
        v
    }

    /// Files and directories land at their workspace-relative path; a glob
    /// matches across directories; a file two entries match is packed once.
    #[test]
    fn copies_files_dirs_and_globs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        write(&ws.join("app/junit.xml"), "j");
        write(&ws.join("app/test-results/a/b/trace.zip"), "t");
        write(&ws.join("app/shots/x.png"), "x");
        write(&ws.join("app/shots/deep/y.png"), "y");
        write(&ws.join("app/shots/deep/z.txt"), "z");
        let sandbox = sandbox(tmp.path(), "sandbox");
        let art = collect(
            &out(vec![
                Content::FilePath("app/junit.xml".into()),
                Content::DirPath("app/test-results/".into()),
                Content::Glob("app/**/*.png".into()),
                Content::FilePath("app/shots/x.png".into()),
                Content::FilePath("app/missing.txt".into()),
            ]),
            &ws,
            &sandbox,
        )
        .expect("kept");
        let files = tar_files(&art);
        let want: Vec<(&str, &str)> = vec![
            ("app/junit.xml", "j"),
            ("app/shots/deep/y.png", "y"),
            ("app/shots/x.png", "x"),
            ("app/test-results/a/b/trace.zip", "t"),
        ];
        let got: Vec<(&str, &str)> = files
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(got, want);
        let names = hcore::hartifactcontent::Content::entry_paths(&art).expect("entries");
        assert_eq!(names.len(), want.len(), "each member once: {names:?}");
        assert_eq!(art.name, hplugin::error::FAILURE_OUT_NAME);
        assert!(!art.hashout.is_empty());
        let hplugin::driver::outputartifact::Content::TarPath(tar) = &art.content else {
            panic!("a tar");
        };
        assert!(tar.owned, "the engine frees it once unpacked");
        assert_eq!(packed(&sandbox).len(), 1);
    }

    /// Nothing matched: no artifact, and no tar packed.
    #[test]
    fn nothing_matched_leaves_no_staging() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).expect("mkdir");
        let sandbox = sandbox(tmp.path(), "sandbox");
        let art = collect(
            &out(vec![
                Content::DirPath("app/test-results".into()),
                Content::Glob("app/**/*.png".into()),
            ]),
            &ws,
            &sandbox,
        );
        assert!(art.is_none());
        assert!(packed(&sandbox).is_empty());
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
        let sandbox = sandbox(tmp.path(), "sandbox");
        let art = collect(
            &out(vec![
                Content::DirPath("app/r".into()),
                Content::Glob("app/**/*.txt".into()),
                Content::FilePath("app/escape/secret.txt".into()),
                Content::DirPath("app/escape".into()),
            ]),
            &ws,
            &sandbox,
        )
        .expect("kept");
        assert_eq!(
            tar_files(&art).into_iter().collect::<Vec<_>>(),
            [
                ("app/r/inside".to_owned(), "real".to_owned()),
                ("app/r/real.txt".to_owned(), "real".to_owned()),
            ],
            "real.txt, and the link to it as a file; nothing else"
        );
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
        let sandbox = sandbox(tmp.path(), "sandbox");
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

    /// A follow root lets a link into the shared stage be kept — as a file
    /// member holding its bytes, not as the link; without the root it is not.
    #[test]
    fn link_into_a_follow_root_is_copied() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        let stage = tmp.path().join("stage-root");
        write(&stage.join("e/gen.txt"), "staged");
        fs::create_dir_all(ws.join("app")).expect("mkdir");
        symlink(stage.join("e/gen.txt"), ws.join("app/gen.txt")).expect("link");
        let sandbox = sandbox(tmp.path(), "sandbox");
        let outputs = out(vec![Content::FilePath("app/gen.txt".into())]);
        let art = collect_failure_outputs(
            &outputs,
            &ws,
            &sandbox,
            "abc",
            &[ws.clone(), stage.clone()],
            &|| false,
        )
        .expect("collect")
        .expect("kept");
        assert_eq!(
            tar_files(&art).get("app/gen.txt").map(String::as_str),
            Some("staged"),
            "a file member (tar_files panics on a link)"
        );
        assert!(collect(&outputs, &ws, &sandbox).is_none(), "no follow root");
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
        let sandbox = sandbox(tmp.path(), "sandbox");
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
    /// skipped; nothing outside the workspace is packed, and no member names
    /// anything but a plain relative path.
    #[test]
    fn non_plain_paths_are_skipped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        write(&ws.join("app/ok.txt"), "ok");
        write(&ws.join("app/self.txt"), "self-bytes");
        write(&tmp.path().join("outside.txt"), "outside");
        let sandbox = sandbox(tmp.path(), "sandbox");
        let abs = ws.join("app/self.txt");
        let art = collect(
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
            &sandbox,
        )
        .expect("kept");
        assert_eq!(
            tar_files(&art).into_keys().collect::<Vec<_>>(),
            ["app/ok.txt"]
        );
        assert_eq!(fs::read_to_string(&abs).expect("self"), "self-bytes");
        assert!(is_plain_relative(Path::new("a/b/**/*.x")));
        for bad in ["/a", "../a", "a/../b", "./a", ""] {
            assert_eq!(is_plain_relative(Path::new(bad)), bad.is_empty(), "{bad}");
        }
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
        let sandbox = sandbox(tmp.path(), "sandbox");
        let art = collect(
            &out(vec![
                Content::DirPath("app/r".into()),
                Content::FilePath("app/top-locked.txt".into()),
                Content::FilePath("app/z.txt".into()),
            ]),
            &ws,
            &sandbox,
        )
        .expect("one unreadable file does not fail the collect");
        assert_eq!(
            tar_files(&art).into_iter().collect::<Vec<_>>(),
            [
                ("app/r/a.txt".to_owned(), "a".to_owned()),
                ("app/z.txt".to_owned(), "z".to_owned()),
            ]
        );
    }

    /// A stopped collect packs nothing and reads as a cancellation; a collect
    /// whose sandbox is gone fails rather than creating it again.
    #[test]
    fn stopped_or_orphaned_copy_writes_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        write(&ws.join("app/a/b.txt"), "b");
        let outputs = out(vec![Content::DirPath("app/a".into())]);
        let live = sandbox(tmp.path(), "sandbox");
        let roots = std::slice::from_ref(&ws);
        let Err(err) = collect_failure_outputs(&outputs, &ws, &live, "abc", roots, &|| true) else {
            panic!("stopped");
        };
        assert!(hplugin::error::is_cancelled(&err), "{err:#}");
        assert!(packed(&live).is_empty());

        let gone = tmp.path().join("removed-sandbox");
        let Err(err) = collect_failure_outputs(&outputs, &ws, &gone, "abc", roots, &|| false)
        else {
            panic!("the sandbox is gone");
        };
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
    /// out has its matches packed and attached, though its pack outlives the
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
                    "abc",
                    false,
                    &token,
                    &ws,
                    &sandbox,
                    &[],
                )
                .await;
                (err, sandbox)
            }
        };

        let timed_out = hcore::hasync::StdCancellationToken::new();
        timed_out.cancel_timed_out();
        let (err, _) = keep("timed-out", timed_out).await;
        let kept = hplugin::error::failure_artifacts(&err).expect("kept on a timeout");
        assert_eq!(kept.0.len(), 1, "{err:#}");
        let tar = hplugin::error::failure_out_tar(&err).expect("the failure_out tar");
        assert_eq!(
            tar_files(tar).get("app/junit.xml").map(String::as_str),
            Some("j")
        );

        let cancelled = hcore::hasync::StdCancellationToken::new();
        cancelled.cancel();
        let (err, sandbox) = keep("cancelled", cancelled).await;
        assert!(hplugin::error::failure_artifacts(&err).is_none(), "{err:#}");
        assert!(
            packed(&sandbox).is_empty(),
            "nothing packed for a request cancel"
        );
    }
}
