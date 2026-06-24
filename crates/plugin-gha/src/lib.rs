//! A GitHub Actions hook: folds the engine's build-event stream into a status
//! tally (done/total, built, cached, failed, slow targets) and surfaces it two
//! ways. Published out-of-process as a cdylib (see `plugin-gha-cdylib`) and
//! enabled via a config `plugins:` entry.
//!
//! - **Live**, while the job runs: a **sticky PR comment** whose body is PATCHed on
//!   a timer. `$GITHUB_STEP_SUMMARY` is rendered by GitHub *only* when the step
//!   ends, so it can't show live progress; a comment can, works with the default
//!   `GITHUB_TOKEN`, and (unlike a check run) never nests under another workflow's
//!   check suite. One comment per job, reused across runs (found by a hidden marker)
//!   so it's never spammed; within it each heph command (each step) keeps its own
//!   section, so a job's earlier steps' results are preserved, not overwritten.
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

    /// A status emoji for the CLI invocation, derived from the tally: ❌ if anything
    /// failed, ✅ once every matched target is done, otherwise ⏳ (still running).
    fn status_emoji(&self) -> &'static str {
        let (done, total) = self.progress();
        if !self.failed.is_empty() {
            "❌"
        } else if self.matched_complete && done == total {
            "✅"
        } else {
            "⏳"
        }
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
        // Heading leads with the invocation status emoji.
        out.push_str(&format!("## {} {heading}\n\n", self.status_emoji()));
        out.push_str(&format!(
            "**Targets:** {done} / {total_str} &nbsp;•&nbsp; **built:** {} &nbsp;•&nbsp; **cached:** {} &nbsp;•&nbsp; **failed:** {}\n",
            self.built,
            self.cached_count(),
            self.failed.len(),
        ));

        let slow = self.slow(now_ms);
        if !slow.is_empty() {
            // Collapsible: slow targets are noise most of the time, expanded on demand.
            // The blank line after </summary> is required for the table to render.
            out.push_str(&format!(
                "\n<details><summary>🐢 Slow targets ({})</summary>\n\n| target | phase | running for |\n| --- | --- | --- |\n",
                slow.len(),
            ));
            for (addr, phase, elapsed) in slow.iter().take(MAX_SLOW_ROWS) {
                out.push_str(&format!("| `{addr}` | {phase} | {}s |\n", elapsed / 1000));
            }
            if slow.len() > MAX_SLOW_ROWS {
                out.push_str(&format!("\n…and {} more\n", slow.len() - MAX_SLOW_ROWS));
            }
            out.push_str("</details>\n");
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

/// Scopes the sticky comment to one per job: the Actions job id (`GITHUB_JOB`) when
/// present, else the heph command (so local / non-Actions runs still get a stable
/// key). A job keeps a single comment across all its steps.
fn comment_key(command: &str, job: Option<String>) -> String {
    match job.filter(|s| !s.is_empty()) {
        Some(job) => job,
        None if command.is_empty() => "heph".to_string(),
        None => command.to_string(),
    }
}

/// Shared GitHub REST auth/version headers.
fn gh_headers(token: &str) -> reqwest::header::HeaderMap {
    use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderName, HeaderValue, USER_AGENT};
    let mut h = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}")) {
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

/// The PR number for the current event, or `None` outside a PR. Prefers the event
/// payload's `pull_request.number`, falling back to `GITHUB_REF`
/// (`refs/pull/<N>/merge`).
fn pr_number() -> Option<u64> {
    if let Some(path) = std::env::var("GITHUB_EVENT_PATH")
        .ok()
        .filter(|s| !s.is_empty())
        && let Ok(bytes) = std::fs::read(path)
        && let Some(n) = pr_number_from_json(&bytes)
    {
        return Some(n);
    }
    pr_number_from_ref(&std::env::var("GITHUB_REF").unwrap_or_default())
}

/// Extract `pull_request.number` from raw event-payload JSON. Pure (testable).
fn pr_number_from_json(bytes: &[u8]) -> Option<u64> {
    let json: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    json.get("pull_request")?.get("number")?.as_u64()
}

/// Parse the PR number out of a `refs/pull/<N>/merge` (or `/head`) ref. Pure.
fn pr_number_from_ref(git_ref: &str) -> Option<u64> {
    git_ref
        .strip_prefix("refs/pull/")?
        .split('/')
        .next()?
        .parse()
        .ok()
}

/// Hidden delimiters wrapping one heph command's section inside the shared
/// per-job comment, so each step (a separate heph process) owns its own block and
/// updates only that — earlier steps' sections are preserved.
fn section_open(key: &str) -> String {
    format!("<!-- heph-gha-step:{key} -->")
}
fn section_close(key: &str) -> String {
    format!("<!-- /heph-gha-step:{key} -->")
}

/// Parse the ordered `(key, content)` sections out of a comment body. Tolerant:
/// anything outside a well-formed open/close pair is ignored.
fn parse_sections(body: &str) -> Vec<(String, String)> {
    const OPEN: &str = "<!-- heph-gha-step:";
    let mut out = Vec::new();
    let mut rest = body;
    // `.get(..)` (not `s[..]` slicing) throughout to satisfy the string-slice lint
    // and stay panic-free on any malformed body.
    while let Some(i) = rest.find(OPEN) {
        let Some(after) = rest.get(i + OPEN.len()..) else {
            break;
        };
        let Some(j) = after.find(" -->") else { break };
        let (Some(key), Some(content_start)) = (after.get(..j), after.get(j + " -->".len()..))
        else {
            break;
        };
        let close = section_close(key);
        let Some(k) = content_start.find(&close) else {
            break;
        };
        let (Some(content), Some(next)) =
            (content_start.get(..k), content_start.get(k + close.len()..))
        else {
            break;
        };
        out.push((key.to_string(), content.trim_matches('\n').to_string()));
        rest = next;
    }
    out
}

/// Replace the section named `key` in place, or append it if new (preserving the
/// order of the others).
fn upsert_section(sections: &mut Vec<(String, String)>, key: &str, content: &str) {
    if let Some(slot) = sections.iter_mut().find(|(k, _)| k == key) {
        slot.1 = content.to_string();
    } else {
        sections.push((key.to_string(), content.to_string()));
    }
}

/// Serialize the comment body: the container marker (used to find the comment)
/// followed by each section wrapped in its hidden delimiters.
fn assemble_body(container_marker: &str, sections: &[(String, String)]) -> String {
    let mut s = String::from(container_marker);
    s.push('\n');
    for (key, content) in sections {
        s.push_str(&format!(
            "{}\n{content}\n{}\n\n",
            section_open(key),
            section_close(key)
        ));
    }
    s.trim_end().to_string()
}

/// The found-or-created comment state, kept across the process's timer ticks so the
/// other steps' sections (loaded once) are preserved on every update.
#[derive(Default)]
struct CommentState {
    /// Whether the existing comment (if any) has been fetched & adopted.
    loaded: bool,
    /// The comment id once found or created.
    id: Option<u64>,
    /// All sections currently in the comment, including this process's.
    sections: Vec<(String, String)>,
}

/// Live updates to a **sticky PR comment**. Unlike a check run, an issue comment is
/// never grouped under a workflow's check suite, so it works with the default
/// `GITHUB_TOKEN` (needs `pull-requests: write`). One comment per job (found-or-
/// created via the hidden `container_marker`, so it's never spammed); within it,
/// each heph command owns a `section_key` block, so a job's many steps each keep
/// their own results instead of overwriting one another.
struct CommentClient {
    http: std::sync::OnceLock<reqwest::blocking::Client>,
    api_url: String,
    repo: String,
    token: String,
    /// The PR to comment on.
    pr: u64,
    /// Hidden marker (`<!-- heph-gha:<job> -->`) identifying *this job's* comment.
    container_marker: String,
    /// This process's section key (the heph command) within that comment.
    section_key: String,
    state: Mutex<CommentState>,
}

impl CommentClient {
    /// Build from the Actions env, or `None` outside a PR / without a token.
    /// `job_key` scopes the comment (one per job); `section_key` scopes this
    /// process's block within it. `token_env` names the token var (default
    /// `GITHUB_TOKEN`).
    fn from_env(job_key: &str, section_key: &str, token_env: Option<String>) -> Option<Self> {
        let nonempty = |v: String| Some(v).filter(|s| !s.is_empty());
        let token_var = token_env
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "GITHUB_TOKEN".to_string());
        let token = std::env::var(&token_var).ok().and_then(nonempty)?;
        let repo = std::env::var("GITHUB_REPOSITORY").ok().and_then(nonempty)?;
        let pr = pr_number()?;
        let api_url = std::env::var("GITHUB_API_URL")
            .ok()
            .and_then(nonempty)
            .unwrap_or_else(|| "https://api.github.com".to_string());
        Some(Self {
            http: std::sync::OnceLock::new(),
            api_url,
            repo,
            token,
            pr,
            container_marker: format!("<!-- heph-gha:{job_key} -->"),
            section_key: section_key.to_string(),
            state: Mutex::new(CommentState::default()),
        })
    }

    fn http(&self) -> &reqwest::blocking::Client {
        self.http.get_or_init(reqwest::blocking::Client::new)
    }

    /// Find this job's comment (by `container_marker`), returning its id + body.
    /// Pages through the PR's comments, capped to bound work.
    fn fetch_existing(&self) -> Option<(u64, String)> {
        const MAX_PAGES: u32 = 10;
        for page in 1..=MAX_PAGES {
            let resp = self
                .http()
                .get(format!(
                    "{}/repos/{}/issues/{}/comments?per_page=100&page={page}",
                    self.api_url, self.repo, self.pr
                ))
                .headers(gh_headers(&self.token))
                .send()
                .and_then(|r| r.error_for_status())
                .and_then(|r| r.json::<Vec<serde_json::Value>>());
            let comments = match resp {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("listing PR comments failed: {e}");
                    return None;
                }
            };
            if comments.is_empty() {
                break;
            }
            for c in &comments {
                let body = c
                    .get("body")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if body.contains(&self.container_marker)
                    && let Some(cid) = c.get("id").and_then(serde_json::Value::as_u64)
                {
                    return Some((cid, body.to_string()));
                }
            }
            if comments.len() < 100 {
                break;
            }
        }
        None
    }

    /// Upsert this process's section with `markdown` and write the merged comment.
    /// On the first call it adopts any existing comment for this job (inheriting the
    /// other steps' sections); afterwards it edits only its own block.
    fn sync(&self, markdown: String) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !st.loaded {
            if let Some((cid, body)) = self.fetch_existing() {
                st.id = Some(cid);
                st.sections = parse_sections(&body);
            }
            st.loaded = true;
        }
        upsert_section(&mut st.sections, &self.section_key, &markdown);
        let body = assemble_body(&self.container_marker, &st.sections);

        let mut payload = serde_json::Map::new();
        payload.insert("body".into(), serde_json::json!(body));

        let result = match st.id {
            Some(cid) => self
                .http()
                .patch(format!(
                    "{}/repos/{}/issues/comments/{cid}",
                    self.api_url, self.repo
                ))
                .headers(gh_headers(&self.token))
                .json(&serde_json::Value::Object(payload))
                .send()
                .and_then(|r| r.error_for_status())
                .map(drop),
            None => self
                .http()
                .post(format!(
                    "{}/repos/{}/issues/{}/comments",
                    self.api_url, self.repo, self.pr
                ))
                .headers(gh_headers(&self.token))
                .json(&serde_json::Value::Object(payload))
                .send()
                .and_then(|r| r.error_for_status())
                .and_then(|r| r.json::<serde_json::Value>())
                .map(|v| {
                    if let Some(new_id) = v.get("id").and_then(serde_json::Value::as_u64) {
                        st.id = Some(new_id);
                    }
                    if let Some(url) = v.get("html_url").and_then(serde_json::Value::as_str) {
                        tracing::info!("status comment {url}");
                    }
                }),
        };
        if let Err(e) = result {
            tracing::warn!("status-comment update failed: {e}");
        }
    }
}

struct Inner {
    tally: Mutex<Tally>,
    /// The summary H2 + comment heading: `heph: <command>`.
    title: String,
    /// Final step-summary path; `None` disables the end-of-run file write.
    summary_path: Option<PathBuf>,
    /// Live sticky-comment updater; `None` when not running under Actions (or no
    /// token / not a PR).
    comment: Option<CommentClient>,
    /// Set by `on_close` so the live-update thread exits.
    stop: AtomicBool,
}

impl Inner {
    /// The full status markdown for the current tally.
    fn render_markdown(&self) -> String {
        let tally = self.tally.lock().unwrap_or_else(|e| e.into_inner());
        tally.render_markdown(now_unix_ms(), &self.title)
    }

    /// Write the full markdown to the step-summary file once, at the end of the
    /// run. Atomic (temp + rename) so a reader never sees a half-written file.
    fn write_summary(&self) {
        let Some(path) = &self.summary_path else {
            return;
        };
        let markdown = self.render_markdown();
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
    /// `refreshSecs` (live PR-comment PATCH interval, default 30), `summaryPath`
    /// (final step-summary file, default `$GITHUB_STEP_SUMMARY`), `tokenEnv` (name
    /// of the env var holding the API token, default `GITHUB_TOKEN`). Spawns the
    /// live-update thread when a PR comment can be created.
    pub fn from_options(opts: &Options) -> anyhow::Result<Self> {
        deny_unknown(
            "gha hook",
            opts,
            &["refreshSecs", "summaryPath", "tokenEnv"],
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
        // The plugin shares the heph process, so its args ARE the heph command:
        // skip the binary (argv[0]) and join the rest, e.g. `run //foo:bar`.
        let command = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
        let title = if command.is_empty() {
            "heph".to_string()
        } else {
            format!("heph: {command}")
        };

        let token_env = decode_opt::<String>(opts, "gha hook", "tokenEnv")?;
        // One sticky PR comment per job (keyed by GITHUB_JOB, command as fallback);
        // within it, one section per heph command so a job's steps each keep their
        // own results.
        let job_key = comment_key(&command, std::env::var("GITHUB_JOB").ok());
        let section_key = if command.is_empty() {
            "heph".to_string()
        } else {
            command.clone()
        };
        let comment = CommentClient::from_env(&job_key, &section_key, token_env);
        if comment.is_none() {
            tracing::info!(
                "gha hook: GITHUB_TOKEN/GITHUB_REPOSITORY/PR not all set; \
                 live status comment disabled (step summary still written at end)"
            );
        }

        let inner = Arc::new(Inner {
            tally: Mutex::new(Tally::default()),
            title,
            summary_path,
            comment,
            stop: AtomicBool::new(false),
        });

        // Live updates run only when a comment is configured. A plain thread (no
        // async runtime) keeps the hook free of runtime entanglement; it creates
        // the comment up front so it appears at job start, then PATCHes it every
        // `refreshSecs` until `on_close` sets `stop`.
        if inner.comment.is_some() {
            let t = Arc::clone(&inner);
            std::thread::spawn(move || {
                if let Some(c) = &t.comment {
                    c.sync(t.render_markdown());
                }
                while !t.stop.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_secs(refresh_secs));
                    if t.stop.load(Ordering::Acquire) {
                        break;
                    }
                    if let Some(c) = &t.comment {
                        c.sync(t.render_markdown());
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
        // Stop the live-update thread, write the final comment, then the step
        // summary once — all synchronously, so they complete before the plugin
        // acks the host (which is the host's drain barrier before process exit).
        self.inner.stop.store(true, Ordering::Release);
        if let Some(c) = &self.inner.comment {
            c.sync(self.inner.render_markdown());
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

        // The H2 is the heph command, led by a status emoji (❌ — there's a failure).
        assert!(md.contains("## ❌ heph: test"), "command heading: {md}");
        // 2 of 3 matched finished (x, y); w finished but isn't in the matched set.
        assert!(md.contains("2 / 3"), "progress: {md}");
        assert!(md.contains("**built:** 1"), "built: {md}");
        assert!(md.contains("**cached:** 1"), "cached: {md}");
        assert!(md.contains("**failed:** 1"), "failed: {md}");
        // //a:z is still running past the threshold — in the collapsible.
        assert!(
            md.contains("<details><summary>🐢 Slow targets (1)</summary>"),
            "slow collapsible: {md}"
        );
        assert!(md.contains("</details>"), "collapsible closed: {md}");
        assert!(md.contains("//a:z"), "slow target listed: {md}");
        // failure surfaced with its message.
        assert!(md.contains("### Failed"), "failed section: {md}");
        assert!(
            md.contains("//a:w") && md.contains("boom"),
            "failure detail: {md}"
        );
    }

    #[test]
    fn status_emoji_tracks_invocation_outcome() {
        let mut t = Tally::default();
        t.apply(&ev(
            0,
            BuildEventKind::Matched {
                addrs: vec!["//a:x".into()],
                complete: true,
            },
        ));
        // Matched but nothing finished → running.
        assert_eq!(t.status_emoji(), "⏳");
        assert!(
            t.render_markdown(0, "heph: test")
                .contains("## ⏳ heph: test")
        );

        // All matched finished, no failure → success.
        t.apply(&ev(
            1,
            BuildEventKind::ResultEnd {
                addr: "//a:x".into(),
                error: None,
            },
        ));
        assert_eq!(t.status_emoji(), "✅");

        // A failure flips it to failed regardless of progress.
        t.apply(&ev(
            2,
            BuildEventKind::ResultEnd {
                addr: "//a:x".into(),
                error: Some("boom".into()),
            },
        ));
        assert_eq!(t.status_emoji(), "❌");
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
    fn comment_key_prefers_job_then_command() {
        // Job id wins → one comment per job.
        assert_eq!(comment_key("run //a:x", Some("test".into())), "test");
        // No job → command keeps it stable (local / non-Actions).
        assert_eq!(comment_key("run //a:x", None), "run //a:x");
        // Empty job string treated as absent.
        assert_eq!(
            comment_key("query //...", Some(String::new())),
            "query //..."
        );
        // Empty command → stable fallback.
        assert_eq!(comment_key("", None), "heph");
    }

    #[test]
    fn comment_sections_preserve_other_steps() {
        // Two steps wrote their sections into one job comment.
        let container = "<!-- heph-gha:test -->";
        let mut sections = parse_sections(&assemble_body(
            container,
            &[
                ("run //a:x".into(), "## heph: run //a:x\nbuilt 1".into()),
                ("run //b:y".into(), "## heph: run //b:y\nbuilt 2".into()),
            ],
        ));
        assert_eq!(sections.len(), 2);

        // A third step updates only its own section; the others stay.
        upsert_section(&mut sections, "run //a:x", "## heph: run //a:x\nbuilt 9");
        let body = assemble_body(container, &sections);
        assert!(body.starts_with(container), "container marker kept: {body}");
        assert!(body.contains("built 9"), "own section updated: {body}");
        assert!(body.contains("built 2"), "other step preserved: {body}");
        // Round-trips back to the same three sections, in order.
        let reparsed = parse_sections(&body);
        assert_eq!(reparsed.len(), 2);
        assert_eq!(reparsed[0].0, "run //a:x");
        assert_eq!(reparsed[1].0, "run //b:y");

        // A brand-new command appends a section rather than clobbering.
        upsert_section(&mut sections, "query //...", "## heph: query //...\nok");
        assert_eq!(
            parse_sections(&assemble_body(container, &sections)).len(),
            3
        );
    }

    #[test]
    fn pr_number_extracted_from_event_and_ref() {
        let payload = serde_json::json!({ "pull_request": { "number": 122 } }).to_string();
        assert_eq!(pr_number_from_json(payload.as_bytes()), Some(122));
        assert_eq!(pr_number_from_json(b"{}"), None);
        // Ref fallback.
        assert_eq!(pr_number_from_ref("refs/pull/122/merge"), Some(122));
        assert_eq!(pr_number_from_ref("refs/pull/7/head"), Some(7));
        assert_eq!(pr_number_from_ref("refs/heads/main"), None);
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
