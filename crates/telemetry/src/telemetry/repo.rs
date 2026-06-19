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
//! Sources for the root commit, in order:
//! 1. Local git: `git rev-list --max-parents=0 HEAD`.
//! 2. On a shallow clone whose history can't reach the root (the default depth-1
//!    GitHub Actions checkout), the GitHub REST API: ask for one commit (starting
//!    from `GITHUB_SHA`/HEAD so the walk follows HEAD's ancestry, matching local
//!    git), read the `Link` header to jump to the last page, and read the first
//!    commit's SHA from there. Because this yields the *same* root SHA a full
//!    clone would, the fingerprint is identical to one computed locally — same
//!    namespace, no special-casing downstream. Gated to GitHub Actions with a
//!    token present.
//!
//! Everything here is strictly best-effort. No git, not a repo, no token, a
//! failed API call, an unreadable cache — any of these just yields `None`, and
//! the attr is omitted. API failures are logged at `trace`.
//!
//! The work never runs on the exit path: a startup warmer ([`prewarm`]) computes
//! and caches the fingerprint on a detached thread, while the exit path
//! ([`cached`]) only ever reads the cache. A cold cache just omits the attr for
//! that run; it lands on the next.
//!
//! Caching (under the config dir, keyed by the working directory):
//! - A success is cached forever, so the (O(history) git walk or two API hops)
//!   runs at most once per repo per machine.
//! - A *failure* is cached too, with a timestamp and a [`NEGATIVE_TTL`]: a huge
//!   monorepo whose root walk keeps hitting [`GIT_TIMEOUT`], or a transient API
//!   failure, would otherwise re-pay the cost on every warm-up. The negative
//!   entry expires so a later full fetch / fixed token is eventually picked up.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Cap on a single git subprocess. On a giant monorepo the root walk can be slow,
/// so we bound it and give up rather than block the warmer indefinitely.
const GIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Cap on the GitHub API round-trips, so a hung endpoint can't pin the warmer.
const API_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a cached *failure* suppresses recomputation. Keeps a repo whose root
/// walk keeps timing out (or whose API call keeps failing) from re-paying it
/// every run, while still letting a later full fetch be picked up within a day.
const NEGATIVE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Default GitHub REST API host (overridden by `GITHUB_API_URL`, e.g. on GHES).
const API_DEFAULT: &str = "https://api.github.com";

/// Read-only lookup for the exit path: the cached fingerprint if one was already
/// computed (by a prior run or this run's [`prewarm`]). Never runs git or the
/// network, never blocks — a cold cache just yields `None` and the attr is
/// omitted this run, then appears once the warmer lands.
pub fn cached(config_dir: &Path) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    match read_cache(&cache_path_for(config_dir, &cwd), now_ms()) {
        Cached::Hit(value) => Some(value),
        Cached::FreshMiss | Cached::Stale | Cached::Absent => None,
    }
}

/// Compute the fingerprint for the process's working directory and cache it.
/// Meant to run off the hot path (a startup warmer thread) so the exit path only
/// ever reads the cache. Honors the cache, so it's free once warm. Falls back to
/// the GitHub API (see [`github_root_commit`]) only when local git can't reach
/// the root.
pub fn prewarm(config_dir: &Path) {
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let _ = fingerprint_in(config_dir, &cwd, github_root_commit, now_ms());
}

/// Inner form taking the working directory, a lazy GitHub-API fallback, and the
/// current time explicitly (the public entries supply the process cwd / env /
/// clock). Split out so tests can target a specific repo, fallback, and clock
/// without global state. The fallback is invoked only when local git fails.
fn fingerprint_in(
    config_dir: &Path,
    work_dir: &Path,
    gh_root: impl FnOnce() -> Option<String>,
    now: u64,
) -> Option<String> {
    let cache_path = cache_path_for(config_dir, work_dir);

    match read_cache(&cache_path, now) {
        Cached::Hit(value) => return Some(value),
        Cached::FreshMiss => return None,
        Cached::Stale | Cached::Absent => {}
    }

    let computed = compute(work_dir, gh_root);
    write_cache(&cache_path, computed.as_deref(), now);
    computed
}

/// Compute the fingerprint from scratch: hash the root commit id(s) from local
/// git, or — when those are unreachable — from the GitHub API fallback.
fn compute(work_dir: &Path, gh_root: impl FnOnce() -> Option<String>) -> Option<String> {
    let root = root_commit(work_dir).or_else(gh_root)?;
    Some(hex::encode(Sha256::digest(root.as_bytes())))
}

/// The cache file for a working directory: `<config_dir>/repo-fingerprints/<h>`,
/// where `h` is a hash of the directory path (so distinct checkouts don't
/// collide and the path itself isn't stored).
fn cache_path_for(config_dir: &Path, work_dir: &Path) -> PathBuf {
    config_dir
        .join("repo-fingerprints")
        .join(hex::encode(Sha256::digest(
            work_dir.as_os_str().as_encoded_bytes(),
        )))
}

/// Outcome of consulting the on-disk cache.
enum Cached {
    /// A previously computed fingerprint.
    Hit(String),
    /// A recent failure — still within the negative TTL, so don't recompute.
    FreshMiss,
    /// A failure older than the TTL — recompute.
    Stale,
    /// No (or unreadable) cache entry.
    Absent,
}

/// Parse the cache file. Two record shapes, tab-separated:
/// - `ok\t<value>` — a computed fingerprint.
/// - `none\t<unix_ms>` — a failure, stamped so it can expire.
fn read_cache(path: &Path, now: u64) -> Cached {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Cached::Absent;
    };
    let line = raw.trim();
    if let Some(value) = line.strip_prefix("ok\t") {
        let value = value.trim();
        return if value.is_empty() {
            Cached::Absent
        } else {
            Cached::Hit(value.to_string())
        };
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
/// next warm-up recomputes.
fn write_cache(path: &Path, value: Option<&str>, now: u64) {
    let body = match value {
        Some(v) => format!("ok\t{v}"),
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
/// when its history actually reaches it — otherwise `None`, and the caller falls
/// back to the GitHub API.
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

/// The repository's root commit SHA via the GitHub REST API — the shallow-CI
/// fallback. Returns `None` unless we're in GitHub Actions with a token and a
/// known `owner/repo`; every failure is logged at `trace` and swallowed.
///
/// Two hops: GET one commit (starting from `GITHUB_SHA`/HEAD when known), read
/// the `Link` header's `rel="last"` URL (the oldest commit's page), then GET that
/// and read its SHA. A repo with a single page (≤1 commit) has no `Link` header,
/// so the first response already holds it.
fn github_root_commit() -> Option<String> {
    if std::env::var("GITHUB_ACTIONS").ok().as_deref() != Some("true") {
        return None;
    }
    let token = nonempty_env("GITHUB_TOKEN")?;
    let repo = nonempty_env("GITHUB_REPOSITORY")?; // "owner/name"
    let api = nonempty_env("GITHUB_API_URL").unwrap_or_else(|| API_DEFAULT.to_string());
    // Walk HEAD's own ancestry (so the root matches local `rev-list HEAD`), not
    // the default branch's. `GITHUB_SHA` is the checked-out commit; absent, the
    // API defaults to the default branch.
    let head = nonempty_env("GITHUB_SHA");

    // reqwest::blocking spins up its own runtime; we're already on a dedicated
    // warmer thread (never the async runtime), so this is safe.
    let client = match reqwest::blocking::Client::builder()
        .timeout(API_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::trace!(error = %e, "telemetry: github api client build failed");
            return None;
        }
    };

    root_via_api(
        |url| gh_get(&client, url, &token),
        &api,
        &repo,
        head.as_deref(),
    )
}

/// A trimmed, non-empty env var, or `None`.
fn nonempty_env(name: &str) -> Option<String> {
    let v = std::env::var(name).ok()?;
    let v = v.trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

/// The bits of an HTTP response we care about: the `Link` header and the body.
struct HttpResp {
    link: Option<String>,
    body: String,
}

/// Orchestrate the two-hop root-commit lookup over an injected fetcher (so the
/// pagination/JSON logic is testable without the network). When `head` is set,
/// the listing starts from it (HEAD's ancestry) instead of the default branch.
fn root_via_api(
    fetch: impl Fn(&str) -> Option<HttpResp>,
    api: &str,
    repo: &str,
    head: Option<&str>,
) -> Option<String> {
    let mut first_url = format!(
        "{}/repos/{}/commits?per_page=1",
        api.trim_end_matches('/'),
        repo
    );
    if let Some(head) = head {
        first_url.push_str("&sha=");
        first_url.push_str(head);
    }
    let first = fetch(&first_url)?;
    let body = match first.link.as_deref().and_then(parse_rel_last) {
        // Jump straight to the last page — its single commit is the root.
        Some(last) => fetch(&last)?.body,
        // No pagination => ≤1 commit total => the first page already holds it.
        None => first.body,
    };
    first_sha(&body)
}

/// Perform one authenticated GitHub API GET. Logs failures at `trace`.
fn gh_get(client: &reqwest::blocking::Client, url: &str, token: &str) -> Option<HttpResp> {
    let resp = match client
        .get(url)
        .header(reqwest::header::USER_AGENT, "heph-telemetry")
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .bearer_auth(token)
        .send()
    {
        Ok(r) => r,
        Err(e) => {
            tracing::trace!(error = %e, url, "telemetry: github api request failed");
            return None;
        }
    };
    if !resp.status().is_success() {
        tracing::trace!(status = %resp.status(), url, "telemetry: github api non-success");
        return None;
    }
    let link = resp
        .headers()
        .get(reqwest::header::LINK)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    match resp.text() {
        Ok(body) => Some(HttpResp { link, body }),
        Err(e) => {
            tracing::trace!(error = %e, "telemetry: github api body read failed");
            None
        }
    }
}

/// Extract the `rel="last"` URL from a GitHub `Link` header, if present.
fn parse_rel_last(link: &str) -> Option<String> {
    link.split(',')
        .map(str::trim)
        .find(|part| part.contains("rel=\"last\""))
        .and_then(|part| {
            let after = part.split_once('<')?.1;
            let url = after.split_once('>')?.0;
            Some(url.to_string())
        })
}

/// The `sha` of the first commit in a GitHub commits-list JSON body.
fn first_sha(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let sha = v.as_array()?.first()?.get("sha")?.as_str()?;
    if sha.is_empty() {
        None
    } else {
        Some(sha.to_string())
    }
}

/// Current wall-clock as unix milliseconds (for stamping negative cache entries).
fn now_ms() -> u64 {
    hcore::events::now_unix_ms()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
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

    /// A gh fallback that must never be called.
    fn no_gh() -> Option<String> {
        None
    }

    #[test]
    fn fingerprint_is_stable_and_hashed() {
        let cfg = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path(), 3);

        let fp = fingerprint_in(cfg.path(), repo.path(), no_gh, T0).expect("fingerprint");
        // SHA-256 hex is 64 chars, and is *not* the raw 40-char commit hash.
        assert_eq!(fp.len(), 64);

        // Same answer on a repeat call (now served from cache).
        let again = fingerprint_in(cfg.path(), repo.path(), no_gh, T0).expect("fingerprint");
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

        let fa = fingerprint_in(cfg.path(), a.path(), no_gh, T0).expect("a");
        let fb = fingerprint_in(cfg.path(), b.path(), no_gh, T0).expect("b");
        assert_eq!(fa, fb);
    }

    #[test]
    fn cache_short_circuits_git() {
        let cfg = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path(), 1);

        let fp = fingerprint_in(cfg.path(), repo.path(), no_gh, T0).expect("fingerprint");
        // Destroy the repo: a non-cached path would now fail. The cache (keyed
        // by work dir) must still return the original value.
        std::fs::remove_dir_all(repo.path().join(".git")).unwrap();
        let cached = fingerprint_in(cfg.path(), repo.path(), no_gh, T0).expect("cached");
        assert_eq!(fp, cached);
    }

    #[test]
    fn non_repo_yields_none() {
        let cfg = tempfile::tempdir().unwrap();
        let plain = tempfile::tempdir().unwrap();
        assert!(fingerprint_in(cfg.path(), plain.path(), no_gh, T0).is_none());
    }

    #[test]
    fn shallow_clone_without_root_yields_none() {
        // A depth-1 clone (the GitHub Actions default) can't reach the root: the
        // grafted boundary commit must not be reported, and with no API fallback
        // there is nothing to report.
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
        assert!(fingerprint_in(cfg.path(), shallow.path(), no_gh, T0).is_none());
    }

    #[test]
    fn github_api_root_used_when_local_git_fails() {
        // No local root reachable -> the API fallback supplies the root SHA, and
        // it hashes into the *same namespace* as a local clone would.
        let cfg = tempfile::tempdir().unwrap();
        let plain = tempfile::tempdir().unwrap(); // not a repo
        let api_sha = "e83c5163316f89bfbde7d9ab23ca2e25604af290";

        let fp = fingerprint_in(cfg.path(), plain.path(), || Some(api_sha.to_string()), T0)
            .expect("api fallback");
        assert_eq!(fp, hex::encode(Sha256::digest(api_sha.as_bytes())));
    }

    #[test]
    fn local_root_wins_and_skips_github() {
        // A real local root takes precedence; the API fallback must not run.
        let cfg = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path(), 1);

        let called = Cell::new(false);
        let fp = fingerprint_in(
            cfg.path(),
            repo.path(),
            || {
                called.set(true);
                Some("unused".to_string())
            },
            T0,
        )
        .expect("local root");
        assert!(!called.get(), "github fallback should not be consulted");
        assert_eq!(fp.len(), 64);
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
        assert!(fingerprint_in(cfg.path(), repo.path(), no_gh, T0).is_none());

        // Past the TTL the entry is stale -> recompute, and now we get a value.
        let later = T0 + NEGATIVE_TTL.as_millis() as u64 + 1;
        let fp = fingerprint_in(cfg.path(), repo.path(), no_gh, later).expect("recomputed");
        assert_eq!(fp.len(), 64);
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
        fingerprint_in(cfg.path(), repo.path(), no_gh, T0).expect("warm");
        assert!(matches!(read_cache(&path, T0), Cached::Hit(_)));
    }

    #[test]
    fn failure_is_negatively_cached() {
        let cfg = tempfile::tempdir().unwrap();
        let plain = tempfile::tempdir().unwrap();
        assert!(fingerprint_in(cfg.path(), plain.path(), no_gh, T0).is_none());

        // The miss is recorded so a giant repo that keeps timing out doesn't
        // re-pay the walk every run.
        let cache = cache_path_for(cfg.path(), plain.path());
        let body = std::fs::read_to_string(&cache).expect("negative entry written");
        assert!(body.starts_with("none\t"), "got: {body}");
    }

    #[test]
    fn parse_rel_last_extracts_last_url() {
        let header = "<https://api.github.com/repositories/123/commits?per_page=1&page=2>; \
             rel=\"next\", \
             <https://api.github.com/repositories/123/commits?per_page=1&page=845>; rel=\"last\"";
        assert_eq!(
            parse_rel_last(header).as_deref(),
            Some("https://api.github.com/repositories/123/commits?per_page=1&page=845")
        );
        // No `last` relation (single page) -> nothing to jump to.
        assert!(parse_rel_last("<https://x/y>; rel=\"self\"").is_none());
    }

    #[test]
    fn first_sha_reads_array_head() {
        assert_eq!(
            first_sha(r#"[{"sha":"abc123","commit":{}}]"#).as_deref(),
            Some("abc123")
        );
        assert!(first_sha("[]").is_none());
        assert!(first_sha("not json").is_none());
    }

    #[test]
    fn root_via_api_jumps_to_last_page() {
        // Multi-page: the first hop yields a `last` link; the second hop's body
        // carries the root commit.
        let last_url = "https://api.test/repositories/9/commits?per_page=1&page=42";
        let fetch = |url: &str| {
            if url.ends_with("/repos/o/r/commits?per_page=1") {
                Some(HttpResp {
                    link: Some(format!("<{last_url}>; rel=\"last\"")),
                    body: r#"[{"sha":"newest"}]"#.to_string(),
                })
            } else if url == last_url {
                Some(HttpResp {
                    link: None,
                    body: r#"[{"sha":"rootsha"}]"#.to_string(),
                })
            } else {
                None
            }
        };
        assert_eq!(
            root_via_api(fetch, "https://api.test", "o/r", None).as_deref(),
            Some("rootsha")
        );
    }

    #[test]
    fn root_via_api_single_page_uses_first_body() {
        // One-commit repo: no `Link` header, so the first response is the root.
        let fetch = |_url: &str| {
            Some(HttpResp {
                link: None,
                body: r#"[{"sha":"only"}]"#.to_string(),
            })
        };
        assert_eq!(
            root_via_api(fetch, "https://api.test", "o/r", None).as_deref(),
            Some("only")
        );
    }

    #[test]
    fn root_via_api_pins_to_head_sha() {
        // A HEAD ref is threaded into the listing as `&sha=`, so the walk follows
        // HEAD's ancestry rather than the default branch.
        let seen = Cell::new(false);
        let fetch = |url: &str| {
            if url.contains("&sha=headref") {
                seen.set(true);
            }
            Some(HttpResp {
                link: None,
                body: r#"[{"sha":"root"}]"#.to_string(),
            })
        };
        assert_eq!(
            root_via_api(fetch, "https://api.test", "o/r", Some("headref")).as_deref(),
            Some("root")
        );
        assert!(seen.get(), "request must pin to the HEAD sha");
    }
}
