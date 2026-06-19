//! Anonymous repository fingerprint.
//!
//! A stable, repo-level identifier that lets PostHog group events by *project*
//! without learning which project it is. Two clones of the same repository — on
//! any machine, under any name, hosted anywhere — produce the same fingerprint,
//! because it is derived from the repository's **root commit** (the parentless
//! initial commit). Different repos almost never share a root commit.
//!
//! The root commit hash is itself hashed (SHA-256) before it leaves the machine,
//! so the fingerprint is opaque: it cannot be reversed to look up the actual
//! commit/repo, while still being deterministic across clones.
//!
//! Everything here is strictly best-effort. No git, not a repo, a shallow clone
//! whose history doesn't reach the root, an unreadable cache — any of these just
//! yields `None`, and the `repo_fingerprint` attr is omitted from the event.
//!
//! The `git rev-list` walk (O(history)) runs at most once per repo per machine:
//! the result is cached under the config dir, keyed by the working directory, so
//! every subsequent invocation is a single file read.

use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Cap on a single git subprocess. Telemetry runs on the exit path and must not
/// stall a real command — on a giant monorepo the root walk can be slow, so we
/// bound it and give up rather than block.
const GIT_TIMEOUT: Duration = Duration::from_secs(2);

/// The repo fingerprint for the process's working directory: SHA-256 (hex) of
/// the repository's root commit(s), or `None` when it can't be determined.
/// Cached under `<config_dir>/repo-fingerprints/`.
pub fn repo_fingerprint(config_dir: &Path) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    fingerprint_in(config_dir, &cwd)
}

/// Inner form taking the working directory explicitly (the public entry passes
/// the process cwd). Split out so tests can target a specific repo without
/// mutating global process state.
fn fingerprint_in(config_dir: &Path, work_dir: &Path) -> Option<String> {
    let cache_path = config_dir
        .join("repo-fingerprints")
        .join(hex(&Sha256::digest(
            work_dir.as_os_str().as_encoded_bytes(),
        )));

    if let Ok(cached) = std::fs::read_to_string(&cache_path) {
        let cached = cached.trim();
        if !cached.is_empty() {
            return Some(cached.to_string());
        }
    }

    let root = root_commit(work_dir)?;
    let fingerprint = hex(&Sha256::digest(root.as_bytes()));

    // Best-effort cache write — a failure here is fine, the next run recomputes.
    // Only positive results are cached: a shallow checkout that can't reach the
    // root returns `None` above, so a later full fetch is free to recompute.
    if let Some(parent) = cache_path.parent()
        && std::fs::create_dir_all(parent).is_ok()
    {
        drop(std::fs::write(&cache_path, &fingerprint));
    }

    Some(fingerprint)
}

/// The repository's root commit id(s): the output of
/// `git rev-list --max-parents=0 HEAD`, sorted and joined (a repo can have
/// several roots from merged histories — sorting keeps the value stable
/// regardless of git's enumeration order).
///
/// Shallow-aware for GitHub Actions: a default `actions/checkout` is a depth-1
/// clone, where the grafted boundary commit *masquerades* as a root. Those
/// grafts (listed in `.git/shallow`) are filtered out, so a shallow clone yields
/// the true root only when its history actually reaches it — otherwise `None`,
/// rather than a per-checkout value that would shatter one repo into thousands
/// of fingerprints. A `fetch-depth: 0` checkout (full history) always works.
fn root_commit(work_dir: &Path) -> Option<String> {
    let out = git(work_dir, &["rev-list", "--max-parents=0", "HEAD"])?;
    let mut roots: Vec<&str> = out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

    if is_shallow(work_dir) {
        let grafts = shallow_grafts(work_dir);
        roots.retain(|r| !grafts.iter().any(|g| g == r));
    }

    if roots.is_empty() {
        return None;
    }
    roots.sort_unstable();
    Some(roots.join("\n"))
}

/// Whether this is a shallow clone (`git rev-parse --is-shallow-repository`).
fn is_shallow(work_dir: &Path) -> bool {
    git(work_dir, &["rev-parse", "--is-shallow-repository"])
        .map(|s| s.trim() == "true")
        .unwrap_or(false)
}

/// The grafted boundary commits of a shallow clone: the lines of `.git/shallow`.
/// These are commits whose parents were omitted, so they falsely report zero
/// parents and must not be mistaken for the true root.
fn shallow_grafts(work_dir: &Path) -> Vec<String> {
    let Some(path) = git(work_dir, &["rev-parse", "--git-path", "shallow"]) else {
        return Vec::new();
    };
    let path = Path::new(path.trim());
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        work_dir.join(path)
    };
    std::fs::read_to_string(path)
        .map(|s| {
            s.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Run `git <args>` in `work_dir`, returning trimmed stdout on a clean exit, or
/// `None` on any failure (git missing, non-repo, non-zero exit, timeout).
fn git(work_dir: &Path, args: &[&str]) -> Option<String> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                break;
            }
            Ok(None) => {
                if started.elapsed() >= GIT_TIMEOUT {
                    drop(child.kill());
                    drop(child.wait());
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }

    use std::io::Read;
    let mut stdout = child.stdout.take()?;
    let mut buf = String::new();
    stdout.read_to_string(&mut buf).ok()?;
    Some(buf.trim().to_string())
}

/// Lowercase hex encoding of a byte slice.
fn hex(bytes: &[u8]) -> String {
    // A single nibble (0..=15) as a lowercase hex digit. No indexing/Result, so
    // it trips neither the panic nor the must-use lints.
    fn nibble(n: u8) -> char {
        (if n < 10 { b'0' + n } else { b'a' + n - 10 }) as char
    }
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(nibble(b >> 4));
        s.push(nibble(b & 0x0f));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    /// Init a repo at `dir` with `n` commits on a fixed branch name.
    fn init_repo(dir: &Path, n: usize) {
        run(dir, &["init", "-q", "-b", "main"]);
        for i in 0..n {
            std::fs::write(dir.join(format!("f{i}")), format!("{i}")).unwrap();
            run(dir, &["add", "."]);
            run(dir, &["commit", "-q", "-m", &format!("c{i}")]);
        }
    }

    #[test]
    fn fingerprint_is_stable_and_hashed() {
        let cfg = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path(), 3);

        let fp = fingerprint_in(cfg.path(), repo.path()).expect("fingerprint");
        // SHA-256 hex is 64 chars, and is *not* the raw 40-char commit hash.
        assert_eq!(fp.len(), 64);

        // Same answer on a repeat call (now served from cache).
        let again = fingerprint_in(cfg.path(), repo.path()).expect("fingerprint");
        assert_eq!(fp, again);
    }

    #[test]
    fn two_clones_share_a_fingerprint() {
        // The whole point: same root commit -> same fingerprint, independent of
        // path/name. Two independent clones must agree.
        let cfg = tempfile::tempdir().unwrap();
        let origin = tempfile::tempdir().unwrap();
        init_repo(origin.path(), 2);

        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        run(
            a.path(),
            &["clone", "-q", origin.path().to_str().unwrap(), "."],
        );
        run(
            b.path(),
            &["clone", "-q", origin.path().to_str().unwrap(), "."],
        );

        let fa = fingerprint_in(cfg.path(), a.path()).expect("a");
        let fb = fingerprint_in(cfg.path(), b.path()).expect("b");
        assert_eq!(fa, fb);
    }

    #[test]
    fn cache_short_circuits_git() {
        let cfg = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path(), 1);

        let fp = fingerprint_in(cfg.path(), repo.path()).expect("fingerprint");
        // Destroy the repo: a non-cached path would now fail. The cache (keyed
        // by work dir) must still return the original value.
        std::fs::remove_dir_all(repo.path().join(".git")).unwrap();
        let cached = fingerprint_in(cfg.path(), repo.path()).expect("cached");
        assert_eq!(fp, cached);
    }

    #[test]
    fn non_repo_yields_none() {
        let cfg = tempfile::tempdir().unwrap();
        let plain = tempfile::tempdir().unwrap();
        assert!(fingerprint_in(cfg.path(), plain.path()).is_none());
    }

    #[test]
    fn shallow_clone_without_root_yields_none() {
        // A depth-1 clone (the GitHub Actions default) can't reach the root: the
        // grafted boundary commit must not be reported as a fingerprint.
        let cfg = tempfile::tempdir().unwrap();
        let origin = tempfile::tempdir().unwrap();
        init_repo(origin.path(), 5);

        let shallow = tempfile::tempdir().unwrap();
        // `--no-local` forces the real (graft-creating) shallow transport.
        run(
            shallow.path(),
            &[
                "clone",
                "-q",
                "--depth=1",
                "--no-local",
                &format!("file://{}", origin.path().display()),
                ".",
            ],
        );

        assert!(is_shallow(shallow.path()), "expected a shallow clone");
        assert!(fingerprint_in(cfg.path(), shallow.path()).is_none());
    }
}
