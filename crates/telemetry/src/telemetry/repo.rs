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
//! On a shallow clone whose history can't reach the root (the default depth-1
//! GitHub Actions checkout), the root is unavailable, so we fall back to a hash
//! of `GITHUB_REPOSITORY_ID` — GitHub's immutable numeric repo id (rename- and
//! transfer-safe). That is a *different namespace* than the root-commit hash, so
//! every fingerprint also carries a [`Fingerprint::source`] tag and the two are
//! never conflated.
//!
//! Everything here is strictly best-effort. No git, not a repo, an unreadable
//! cache — any of these just yields `None`, and the attrs are omitted.
//!
//! The `git rev-list` walk is O(history) and never runs on the exit path: a
//! startup warmer ([`prewarm`]) computes and caches it on a detached thread,
//! while the exit path ([`cached`]) only ever reads the cache. A cold cache just
//! omits the attr for that run; it lands on the next.
//!
//! Caching (under the config dir, keyed by the working directory):
//! - A success is cached forever, so the walk runs at most once per repo per
//!   machine.
//! - A *failure* is cached too, with a timestamp and a [`NEGATIVE_TTL`]: a huge
//!   monorepo whose root walk keeps hitting [`GIT_TIMEOUT`] would otherwise re-pay
//!   the walk on every warm-up. The negative entry expires so a later full fetch
//!   is eventually picked up.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Cap on a single git subprocess. Telemetry runs on the exit path and must not
/// stall a real command — on a giant monorepo the root walk can be slow, so we
/// bound it and give up rather than block.
const GIT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a cached *failure* suppresses recomputation. Keeps a repo whose root
/// walk keeps timing out from re-paying it every run, while still letting a later
/// full fetch be picked up within a day.
const NEGATIVE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Where the fingerprint came from — recorded alongside the value so the two
/// namespaces (root commit vs CI repo id) are never silently mixed in analysis.
mod source {
    /// SHA-256 of the git root commit(s). The portable, cross-machine identity.
    pub const ROOT_COMMIT: &str = "root_commit";
    /// SHA-256 of `GITHUB_REPOSITORY_ID`. Shallow-CI fallback only.
    pub const GITHUB_REPO_ID: &str = "github_repo_id";

    /// Whether `s` is a tag we wrote (guards against a corrupt cache file).
    pub fn is_known(s: &str) -> bool {
        s == ROOT_COMMIT || s == GITHUB_REPO_ID
    }
}

/// A computed fingerprint plus the kind of identity it was derived from.
#[derive(Debug, Clone)]
pub struct Fingerprint {
    pub value: String,
    pub source: &'static str,
}

/// Read-only lookup for the exit path: the cached fingerprint if one was already
/// computed (by a prior run or this run's [`prewarm`]). Never runs git, never
/// blocks — a cold cache just yields `None` and the attr is omitted this run,
/// then appears once the warmer lands.
pub fn cached(config_dir: &Path) -> Option<Fingerprint> {
    let cwd = std::env::current_dir().ok()?;
    match read_cache(&cache_path_for(config_dir, &cwd), now_ms()) {
        Cached::Hit(fp) => Some(fp),
        Cached::FreshMiss | Cached::Stale | Cached::Absent => None,
    }
}

/// Compute the fingerprint for the process's working directory and cache it.
/// Meant to run off the hot path (a startup warmer thread) so the exit path only
/// ever reads the cache. Honors the cache, so it's free once warm. Reads
/// `GITHUB_REPOSITORY_ID` for the shallow-CI fallback.
pub fn prewarm(config_dir: &Path) {
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let ci_repo_id = std::env::var("GITHUB_REPOSITORY_ID").ok();
    let _ = fingerprint_in(config_dir, &cwd, ci_repo_id.as_deref(), now_ms());
}

/// Inner form taking the working directory, the CI repo id, and the current time
/// explicitly (the public entry supplies the process cwd / env / clock). Split
/// out so tests can target a specific repo and clock without global state.
fn fingerprint_in(
    config_dir: &Path,
    work_dir: &Path,
    ci_repo_id: Option<&str>,
    now: u64,
) -> Option<Fingerprint> {
    let cache_path = cache_path_for(config_dir, work_dir);

    match read_cache(&cache_path, now) {
        Cached::Hit(fp) => return Some(fp),
        Cached::FreshMiss => return None,
        Cached::Stale | Cached::Absent => {}
    }

    let computed = compute(work_dir, ci_repo_id);
    write_cache(&cache_path, computed.as_ref(), now);
    computed
}

/// Compute the fingerprint from scratch: the git root commit if reachable,
/// otherwise the CI repo id, otherwise nothing.
fn compute(work_dir: &Path, ci_repo_id: Option<&str>) -> Option<Fingerprint> {
    if let Some(root) = root_commit(work_dir) {
        return Some(Fingerprint {
            value: hex(&Sha256::digest(root.as_bytes())),
            source: source::ROOT_COMMIT,
        });
    }
    // Shallow CI: the root is unreachable, so fall back to GitHub's immutable
    // numeric repo id (a different namespace — tagged accordingly).
    let id = ci_repo_id?.trim();
    if id.is_empty() {
        return None;
    }
    Some(Fingerprint {
        value: hex(&Sha256::digest(id.as_bytes())),
        source: source::GITHUB_REPO_ID,
    })
}

/// The cache file for a working directory: `<config_dir>/repo-fingerprints/<h>`,
/// where `h` is a hash of the directory path (so distinct checkouts don't
/// collide and the path itself isn't stored).
fn cache_path_for(config_dir: &Path, work_dir: &Path) -> PathBuf {
    config_dir
        .join("repo-fingerprints")
        .join(hex(&Sha256::digest(
            work_dir.as_os_str().as_encoded_bytes(),
        )))
}

/// Outcome of consulting the on-disk cache.
enum Cached {
    /// A previously computed fingerprint.
    Hit(Fingerprint),
    /// A recent failure — still within the negative TTL, so don't recompute.
    FreshMiss,
    /// A failure older than the TTL — recompute.
    Stale,
    /// No (or unreadable) cache entry.
    Absent,
}

/// Parse the cache file. Two record shapes, tab-separated:
/// - `ok\t<source>\t<value>` — a computed fingerprint.
/// - `none\t<unix_ms>` — a failure, stamped so it can expire.
fn read_cache(path: &Path, now: u64) -> Cached {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Cached::Absent;
    };
    let line = raw.trim();
    if let Some(rest) = line.strip_prefix("ok\t") {
        if let Some((src, value)) = rest.split_once('\t')
            && source::is_known(src)
            && !value.is_empty()
        {
            let source = if src == source::ROOT_COMMIT {
                source::ROOT_COMMIT
            } else {
                source::GITHUB_REPO_ID
            };
            return Cached::Hit(Fingerprint {
                value: value.to_string(),
                source,
            });
        }
        return Cached::Absent;
    }
    if let Some(ts) = line.strip_prefix("none\t") {
        return match ts.trim().parse::<u64>() {
            Ok(stamped) if now.saturating_sub(stamped) < NEGATIVE_TTL.as_millis() as u64 => {
                Cached::FreshMiss
            }
            Ok(_) => Cached::Stale,
            Err(_) => Cached::Absent,
        };
    }
    Cached::Absent
}

/// Persist the computation outcome. Best-effort — a failure here just means the
/// next run recomputes.
fn write_cache(path: &Path, computed: Option<&Fingerprint>, now: u64) {
    let body = match computed {
        Some(fp) => format!("ok\t{}\t{}", fp.source, fp.value),
        None => format!("none\t{now}"),
    };
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_ok()
    {
        drop(std::fs::write(path, body));
    }
}

/// The repository's root commit id(s): the output of
/// `git rev-list --max-parents=0 HEAD`, sorted and joined (a repo can have
/// several roots from merged histories — sorting keeps the value stable
/// regardless of git's enumeration order).
///
/// Shallow-aware: a default `actions/checkout` is a depth-1 clone, where the
/// grafted boundary commit *masquerades* as a root. Those grafts (listed in
/// `.git/shallow`) are filtered out, so a shallow clone yields the true root only
/// when its history actually reaches it — otherwise `None`, never a per-checkout
/// value that would shatter one repo into thousands of fingerprints.
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

/// Current wall-clock as unix milliseconds (for stamping negative cache entries).
fn now_ms() -> u64 {
    hcore::events::now_unix_ms()
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

    const T0: u64 = 1_000_000_000_000;

    #[test]
    fn fingerprint_is_stable_and_hashed() {
        let cfg = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path(), 3);

        let fp = fingerprint_in(cfg.path(), repo.path(), None, T0).expect("fingerprint");
        // SHA-256 hex is 64 chars, and is *not* the raw 40-char commit hash.
        assert_eq!(fp.value.len(), 64);
        assert_eq!(fp.source, source::ROOT_COMMIT);

        // Same answer on a repeat call (now served from cache).
        let again = fingerprint_in(cfg.path(), repo.path(), None, T0).expect("fingerprint");
        assert_eq!(fp.value, again.value);
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

        let fa = fingerprint_in(cfg.path(), a.path(), None, T0).expect("a");
        let fb = fingerprint_in(cfg.path(), b.path(), None, T0).expect("b");
        assert_eq!(fa.value, fb.value);
    }

    #[test]
    fn cache_short_circuits_git() {
        let cfg = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path(), 1);

        let fp = fingerprint_in(cfg.path(), repo.path(), None, T0).expect("fingerprint");
        // Destroy the repo: a non-cached path would now fail. The cache (keyed
        // by work dir) must still return the original value.
        std::fs::remove_dir_all(repo.path().join(".git")).unwrap();
        let cached = fingerprint_in(cfg.path(), repo.path(), None, T0).expect("cached");
        assert_eq!(fp.value, cached.value);
    }

    #[test]
    fn non_repo_yields_none() {
        let cfg = tempfile::tempdir().unwrap();
        let plain = tempfile::tempdir().unwrap();
        assert!(fingerprint_in(cfg.path(), plain.path(), None, T0).is_none());
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
        assert!(fingerprint_in(cfg.path(), shallow.path(), None, T0).is_none());
    }

    #[test]
    fn shallow_clone_falls_back_to_github_repo_id() {
        // When the root is unreachable but the CI repo id is present, fall back
        // to it — tagged as a distinct source.
        let cfg = tempfile::tempdir().unwrap();
        let plain = tempfile::tempdir().unwrap(); // not a repo -> no root commit

        let fp =
            fingerprint_in(cfg.path(), plain.path(), Some("123456"), T0).expect("github fallback");
        assert_eq!(fp.source, source::GITHUB_REPO_ID);
        assert_eq!(fp.value, hex(&Sha256::digest(b"123456")));

        // Same id anywhere -> same fingerprint (stable across CI runs).
        let other = tempfile::tempdir().unwrap();
        let fp2 = fingerprint_in(cfg.path(), other.path(), Some("123456"), T0).expect("again");
        assert_eq!(fp.value, fp2.value);
    }

    #[test]
    fn root_commit_wins_over_github_id() {
        // A real root is the portable identity and takes precedence over the
        // CI-only fallback even when both are available.
        let cfg = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path(), 1);

        let fp = fingerprint_in(cfg.path(), repo.path(), Some("999"), T0).expect("root");
        assert_eq!(fp.source, source::ROOT_COMMIT);
    }

    #[test]
    fn negative_cache_skips_then_expires() {
        let cfg = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path(), 1);
        let cache = cache_path_for(cfg.path(), repo.path());
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();

        // A *fresh* negative entry short-circuits computation: even though the
        // repo has a perfectly good root, we honor the recent failure and skip.
        std::fs::write(&cache, format!("none\t{T0}")).unwrap();
        assert!(fingerprint_in(cfg.path(), repo.path(), None, T0).is_none());

        // Past the TTL the entry is stale -> recompute, and now we get a value.
        let later = T0 + NEGATIVE_TTL.as_millis() as u64 + 1;
        let fp = fingerprint_in(cfg.path(), repo.path(), None, later).expect("recomputed");
        assert_eq!(fp.source, source::ROOT_COMMIT);
    }

    #[test]
    fn read_only_lookup_is_cold_until_warmed() {
        // The exit path (`cached`) only reads; computation happens in `prewarm`.
        let cfg = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path(), 1);
        let path = cache_path_for(cfg.path(), repo.path());

        // Cold: nothing computed yet -> the read-only lookup misses.
        assert!(matches!(read_cache(&path, T0), Cached::Absent));

        // Warm it (the prewarm path), then the read-only lookup hits.
        fingerprint_in(cfg.path(), repo.path(), None, T0).expect("warm");
        assert!(matches!(read_cache(&path, T0), Cached::Hit(_)));
    }

    #[test]
    fn failure_is_negatively_cached() {
        let cfg = tempfile::tempdir().unwrap();
        let plain = tempfile::tempdir().unwrap();
        assert!(fingerprint_in(cfg.path(), plain.path(), None, T0).is_none());

        // The miss is recorded so a giant repo that keeps timing out doesn't
        // re-pay the walk every run.
        let cache = cache_path_for(cfg.path(), plain.path());
        let body = std::fs::read_to_string(&cache).expect("negative entry written");
        assert!(body.starts_with("none\t"), "got: {body}");
    }
}
