//! A GitHub Actions hook: folds the engine's build-event stream into a status
//! tally (done/total, built, cached, failed, slow targets) and surfaces it two
//! ways. Published out-of-process as a cdylib (see `plugin-gha-cdylib`) and
//! enabled via a config `plugins:` entry.
//!
//! - **Live**, while the job runs: a dedicated GitHub **check run** whose markdown
//!   `output` is PATCHed on a timer. A check run is the only Actions surface that
//!   updates in real time — `$GITHUB_STEP_SUMMARY` is rendered by GitHub *only*
//!   when the step ends, so it cannot show live progress.
//! - **At the end**: the full markdown is written once to `$GITHUB_STEP_SUMMARY`.
//!
//! The aggregation is intentionally self-contained (a small [`Tally`]) rather than
//! reusing the TUI's `BuildState`: the renderer's aggregator is coupled to ratatui
//! and lives in the (terminal) `tui` crate, and a hook only needs a handful of
//! counts plus the in-flight set for slow detection.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hcore::events::{BuildEvent, BuildEventKind, now_unix_ms};
use hplugin::config::{Options, decode_opt, deny_unknown};
use hplugin::hook::Hook;

/// A target executing longer than this (ms) is surfaced in the "slow targets"
/// section of the summary.
const SLOW_THRESHOLD_MS: u64 = 10_000;

/// `$GITHUB_STEP_SUMMARY` is capped at 1 MiB by GitHub. The header is tiny and
/// fixed; the only unbounded sections are the slow + failed lists, so they are
/// truncated to the slowest / first N (with a "…and X more" line) to keep every
/// snapshot well under the limit.
const MAX_SLOW_ROWS: usize = 20;
const MAX_FAILED_ROWS: usize = 50;

/// One long-running phase a target can currently be in. A target is slow because
/// of a *single* active phase (it executes, or pulls from cache, or writes cache —
/// not several at once), so the summary names which one.
#[derive(Clone, Copy)]
enum Phase {
    Execute,
    CachePull,
    LocalCacheWrite,
    RemoteCacheWrite,
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Phase::Execute => "execute",
            Phase::CachePull => "cache pull",
            Phase::LocalCacheWrite => "cache write",
            Phase::RemoteCacheWrite => "remote cache write",
        }
    }
}

/// The folded build status. Only what the summary renders: matched/finished sets
/// for progress, cache-hit + built counts, the failure list, and the in-flight
/// active phase per target for slow detection.
#[derive(Default)]
struct Tally {
    matched: BTreeSet<String>,
    matched_complete: bool,
    finished: BTreeSet<String>,
    built: usize,
    cache_hit: BTreeSet<String>,
    failed: Vec<(String, Option<String>)>,
    /// addr -> (active phase, its start timestamp). Set on a phase `*Start`,
    /// cleared on the matching `*End`; the remaining entries are the targets
    /// currently inside a phase (one phase each).
    running: BTreeMap<String, (Phase, u64)>,
}

impl Tally {
    fn apply(&mut self, ev: &BuildEvent) {
        match &ev.kind {
            BuildEventKind::Matched { addrs, complete } => {
                for a in addrs {
                    self.matched.insert(a.clone());
                }
                if *complete {
                    self.matched_complete = true;
                }
            }
            BuildEventKind::ResultEnd { addr, error } => {
                self.finished.insert(addr.clone());
                if let Some(e) = error {
                    self.failed.push((addr.clone(), Some(e.clone())));
                }
            }
            BuildEventKind::ExecuteStart { addr, .. } => {
                self.running
                    .insert(addr.clone(), (Phase::Execute, ev.at_unix_ms));
            }
            BuildEventKind::ExecuteEnd { addr, error } => {
                self.running.remove(addr);
                if error.is_none() {
                    self.built += 1;
                }
            }
            BuildEventKind::RemoteCacheReadStart { addr } => {
                self.running
                    .insert(addr.clone(), (Phase::CachePull, ev.at_unix_ms));
            }
            BuildEventKind::LocalCacheWriteStart { addr } => {
                self.running
                    .insert(addr.clone(), (Phase::LocalCacheWrite, ev.at_unix_ms));
            }
            BuildEventKind::RemoteCacheWriteStart { addr } => {
                self.running
                    .insert(addr.clone(), (Phase::RemoteCacheWrite, ev.at_unix_ms));
            }
            BuildEventKind::RemoteCacheReadEnd { addr, .. }
            | BuildEventKind::LocalCacheWriteEnd { addr, .. }
            | BuildEventKind::RemoteCacheWriteEnd { addr, .. } => {
                self.running.remove(addr);
            }
            BuildEventKind::LocalCacheHit { addr } | BuildEventKind::RemoteCacheHit { addr } => {
                self.cache_hit.insert(addr.clone());
            }
            _ => {}
        }
    }

    /// `(done, total)` over the matched top-level set: done = matched targets that
    /// have finished. `total` is provisional until the matcher resolves.
    fn progress(&self) -> (usize, usize) {
        let done = self
            .matched
            .iter()
            .filter(|a| self.finished.contains(*a))
            .count();
        (done, self.matched.len())
    }

    fn cached_count(&self) -> usize {
        self.matched
            .iter()
            .filter(|a| self.cache_hit.contains(*a))
            .count()
    }

    /// Targets stuck in a single phase past [`SLOW_THRESHOLD_MS`], slowest first.
    /// Each carries the phase label so the summary shows *what* is slow.
    fn slow(&self, now_ms: u64) -> Vec<(String, &'static str, u64)> {
        let mut slow: Vec<(String, &'static str, u64)> = self
            .running
            .iter()
            .filter_map(|(addr, (phase, start))| {
                let elapsed = now_ms.saturating_sub(*start);
                (elapsed >= SLOW_THRESHOLD_MS).then(|| (addr.clone(), phase.label(), elapsed))
            })
            .collect();
        // Slowest first.
        slow.sort_by_key(|(_, _, elapsed)| std::cmp::Reverse(*elapsed));
        slow
    }

    /// Render the GitHub-Actions markdown for the current tally. `heading` is the
    /// H2 title (the heph command being run).
    fn render_markdown(&self, now_ms: u64, heading: &str) -> String {
        let (done, total) = self.progress();
        let total_str = if self.matched_complete {
            total.to_string()
        } else {
            format!("~{total}")
        };
        let mut out = String::new();
        out.push_str(&format!("## {heading}\n\n"));
        out.push_str(&format!(
            "**Targets:** {done} / {total_str} &nbsp;•&nbsp; **built:** {} &nbsp;•&nbsp; **cached:** {} &nbsp;•&nbsp; **failed:** {}\n",
            self.built,
            self.cached_count(),
            self.failed.len(),
        ));

        let slow = self.slow(now_ms);
        if !slow.is_empty() {
            out.push_str(
                "\n### Slow targets\n\n| target | phase | running for |\n| --- | --- | --- |\n",
            );
            for (addr, phase, elapsed) in slow.iter().take(MAX_SLOW_ROWS) {
                out.push_str(&format!("| `{addr}` | {phase} | {}s |\n", elapsed / 1000));
            }
            if slow.len() > MAX_SLOW_ROWS {
                out.push_str(&format!("\n…and {} more\n", slow.len() - MAX_SLOW_ROWS));
            }
        }

        if !self.failed.is_empty() {
            out.push_str("\n### Failed\n\n");
            for (addr, err) in self.failed.iter().take(MAX_FAILED_ROWS) {
                match err {
                    Some(e) => out.push_str(&format!(
                        "- `{addr}` — {}\n",
                        e.lines().next().unwrap_or("")
                    )),
                    None => out.push_str(&format!("- `{addr}`\n")),
                }
            }
            if self.failed.len() > MAX_FAILED_ROWS {
                out.push_str(&format!(
                    "\n…and {} more\n",
                    self.failed.len() - MAX_FAILED_ROWS
                ));
            }
        }
        out
    }
}

/// The commit a check run should attach to. On a `pull_request` event `GITHUB_SHA`
/// is the ephemeral *merge* commit (`refs/pull/N/merge`); a check attached there is
/// invisible on the PR. The commit reviewers actually see is the PR **head**, which
/// the event payload carries at `pull_request.head.sha`. So: on a PR event, read the
/// event JSON (`GITHUB_EVENT_PATH`) and use that head sha; otherwise (push, etc.)
/// fall back to `GITHUB_SHA`. This is why the workflow no longer needs to override
/// `GITHUB_SHA` itself.
fn resolve_head_sha() -> Option<String> {
    let nonempty = |s: String| Some(s).filter(|s| !s.is_empty());
    let event = std::env::var("GITHUB_EVENT_NAME").unwrap_or_default();
    if (event == "pull_request" || event == "pull_request_target")
        && let Some(sha) = pr_head_sha_from_event()
    {
        return Some(sha);
    }
    std::env::var("GITHUB_SHA").ok().and_then(nonempty)
}

/// Read the Actions event payload (`GITHUB_EVENT_PATH`) and pull
/// `pull_request.head.sha` out of it; `None` if unset / unreadable / absent.
fn pr_head_sha_from_event() -> Option<String> {
    let path = std::env::var("GITHUB_EVENT_PATH")
        .ok()
        .filter(|s| !s.is_empty())?;
    let bytes = std::fs::read(path).ok()?;
    pr_head_sha_from_json(&bytes)
}

/// Extract `pull_request.head.sha` from raw event-payload JSON. Pure (no env / fs)
/// so it is unit-testable; `None` if the bytes don't parse or the field is missing.
fn pr_head_sha_from_json(bytes: &[u8]) -> Option<String> {
    let json: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    json.get("pull_request")?
        .get("head")?
        .get("sha")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Live updates to a dedicated GitHub **check run**. A check run's markdown
/// `output` can be PATCHed while the job runs (the step summary cannot), so this
/// is the live surface. Configured from the Actions environment; disabled (with no
/// network calls) if any of token / repo / head-sha is missing. All calls are
/// best-effort — an API/network error is logged, never fatal.
struct ChecksClient {
    /// Built lazily on first use. The blocking client must not be constructed on a
    /// thread inside a tokio runtime; building it here, on the off-runtime update
    /// thread (or the `on_close` blocking thread), keeps that guaranteed regardless
    /// of where the plugin was loaded.
    http: std::sync::OnceLock<reqwest::blocking::Client>,
    api_url: String,
    repo: String,
    token: String,
    head_sha: String,
    /// The check run id, assigned by the first (create) call, reused by PATCHes.
    id: Mutex<Option<u64>>,
}

impl ChecksClient {
    /// Build from the Actions env (`GITHUB_TOKEN` / `GITHUB_REPOSITORY` /
    /// `GITHUB_SHA`, `GITHUB_API_URL` optional), or `None` if a required piece is
    /// absent. The head sha is resolved by [`resolve_head_sha`] so a check on a PR
    /// lands on the PR head commit, not the merge commit — no workflow override
    /// needed. `head_sha_override` (the `headSha` option) still wins if set.
    ///
    /// `token_env` names the env var the API token is read from (default
    /// `GITHUB_TOKEN`). Point it at a **GitHub App** installation token (or a PAT)
    /// to give the check run its own check suite — with the default `GITHUB_TOKEN`,
    /// GitHub nests the API-created run under another workflow's github-actions
    /// check suite (e.g. a labeler), which the check-runs API can't override.
    fn from_env(head_sha_override: Option<String>, token_env: Option<String>) -> Option<Self> {
        let nonempty = |v: String| Some(v).filter(|s| !s.is_empty());
        let token_var = token_env
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "GITHUB_TOKEN".to_string());
        let token = std::env::var(&token_var).ok().and_then(nonempty)?;
        let repo = std::env::var("GITHUB_REPOSITORY").ok().and_then(nonempty)?;
        let head_sha = head_sha_override
            .and_then(nonempty)
            .or_else(resolve_head_sha)?;
        let api_url = std::env::var("GITHUB_API_URL")
            .ok()
            .and_then(nonempty)
            .unwrap_or_else(|| "https://api.github.com".to_string());
        Some(Self {
            http: std::sync::OnceLock::new(),
            api_url,
            repo,
            token,
            head_sha,
            id: Mutex::new(None),
        })
    }

    fn http(&self) -> &reqwest::blocking::Client {
        self.http.get_or_init(reqwest::blocking::Client::new)
    }

    fn headers(&self) -> reqwest::header::HeaderMap {
        use reqwest::header::{
            ACCEPT, AUTHORIZATION, HeaderMap, HeaderName, HeaderValue, USER_AGENT,
        };
        let mut h = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", self.token)) {
            h.insert(AUTHORIZATION, v);
        }
        h.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        h.insert(USER_AGENT, HeaderValue::from_static("heph"));
        h.insert(
            HeaderName::from_static("x-github-api-version"),
            HeaderValue::from_static("2022-11-28"),
        );
        h
    }

    /// Create the check run on the first call, PATCH it after. `completed` finalizes
    /// it with a `success`/`failure` conclusion from `ok`.
    fn sync(&self, title: String, summary: String, completed: bool, ok: bool) {
        let mut id = self.id.lock().unwrap_or_else(|e| e.into_inner());
        let status = if completed {
            "completed"
        } else {
            "in_progress"
        };
        let conclusion = completed.then_some(if ok { "success" } else { "failure" });
        let output = serde_json::json!({ "title": title, "summary": summary });

        // Build the request body via a map (avoids `Value` index assignment).
        let mut body = serde_json::Map::new();
        body.insert("status".into(), serde_json::json!(status));
        body.insert("output".into(), output);
        if let Some(c) = conclusion {
            body.insert("conclusion".into(), serde_json::json!(c));
        }

        let result = match *id {
            None => {
                // Create: `name` + `head_sha` are required.
                body.insert("name".into(), serde_json::json!("heph build"));
                body.insert("head_sha".into(), serde_json::json!(self.head_sha));
                self.http()
                    .post(format!("{}/repos/{}/check-runs", self.api_url, self.repo))
                    .headers(self.headers())
                    .json(&serde_json::Value::Object(body))
                    .send()
                    .and_then(|r| r.error_for_status())
                    .and_then(|r| r.json::<serde_json::Value>())
                    .map(|v| {
                        if let Some(new_id) = v.get("id").and_then(serde_json::Value::as_u64) {
                            *id = Some(new_id);
                        }
                        // Surface the deep-link to the check's output in the job log.
                        if let Some(url) = v.get("html_url").and_then(serde_json::Value::as_str) {
                            tracing::info!("check run {url}");
                        }
                    })
            }
            Some(rid) => self
                .http()
                .patch(format!(
                    "{}/repos/{}/check-runs/{rid}",
                    self.api_url, self.repo
                ))
                .headers(self.headers())
                .json(&serde_json::Value::Object(body))
                .send()
                .and_then(|r| r.error_for_status())
                .map(drop),
        };
        if let Err(e) = result {
            tracing::warn!("check-run update failed: {e}");
        }
    }
}

struct Inner {
    tally: Mutex<Tally>,
    /// Title for the check run + the summary H2: `heph: <command>`.
    title: String,
    /// Final step-summary path; `None` disables the end-of-run file write.
    summary_path: Option<PathBuf>,
    /// Live check-run updater; `None` when not running under Actions (or no token).
    checks: Option<ChecksClient>,
    /// Set by `on_close` so the live-update thread exits.
    stop: AtomicBool,
}

impl Inner {
    /// (title, full markdown) for the current tally.
    fn render(&self) -> (String, String) {
        let tally = self.tally.lock().unwrap_or_else(|e| e.into_inner());
        let markdown = tally.render_markdown(now_unix_ms(), &self.title);
        (self.title.clone(), markdown)
    }

    fn ok(&self) -> bool {
        self.tally
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failed
            .is_empty()
    }

    /// Write the full markdown to the step-summary file once, at the end of the
    /// run. Atomic (temp + rename) so a reader never sees a half-written file.
    fn write_summary(&self) {
        let Some(path) = &self.summary_path else {
            return;
        };
        let (_, markdown) = self.render();
        let tmp = path.with_extension("heph-tmp");
        if std::fs::write(&tmp, markdown.as_bytes()).is_ok()
            && let Err(e) = std::fs::rename(&tmp, path)
        {
            tracing::warn!("failed to write step summary: {e}");
        }
    }
}

/// The GitHub Actions build-status hook.
pub struct GhaHook {
    inner: Arc<Inner>,
}

impl GhaHook {
    /// Build from the plugin's `options:` map. Options (all optional):
    /// `refreshSecs` (live check-run PATCH interval, default 30), `summaryPath`
    /// (final step-summary file, default `$GITHUB_STEP_SUMMARY`), `headSha` (check
    /// run head sha; default is auto-resolved by [`resolve_head_sha`] — the PR head
    /// on a `pull_request` event, else `$GITHUB_SHA`), `tokenEnv` (name of the env
    /// var holding the API token, default `GITHUB_TOKEN`; point at a GitHub App /
    /// PAT token so the check gets its own check suite instead of nesting under
    /// another workflow's). Spawns the live-update thread when a check run can be
    /// created.
    pub fn from_options(opts: &Options) -> anyhow::Result<Self> {
        deny_unknown(
            "gha hook",
            opts,
            &["refreshSecs", "summaryPath", "headSha", "tokenEnv"],
        )?;
        tracing::info!("gha hook loaded");
        let refresh_secs: u64 = decode_opt(opts, "gha hook", "refreshSecs")?
            .unwrap_or(30)
            .max(1);
        let summary_path = decode_opt::<String>(opts, "gha hook", "summaryPath")?
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("GITHUB_STEP_SUMMARY").map(PathBuf::from));
        if summary_path.is_none() {
            tracing::warn!(
                "gha hook: neither `summaryPath` option nor $GITHUB_STEP_SUMMARY set; \
                 no step summary will be written"
            );
        }
        let head_sha = decode_opt::<String>(opts, "gha hook", "headSha")?;
        let token_env = decode_opt::<String>(opts, "gha hook", "tokenEnv")?;
        let checks = ChecksClient::from_env(head_sha, token_env);
        if checks.is_none() {
            tracing::info!(
                "gha hook: GITHUB_TOKEN/GITHUB_REPOSITORY/GITHUB_SHA not all set; \
                 live check-run updates disabled (step summary still written at end)"
            );
        }

        // The plugin shares the heph process, so its args ARE the heph command:
        // skip the binary (argv[0]) and join the rest, e.g. `run //foo:bar`.
        let command = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
        let title = if command.is_empty() {
            "heph".to_string()
        } else {
            format!("heph: {command}")
        };

        let inner = Arc::new(Inner {
            tally: Mutex::new(Tally::default()),
            title,
            summary_path,
            checks,
            stop: AtomicBool::new(false),
        });

        // Live updates run only when a check run is configured. A plain thread (no
        // async runtime) keeps the hook free of runtime entanglement; it creates
        // the check run up front so it appears at job start, then PATCHes it every
        // `refreshSecs` until `on_close` sets `stop`.
        if inner.checks.is_some() {
            let t = Arc::clone(&inner);
            std::thread::spawn(move || {
                let (title, summary) = t.render();
                if let Some(c) = &t.checks {
                    c.sync(title, summary, false, true);
                }
                while !t.stop.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_secs(refresh_secs));
                    if t.stop.load(Ordering::Acquire) {
                        break;
                    }
                    let (title, summary) = t.render();
                    if let Some(c) = &t.checks {
                        c.sync(title, summary, false, true);
                    }
                }
            });
        }

        Ok(Self { inner })
    }
}

impl Hook for GhaHook {
    fn name(&self) -> String {
        "gha".to_string()
    }

    fn on_event(&self, ev: &BuildEvent) {
        self.inner
            .tally
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .apply(ev);
    }

    fn on_close(&self) {
        // Stop the live-update thread, finalize the check run, then write the step
        // summary once — all synchronously, so they complete before the plugin
        // acks the host (which is the host's drain barrier before process exit).
        self.inner.stop.store(true, Ordering::Release);
        if let Some(c) = &self.inner.checks {
            let (title, summary) = self.inner.render();
            c.sync(title, summary, true, self.inner.ok());
        }
        self.inner.write_summary();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(at: u64, kind: BuildEventKind) -> BuildEvent {
        BuildEvent {
            at_unix_ms: at,
            kind,
        }
    }

    fn scripted() -> Tally {
        let mut t = Tally::default();
        t.apply(&ev(
            0,
            BuildEventKind::Matched {
                addrs: vec!["//a:x".into(), "//a:y".into(), "//a:z".into()],
                complete: true,
            },
        ));
        // x: built
        t.apply(&ev(
            10,
            BuildEventKind::ExecuteStart {
                addr: "//a:x".into(),
                driver: "exec".into(),
                cache: true,
            },
        ));
        t.apply(&ev(
            20,
            BuildEventKind::ExecuteEnd {
                addr: "//a:x".into(),
                error: None,
            },
        ));
        t.apply(&ev(
            20,
            BuildEventKind::ResultEnd {
                addr: "//a:x".into(),
                error: None,
            },
        ));
        // y: cache hit, finished
        t.apply(&ev(
            15,
            BuildEventKind::LocalCacheHit {
                addr: "//a:y".into(),
            },
        ));
        t.apply(&ev(
            15,
            BuildEventKind::ResultEnd {
                addr: "//a:y".into(),
                error: None,
            },
        ));
        // z: started long ago, still running (slow), then a separate failure
        t.apply(&ev(
            0,
            BuildEventKind::ExecuteStart {
                addr: "//a:z".into(),
                driver: "exec".into(),
                cache: false,
            },
        ));
        t.apply(&ev(
            30,
            BuildEventKind::ResultEnd {
                addr: "//a:w".into(),
                error: Some("boom".into()),
            },
        ));
        t
    }

    #[test]
    fn markdown_reports_counts_slow_and_failures() {
        let t = scripted();
        // now far enough past //a:z's start to mark it slow.
        let md = t.render_markdown(SLOW_THRESHOLD_MS + 5_000, "heph: test");

        // The H2 is the heph command.
        assert!(md.contains("## heph: test"), "command heading: {md}");
        // 2 of 3 matched finished (x, y); w finished but isn't in the matched set.
        assert!(md.contains("2 / 3"), "progress: {md}");
        assert!(md.contains("**built:** 1"), "built: {md}");
        assert!(md.contains("**cached:** 1"), "cached: {md}");
        assert!(md.contains("**failed:** 1"), "failed: {md}");
        // //a:z is still running past the threshold.
        assert!(md.contains("### Slow targets"), "slow section: {md}");
        assert!(md.contains("//a:z"), "slow target listed: {md}");
        // failure surfaced with its message.
        assert!(md.contains("### Failed"), "failed section: {md}");
        assert!(
            md.contains("//a:w") && md.contains("boom"),
            "failure detail: {md}"
        );
    }

    #[test]
    fn slow_target_names_its_active_phase() {
        let mut t = Tally::default();
        // Stuck pulling from the remote cache (not executing).
        t.apply(&ev(
            0,
            BuildEventKind::RemoteCacheReadStart {
                addr: "//a:p".into(),
            },
        ));
        let md = t.render_markdown(SLOW_THRESHOLD_MS + 1, "heph: test");
        assert!(md.contains("//a:p"), "slow target listed: {md}");
        assert!(md.contains("cache pull"), "phase labelled: {md}");
        assert!(md.contains("| phase |"), "phase column header: {md}");

        // Its end clears it — no longer slow.
        t.apply(&ev(
            0,
            BuildEventKind::RemoteCacheReadEnd {
                addr: "//a:p".into(),
                error: None,
            },
        ));
        assert!(
            !t.render_markdown(SLOW_THRESHOLD_MS + 1, "heph: test")
                .contains("### Slow"),
            "cleared on phase end"
        );
    }

    #[test]
    fn slow_list_capped_to_top_n() {
        let mut t = Tally::default();
        // Many concurrent long-running targets; the slowest started earliest.
        for i in 0..(MAX_SLOW_ROWS + 5) {
            t.apply(&ev(
                i as u64,
                BuildEventKind::ExecuteStart {
                    addr: format!("//a:t{i}"),
                    driver: "exec".into(),
                    cache: false,
                },
            ));
        }
        let md = t.render_markdown(1_000_000, "heph: test");
        let rows = md.matches("| `//a:t").count();
        assert_eq!(rows, MAX_SLOW_ROWS, "slow rows capped: {rows}");
        assert!(md.contains("…and 5 more"), "overflow line: {md}");
    }

    #[test]
    fn provisional_total_until_matcher_complete() {
        let mut t = Tally::default();
        t.apply(&ev(
            0,
            BuildEventKind::Matched {
                addrs: vec!["//a:x".into()],
                complete: false,
            },
        ));
        assert!(
            t.render_markdown(0, "heph: test").contains("~1"),
            "provisional total"
        );
    }

    #[test]
    fn pr_head_sha_extracted_from_event_json() {
        // A trimmed `pull_request` event payload.
        let payload = serde_json::json!({
            "pull_request": { "head": { "sha": "deadbeefcafe" } }
        })
        .to_string();
        assert_eq!(
            pr_head_sha_from_json(payload.as_bytes()).as_deref(),
            Some("deadbeefcafe"),
        );
        // Missing field / wrong shape → None (so it falls back to GITHUB_SHA).
        assert_eq!(pr_head_sha_from_json(b"{}"), None);
        assert_eq!(pr_head_sha_from_json(b"not json"), None);
        let no_sha = serde_json::json!({ "pull_request": { "head": {} } }).to_string();
        assert_eq!(pr_head_sha_from_json(no_sha.as_bytes()), None);
    }

    #[test]
    fn on_close_writes_final_summary_to_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("summary.md");
        let opts: Options = [(
            "summaryPath".to_string(),
            serde_yaml::Value::String(path.to_string_lossy().into_owned()),
        )]
        .into_iter()
        .collect();
        let hook = GhaHook::from_options(&opts).expect("hook");
        hook.on_event(&ev(
            0,
            BuildEventKind::Matched {
                addrs: vec!["//a:x".into()],
                complete: true,
            },
        ));
        hook.on_event(&ev(
            5,
            BuildEventKind::ResultEnd {
                addr: "//a:x".into(),
                error: None,
            },
        ));
        hook.on_close();

        let written = std::fs::read_to_string(&path).expect("summary written");
        assert!(written.contains("1 / 1"), "final summary: {written}");
    }
}
