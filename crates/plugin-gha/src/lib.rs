//! A GitHub Actions hook: folds the engine's build-event stream into a status
//! [`Tally`] and surfaces it two ways. Published out-of-process as a cdylib (see
//! `plugin-gha-cdylib`) and enabled via a config `plugins:` entry.
//!
//! The design, and the reasoning behind it, is `docs/GHA_REPORTING.md`. The two
//! things to know before editing:
//!
//! - **The live comment and the final summary are different products**, with
//!   separate renderers ([`render::render_live`] / [`render::render_final`]). One
//!   renderer serving both is what made the summary ship a "slow targets" table
//!   that was structurally near-empty — it reported *currently-running* targets
//!   at the moment everything had finished.
//! - **A CI run has ~20k targets.** Nothing retained may be proportional to the
//!   graph, and nothing rendered may be a per-target list. See [`tally`].
//!
//! Surfaces:
//!
//! - **Live**, while the job runs: a **sticky PR comment** whose body is PATCHed on
//!   a timer. `$GITHUB_STEP_SUMMARY` is rendered by GitHub *only* when the step
//!   ends, so it can't show live progress; a comment can, works with the default
//!   `GITHUB_TOKEN`, and (unlike a check run) never nests under another workflow's
//!   check suite. One comment per job, reused across runs (found by a hidden marker)
//!   so it's never spammed; within it each heph command (each step) keeps its own
//!   section, so a job's earlier steps' results are preserved, not overwritten. The
//!   comment also records the workflow run id, so a *new* run's first step resets
//!   the body instead of stacking its sections on the previous build's.
//! - **At the end**: the final report is appended once to `$GITHUB_STEP_SUMMARY`.
//!
//! The aggregation is intentionally self-contained rather than reusing the TUI's
//! `BuildState`: that aggregator is coupled to ratatui and lives in the
//! (terminal) `tui` crate.

mod render;
mod tally;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hcore::events::{BuildEvent, now_unix_ms};
use hplugin::config::{Options, decode_opt, deny_unknown};
use hplugin::hook::Hook;

use crate::tally::Tally;

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

/// Longest command label rendered into a public comment.
const MAX_COMMAND_LABEL: usize = 120;

/// A safe label for the heph command being run, for the comment heading and the
/// section key.
///
/// **Never the raw argv.** The comment is public on the PR, and joining
/// `std::env::args()` publishes every flag — including any `--define`/`--env`
/// carrying a secret.
///
/// This is an **allowlist, not a blocklist**: only the subcommand and things
/// that look like target selectors (`//…`, `:…`, `…`) are kept. A blocklist that
/// tried to pair flags with their values would have to know which flags are
/// boolean — get that wrong in one direction and a selector is dropped, wrong in
/// the other and a secret is published. Recognising the two shapes worth showing
/// cannot leak a `--define` value, because a secret does not look like a target.
fn command_label() -> String {
    command_label_from(std::env::args().skip(1))
}

/// The filtering behind [`command_label`], over a supplied argument list so it
/// can be tested without depending on how the test harness was invoked.
fn command_label_from(args: impl Iterator<Item = String>) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut have_subcommand = false;
    for arg in args {
        if arg.starts_with('-') {
            continue;
        }
        let is_selector = arg.starts_with("//") || arg.starts_with(':') || arg == "...";
        if is_selector {
            parts.push(arg);
            continue;
        }
        // The first bare word is the subcommand (`run`, `query`, …). Later bare
        // words are flag values and are dropped.
        if !have_subcommand {
            have_subcommand = true;
            parts.push(arg);
        }
    }
    let joined = parts.join(" ");
    if joined.chars().count() <= MAX_COMMAND_LABEL {
        return joined;
    }
    joined
        .chars()
        .take(MAX_COMMAND_LABEL)
        .chain(std::iter::once('…'))
        .collect()
}

/// A link to the current workflow run, when the Actions env provides one.
fn run_url_from_env() -> Option<String> {
    let server = std::env::var("GITHUB_SERVER_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://github.com".to_string());
    let repo = std::env::var("GITHUB_REPOSITORY")
        .ok()
        .filter(|s| !s.is_empty())?;
    let run = std::env::var("GITHUB_RUN_ID")
        .ok()
        .filter(|s| !s.is_empty())?;
    Some(format!("{server}/{repo}/actions/runs/{run}"))
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

/// Hidden marker recording which workflow *run* last wrote the comment. The
/// comment is reused across runs (found by the container marker), but each new
/// run must start from a clean slate — otherwise the previous build's sections
/// pile up. Comparing this marker on adopt tells a new run to reset.
fn run_marker(run_id: &str) -> String {
    format!("<!-- heph-gha-run:{run_id} -->")
}

/// Extract the run id from a comment body's [`run_marker`], or `None` if absent
/// (e.g. a comment written before this marker existed). Pure (testable).
fn parse_run_id(body: &str) -> Option<String> {
    const OPEN: &str = "<!-- heph-gha-run:";
    let after = body.find(OPEN).and_then(|i| body.get(i + OPEN.len()..))?;
    let end = after.find(" -->")?;
    after.get(..end).map(str::to_string)
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

/// Serialize the comment body: the container marker (used to find the comment),
/// the run marker (used to detect a new run on adopt), then each section wrapped
/// in its hidden delimiters.
fn assemble_body(container_marker: &str, run_id: &str, sections: &[(String, String)]) -> String {
    let mut s = String::from(container_marker);
    s.push('\n');
    s.push_str(&run_marker(run_id));
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
    /// Identifies the current workflow run (run id + attempt). When an adopted
    /// comment carries a different run, its sections are from a prior build and
    /// are reset. Empty outside Actions (local runs keep reusing the comment).
    run_id: String,
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
        // Run id + attempt: a re-run of the same workflow gets a fresh attempt and
        // must reset too, so both are folded in.
        let run = std::env::var("GITHUB_RUN_ID").unwrap_or_default();
        let attempt = std::env::var("GITHUB_RUN_ATTEMPT").unwrap_or_default();
        let run_id = if run.is_empty() && attempt.is_empty() {
            String::new()
        } else {
            format!("{run}-{attempt}")
        };
        Some(Self {
            http: std::sync::OnceLock::new(),
            api_url,
            repo,
            token,
            pr,
            container_marker: format!("<!-- heph-gha:{job_key} -->"),
            run_id,
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
                // Same run → inherit the other steps' sections. Different (or
                // missing) run → the comment is from a prior build; start fresh so
                // its stale sections don't pile up.
                if parse_run_id(&body).as_deref() == Some(self.run_id.as_str()) {
                    st.sections = parse_sections(&body);
                }
            }
            st.loaded = true;
        }
        upsert_section(&mut st.sections, &self.section_key, &markdown);
        let body = assemble_body(&self.container_marker, &self.run_id, &st.sections);

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
    /// The summary H2 + comment heading: `heph <command>`.
    title: String,
    /// Final step-summary path; `None` disables the end-of-run file write.
    summary_path: Option<PathBuf>,
    /// Live sticky-comment updater; `None` when not running under Actions (or no
    /// token / not a PR).
    comment: Option<CommentClient>,
    /// Set by `on_close` so the live-update thread exits.
    stop: AtomicBool,
    /// Threshold for "running longest" rows and lock-wait notices.
    slow_after_ms: u64,
    /// Link to the workflow run, when the Actions env provides one.
    run_url: Option<String>,
}

impl Inner {
    fn ctx(&self) -> render::RenderCtx<'_> {
        render::RenderCtx {
            heading: &self.title,
            now_ms: now_unix_ms(),
            slow_after_ms: self.slow_after_ms,
            run_url: self.run_url.as_deref(),
        }
    }

    /// The live comment body for the current tally.
    ///
    /// Budgeted well under GitHub's 65,536-character comment cap: this is one
    /// section inside a body shared with every other heph step in the job, and
    /// `assemble_body` concatenates them all.
    fn render_live(&self) -> String {
        let tally = self.tally.lock().unwrap_or_else(|e| e.into_inner());
        render::render_live(&tally, &self.ctx(), render::LIVE_SECTION_BUDGET)
    }

    /// The end-of-run report for `$GITHUB_STEP_SUMMARY`.
    fn render_final(&self) -> String {
        let tally = self.tally.lock().unwrap_or_else(|e| e.into_inner());
        render::render_final(&tally, &self.ctx(), render::SUMMARY_BUDGET)
    }

    /// Append the final report to the step-summary file.
    ///
    /// **Append, not replace.** `$GITHUB_STEP_SUMMARY` is a per-step append
    /// target (the documented usage is `echo >> $GITHUB_STEP_SUMMARY`), so the
    /// previous write-then-rename destroyed anything an earlier command in the
    /// same step had written — including a user's own summary lines.
    ///
    /// Both failure paths are logged. The previous form,
    /// `if write().is_ok() && let Err(e) = rename()`, short-circuited on a failed
    /// *write*, so a full disk or a permissions error produced no summary and no
    /// warning either.
    fn write_summary(&self) {
        let Some(path) = &self.summary_path else {
            return;
        };
        let markdown = self.render_final();
        let opened = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path);
        match opened {
            Ok(mut f) => {
                use std::io::Write;
                if let Err(e) = f.write_all(markdown.as_bytes()) {
                    tracing::warn!(path = %path.display(), "writing step summary failed: {e}");
                }
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), "opening step summary failed: {e}");
            }
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
    /// of the env var holding the API token, default `GITHUB_TOKEN`),
    /// `slowAfterSecs` (how long a target must run before it is surfaced,
    /// default 30). Spawns the live-update thread when a PR comment can be
    /// created.
    pub fn from_options(opts: &Options) -> anyhow::Result<Self> {
        deny_unknown(
            "gha hook",
            opts,
            &["refreshSecs", "summaryPath", "tokenEnv", "slowAfterSecs"],
        )?;
        tracing::info!("gha hook loaded");
        let refresh_secs: u64 = decode_opt(opts, "gha hook", "refreshSecs")?
            .unwrap_or(30)
            .max(1);
        // 30s, not the previous hardcoded 10s: with a 30-second refresh, a 10s
        // threshold lists half of a cold build as "slow".
        let slow_after_secs: u64 = decode_opt(opts, "gha hook", "slowAfterSecs")?
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
        // The plugin shares the heph process, so its args ARE the heph command.
        let command = command_label();
        let title = if command.is_empty() {
            "heph".to_string()
        } else {
            format!("heph {command}")
        };
        let run_url = run_url_from_env();

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
            slow_after_ms: slow_after_secs.saturating_mul(1000),
            run_url,
        });

        // Live updates run only when a comment is configured. A plain thread (no
        // async runtime) keeps the hook free of runtime entanglement; it creates
        // the comment up front so it appears at job start, then PATCHes it every
        // `refreshSecs` until `on_close` sets `stop`.
        if inner.comment.is_some() {
            let t = Arc::clone(&inner);
            std::thread::spawn(move || {
                if let Some(c) = &t.comment {
                    c.sync(t.render_live());
                }
                while !t.stop.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_secs(refresh_secs));
                    if t.stop.load(Ordering::Acquire) {
                        break;
                    }
                    if let Some(c) = &t.comment {
                        c.sync(t.render_live());
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
        // Settle the status before the final render: the build is over.
        self.inner
            .tally
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_closed();
        if let Some(c) = &self.inner.comment {
            c.sync(self.inner.render_live());
        }
        self.inner.write_summary();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::events::BuildEventKind;

    fn ev(at: u64, kind: BuildEventKind) -> BuildEvent {
        BuildEvent {
            at_unix_ms: at,
            kind,
        }
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
            "run1",
            &[
                ("run //a:x".into(), "## heph: run //a:x\nbuilt 1".into()),
                ("run //b:y".into(), "## heph: run //b:y\nbuilt 2".into()),
            ],
        ));
        assert_eq!(sections.len(), 2);

        // A third step updates only its own section; the others stay.
        upsert_section(&mut sections, "run //a:x", "## heph: run //a:x\nbuilt 9");
        let body = assemble_body(container, "run1", &sections);
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
            parse_sections(&assemble_body(container, "run1", &sections)).len(),
            3
        );
    }

    #[test]
    fn run_marker_round_trips_and_signals_new_run() {
        let container = "<!-- heph-gha:test -->";
        // A first run wrote three steps' sections.
        let body = assemble_body(
            container,
            "10-1",
            &[
                ("run //a:x".into(), "x".into()),
                ("run //b:y".into(), "y".into()),
                ("run //c:z".into(), "z".into()),
            ],
        );
        assert_eq!(parse_run_id(&body).as_deref(), Some("10-1"));
        assert_eq!(parse_sections(&body).len(), 3);

        // The next build (different run id) detects the mismatch — its first step
        // resets the sections instead of stacking onto the prior three.
        let prev = parse_run_id(&body);
        let current = "11-1";
        let mut sections = if prev.as_deref() == Some(current) {
            parse_sections(&body)
        } else {
            Vec::new()
        };
        upsert_section(&mut sections, "run //a:x", "fresh");
        let new_body = assemble_body(container, current, &sections);
        let reparsed = parse_sections(&new_body);
        assert_eq!(reparsed.len(), 1, "stale sections cleared: {new_body}");
        assert_eq!(reparsed[0].0, "run //a:x");
        assert_eq!(parse_run_id(&new_body).as_deref(), Some(current));

        // A comment from before this marker existed has no run id → also resets.
        assert_eq!(parse_run_id("<!-- heph-gha:test -->\nlegacy"), None);
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

    fn hook_writing_to(path: &std::path::Path) -> GhaHook {
        let opts: Options = [(
            "summaryPath".to_string(),
            serde_yaml::Value::String(path.to_string_lossy().into_owned()),
        )]
        .into_iter()
        .collect();
        GhaHook::from_options(&opts).expect("hook")
    }

    #[test]
    fn on_close_writes_final_summary_to_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("summary.md");
        let hook = hook_writing_to(&path);
        hook.on_event(&ev(
            0,
            BuildEventKind::Matched {
                addrs: vec!["//a:x".into()],
                complete: true,
            },
        ));
        hook.on_event(&ev(
            0,
            BuildEventKind::LocalCacheHit {
                addr: "//a:x".into(),
            },
        ));
        hook.on_event(&ev(
            5,
            BuildEventKind::ResultEnd {
                addr: "//a:x".into(),
                error: None,
                upstream_of: None,
                exit_status: None,
                log_tail: None,
            },
        ));
        hook.on_close();

        let written = std::fs::read_to_string(&path).expect("summary written");
        assert!(written.contains("✅"), "final summary: {written}");
        assert!(
            written.contains("nothing executed"),
            "all-cached one-liner: {written}"
        );
    }

    #[test]
    fn step_summary_is_appended_not_clobbered() {
        // `$GITHUB_STEP_SUMMARY` is a per-step *append* target. The previous
        // write-then-rename destroyed whatever an earlier command in the same
        // step had written there.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("summary.md");
        std::fs::write(&path, "## Set up by an earlier step\n").expect("seed");

        let hook = hook_writing_to(&path);
        hook.on_event(&ev(
            0,
            BuildEventKind::Matched {
                addrs: vec!["//a:x".into()],
                complete: true,
            },
        ));
        hook.on_close();

        let written = std::fs::read_to_string(&path).expect("summary written");
        assert!(
            written.contains("Set up by an earlier step"),
            "pre-existing content preserved: {written}"
        );
        assert!(
            written.contains("heph"),
            "heph's report appended: {written}"
        );
    }

    #[test]
    fn a_failed_summary_write_is_logged_not_swallowed() {
        // The path is a directory, so opening it for append fails. The old code
        // short-circuited on a failed write and logged nothing at all; this must
        // at minimum not panic and not create a file.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a-directory");
        std::fs::create_dir(&path).expect("mkdir");

        let hook = hook_writing_to(&path);
        hook.on_close();

        assert!(path.is_dir(), "still a directory, nothing clobbered");
    }

    #[test]
    fn command_label_drops_flags_and_their_values() {
        // The comment is public on the PR: a `--define`/`--env` carrying a secret
        // must never reach it.
        assert_eq!(label_from(&["run", "//foo:bar"]), "run //foo:bar");
        assert_eq!(
            label_from(&["run", "--define", "TOKEN=hunter2", "//foo:bar"]),
            "run //foo:bar"
        );
        assert_eq!(
            label_from(&["run", "--env=SECRET=hunter2", "//foo:bar"]),
            "run //foo:bar"
        );
        // A boolean flag must not swallow the selector that follows it.
        assert_eq!(label_from(&["run", "--ff", "//a:b"]), "run //a:b");
        // A flag value that is a bare word is dropped, not mistaken for a target.
        assert_eq!(
            label_from(&["run", "--token", "hunter2", "//a:b"]),
            "run //a:b"
        );
        assert_eq!(label_from(&["query", "..."]), "query ...");
        // Over-long labels are capped rather than published whole.
        let long: Vec<String> = (0..100).map(|i| format!("//pkg{i}:target")).collect();
        let refs: Vec<&str> = long.iter().map(String::as_str).collect();
        let out = label_from(&refs);
        assert!(
            out.chars().count() <= MAX_COMMAND_LABEL + 1,
            "capped: {out}"
        );
    }

    fn label_from(args: &[&str]) -> String {
        command_label_from(args.iter().map(|s| (*s).to_string()))
    }
}
