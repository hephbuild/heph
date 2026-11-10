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
//! - **Only after a process exit** ([`kept_failure_exit`]): never on
//!   cancellation, never under `--shell`, never for a failure no process exit
//!   is behind (sandbox prep, credentials, a runner).
//! - **Copied** with `std::fs::copy`, which reflinks where the filesystem can.
//!   Never a hardlink and never a rename of the original: a hardlink would
//!   share an inode with a staged input, and an edit in the slot would change
//!   that input under the same key.
//! - **Lenient.** A path that matches nothing is skipped. A symlink is followed
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
use hplugin::driver::targetdef::FailureOutput;
use hplugin::driver::targetdef::path::Content;
use hplugin::error::Exit;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The directory, directly under the run's sandbox, a failed run's
/// `failure_out` matches are copied into. Outside `ws/`, so the run can never
/// have written it, and nothing the run wrote is mistaken for it.
pub const FAILURE_OUT_STAGING: &str = ".heph-failure-out";

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

/// Keep `outputs`' matches for a failed run: copy them out of `ws_dir` into
/// `<staging_parent>/.heph-failure-out` and attach that directory to `err`.
///
/// `follow_roots` are the directories, besides `ws_dir`, a symlink may resolve
/// into and still be followed (the shared stage). Returns `err` unchanged when
/// the run keeps nothing ([`kept_failure_exit`]) or nothing matched; with a
/// `warn!` and one context line when collecting failed — never another error
/// in its place.
pub async fn keep_failure_outputs(
    err: anyhow::Error,
    outputs: &[FailureOutput],
    shell: bool,
    ctoken: &(dyn Cancellable + Send + Sync),
    ws_dir: &Path,
    staging_parent: &Path,
    follow_roots: &[PathBuf],
) -> anyhow::Error {
    if outputs.is_empty() || kept_failure_exit(&err, shell, ctoken).is_none() {
        return err;
    }
    let outputs = outputs.to_vec();
    let staging = staging_parent.join(FAILURE_OUT_STAGING);
    let mut roots = Vec::with_capacity(follow_roots.len() + 1);
    roots.push(ws_dir.to_path_buf());
    roots.extend_from_slice(follow_roots);
    let ws_dir = ws_dir.to_path_buf();
    // Off the worker: a Playwright report is thousands of files.
    let job_staging = staging.clone();
    let copied = hcore::blocking::run(move || {
        collect_failure_outputs(&outputs, &ws_dir, &job_staging, &roots)
    })
    .await;
    match copied {
        Ok(0) => err,
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

/// Copy every match of `outputs` under `ws_dir` into `staging`, at its
/// workspace-relative path. Returns how many files were copied; with none,
/// `staging` is left absent.
pub fn collect_failure_outputs(
    outputs: &[FailureOutput],
    ws_dir: &Path,
    staging: &Path,
    roots: &[PathBuf],
) -> anyhow::Result<usize> {
    let roots: Vec<PathBuf> = roots
        .iter()
        .filter_map(|r| fs::canonicalize(r).ok())
        .collect();
    let mut matched: BTreeSet<PathBuf> = BTreeSet::new();
    for output in outputs {
        for content in &output.paths {
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
        let n = copy_entry(&src, &staging.join(rel), &roots, true)
            .with_context(|| format!("copying failure_out {}", rel.display()))?;
        if n.dir {
            last_dir = Some(rel);
        }
        copied += n.files;
    }
    if copied == 0 {
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
    let opts = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };
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
        if !compiled.matches_with(rel_str, opts) {
            continue;
        }
        if entry.file_type().is_dir() {
            walk.skip_current_dir();
        }
        matched.insert(rel.to_path_buf());
    }
    Ok(())
}

/// What one [`copy_entry`] copied.
#[derive(Debug, Default, Clone, Copy)]
struct Copied {
    files: usize,
    /// A directory was copied whole.
    dir: bool,
}

/// Copy `src` to `dst`: a file, or a directory whole. A missing `src` copies
/// nothing. A symlink is copied as what it resolves to when that is inside
/// `roots`: a file always, a directory only when `follow_dir_link` (the
/// declared path itself, never a link met inside a walk). Anything else — a
/// socket, a fifo, a dangling link, a loop — is skipped.
fn copy_entry(
    src: &Path,
    dst: &Path,
    roots: &[PathBuf],
    follow_dir_link: bool,
) -> anyhow::Result<Copied> {
    let md = match fs::symlink_metadata(src) {
        Ok(md) => md,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Copied::default()),
        Err(e) => return Err(e).with_context(|| format!("lstat {}", src.display())),
    };
    let ft = md.file_type();
    if ft.is_symlink() {
        let real = match fs::canonicalize(src) {
            Ok(real) if roots.iter().any(|r| real.starts_with(r)) => real,
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
            copy_file(&real, dst)
        } else if md.is_dir() && follow_dir_link {
            copy_dir(&real, dst, roots)
        } else {
            Ok(Copied::default())
        };
    }
    if ft.is_file() {
        copy_file(src, dst)
    } else if ft.is_dir() {
        copy_dir(src, dst, roots)
    } else {
        Ok(Copied::default())
    }
}

fn copy_file(src: &Path, dst: &Path) -> anyhow::Result<Copied> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    // Reflinks where it can (`clonefile` on APFS, `copy_file_range` on btrfs
    // and XFS); a new inode either way.
    fs::copy(src, dst)
        .with_context(|| format!("copying {} to {}", src.display(), dst.display()))?;
    Ok(Copied {
        files: 1,
        dir: false,
    })
}

fn copy_dir(src: &Path, dst: &Path, roots: &[PathBuf]) -> anyhow::Result<Copied> {
    fs::create_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;
    let mut files = 0;
    for entry in walkdir::WalkDir::new(src).follow_links(false).min_depth(1) {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(error = %e, "walking a failure_out directory; entry skipped");
                continue;
            }
        };
        let Ok(rel) = entry.path().strip_prefix(src) else {
            continue;
        };
        let to = dst.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&to).with_context(|| format!("creating {}", to.display()))?;
        } else {
            files += copy_entry(entry.path(), &to, roots, false)?.files;
        }
    }
    Ok(Copied { files, dir: true })
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
}
