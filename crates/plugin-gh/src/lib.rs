//! The `gh` (GitHub) plugin. Published out-of-process as a cdylib (see
//! `plugin-gh-cdylib`, which only wires these parts) and enabled via a config
//! `plugins:` entry. Its parts:
//!
//! - **The Actions reporting hook** ([`GhaHook`], below): build status as a
//!   sticky PR comment and a step summary. Its options sit under the plugin's
//!   `hook:` key. They are always decoded, so a typo fails even while the hook
//!   is off; the hook is constructed only when `hook.enable` is true.
//! - **`heph.gh.release_asset(...)`** ([`release`]): a BUILD function declaring
//!   an `http_fetch` of a release asset, and optionally an `archive.extract`
//!   of it (the builtin `archive` plugin's driver, named rather than linked:
//!   see `docs/ARCHIVE.md`). With `credentials` (a private repository) it
//!   fetches through the API: an `http_fetch` of the release JSON, an
//!   `asset_url` picking the asset's URL out of it, and an `http_fetch` of
//!   that URL. See `docs/GH_RELEASES.md`.
//! - **The `asset_url` driver** ([`asset_url`]): one asset's API URL from a
//!   release JSON.
//!
//! The plugin's `options:` are one [`GhOptions`]: the hook's, under `hook:`.
//! `release_asset` reads no option; its server is an argument of each call.
//! A bad option never aborts the host: a [`ConfigErrorHook`] logs it.
//!
//! # The Actions reporting hook
//!
//! Folds the engine's build-event stream into a status [`Tally`] and surfaces
//! it two ways.
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
//!   section, so a job's earlier steps' results are preserved, not overwritten.
//!   The comment has one block for the PR's own CI and one for its merge-queue
//!   run (`merge_group`, whose PR is read from the `gh-readonly-queue` ref), in
//!   that order, each under a heading naming what it tested and the run that
//!   wrote it. Each block records that run, so a *new* run's first step resets
//!   only its own block instead of stacking its sections on the previous
//!   build's — a merge-queue re-run keeps the PR's CI block, and a PR re-run
//!   keeps the merge-queue block. The two can run at once, so every write
//!   re-reads the comment first (a conditional `GET`, free when nothing
//!   changed) and carries the other block as it is then; only a write by the
//!   other run landing within that one round trip can be overwritten.
//! - **At the end**: the final report is appended once to `$GITHUB_STEP_SUMMARY`.
//!
//! The aggregation is intentionally self-contained rather than reusing the TUI's
//! `BuildState`: that aggregator is coupled to ratatui and lives in the
//! (terminal) `tui` crate.

pub mod asset_url;
#[cfg(test)]
mod comment_tests;
pub mod release;
mod render;
mod report;
mod tally;

// The `htspec` derive macros expand to `crate::htvalue` / `crate::htspec`.
pub(crate) use hcore::htvalue;
pub(crate) use hplugin::htspec;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use hcore::events::{BuildEvent, now_unix_ms};
use hplugin::config::Options;
use hplugin::hook::Hook;
use serde::Deserialize;

use crate::tally::Tally;

/// The plugin's `options:`: the hook's, under `hook:`. An unknown key is
/// refused at either level, so a typo (or a hook option at the top level) is
/// caught. Nothing here reaches `release_asset`, whose server is an argument
/// of each call: one workspace may pull from github.com and from a GitHub
/// Enterprise server.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GhOptions {
    /// Absent, or a bare `hook:`, is the defaults: no hook.
    #[serde(default)]
    pub hook: HookOptions,
}

impl GhOptions {
    /// Decode the plugin's whole `options:` map. Reads nothing but the map.
    pub fn decode(opts: &Options) -> anyhow::Result<Self> {
        let value = serde_yaml::to_value(opts).context("gh: reading `options:`")?;
        serde_path_to_error::deserialize(value).context("gh: invalid `options:`")
    }
}

/// Scopes the sticky comment to one per job **leg**.
///
/// `GITHUB_JOB` alone is not enough: it is the job id *from the workflow file*,
/// so every leg of a matrix reports the same value. With that as the sole key,
/// all legs race `fetch_existing`, all find nothing, and all POST — producing
/// duplicate comments — after which each caches the section list once and never
/// re-reads, so the legs permanently erase each other's sections. This repo's own
/// `test` job has three legs.
///
/// `leg` disambiguates them. It is the runner's OS/arch (`RUNNER_OS`,
/// `RUNNER_ARCH`) rather than the matrix context, because the matrix is not
/// exposed to a process as an env var while the runner identity always is, and
/// the overwhelmingly common matrix axis *is* the platform. A matrix that varies
/// something else (a Go version, a feature flag) still collides — hence the
/// `commentKey` option, which overrides this wholesale.
fn comment_key(command: &str, job: Option<String>, leg: Option<String>) -> String {
    let base = match job.filter(|s| !s.is_empty()) {
        Some(job) => job,
        None if command.is_empty() => "heph".to_string(),
        None => command.to_string(),
    };
    match leg.filter(|s| !s.is_empty()) {
        Some(leg) => format!("{base}:{leg}"),
        None => base,
    }
}

/// The runner's identity, used to separate matrix legs. `None` outside Actions.
fn runner_leg() -> Option<String> {
    let os = std::env::var("RUNNER_OS").ok().filter(|s| !s.is_empty());
    let arch = std::env::var("RUNNER_ARCH").ok().filter(|s| !s.is_empty());
    match (os, arch) {
        (Some(os), Some(arch)) => Some(format!("{os}-{arch}")),
        (Some(v), None) | (None, Some(v)) => Some(v),
        (None, None) => None,
    }
}

/// A stable, delimiter-safe identifier for a section key.
///
/// The key goes into an HTML comment (`<!-- heph-gha-step:KEY -->`), so it must
/// not contain the delimiter or a newline. It used to be the raw command: a
/// command containing `" -->"` terminated the opening marker early, so
/// `upsert_section` never matched that section again and every step **appended a
/// new one** — growing the body until it hit GitHub's cap and 422'd.
///
/// Hashing rather than escaping, because the key is a hidden marker nobody reads;
/// it only has to be stable and unambiguous.
fn section_id(key: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    format!("{:016x}", h.finish())
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
/// This is an **allowlist, not a blocklist**. Kept:
///
/// - target selectors (`//…`, `:…`, `…/...`, `...`), wherever they appear;
/// - the `-e`/`--expr` query expression, which is a selector too;
/// - a bare word (the subcommand path, `run`'s label) — but only when the
///   argument before it is not a flag written without `=value`. The plugin
///   cannot tell a boolean flag from one taking a value, so a word after
///   `--flag` may be that flag's value and is dropped. That errs towards
///   dropping a label (`heph run --force build //...` shows `run //...`), never
///   towards publishing a `--token hunter2`.
///
/// Everything after `--` is an entrypoint's own arguments and is never shown.
fn command_label() -> String {
    command_label_from(std::env::args().skip(1))
}

/// A target selector: always safe to show, since a secret does not look like
/// a target.
fn is_selector(arg: &str) -> bool {
    arg.starts_with("//") || arg.starts_with(':') || arg.ends_with("...")
}

/// A subcommand name or a label: an identifier, short.
fn is_word(arg: &str) -> bool {
    const MAX_WORD: usize = 64;
    let mut chars = arg.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        && arg.len() <= MAX_WORD
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// A query expression as a code span. Backticks and angle brackets are
/// dropped: the first could close the span, the second open an HTML comment
/// inside a body whose sections are delimited by HTML comments.
fn expr_part(expr: &str) -> String {
    let clean: String = expr
        .chars()
        .filter(|c| !matches!(c, '`' | '<' | '>') && !c.is_control())
        .collect();
    format!("-e `{}`", clean.trim())
}

/// The filtering behind [`command_label`], over a supplied argument list so it
/// can be tested without depending on how the test harness was invoked.
fn command_label_from(args: impl Iterator<Item = String>) -> String {
    let mut parts: Vec<String> = Vec::new();
    // The previous argument was a flag with no `=value`: this one may be its
    // value, so it is shown only if it is a selector.
    let mut after_flag = false;
    // The previous argument was `-e`/`--expr`: this one is the expression.
    let mut expr_next = false;
    for arg in args {
        if arg == "--" {
            break;
        }
        if std::mem::take(&mut expr_next) {
            parts.push(expr_part(&arg));
            continue;
        }
        if arg == "-e" || arg == "--expr" {
            expr_next = true;
            continue;
        }
        if let Some(expr) = arg.strip_prefix("--expr=") {
            parts.push(expr_part(expr));
            after_flag = false;
            continue;
        }
        if arg.starts_with('-') {
            after_flag = !arg.contains('=');
            continue;
        }
        let maybe_flag_value = std::mem::take(&mut after_flag);
        if is_selector(&arg) || (!maybe_flag_value && is_word(&arg)) {
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

/// Which kind of workflow run this process belongs to.
///
/// A PR's comment keeps one block per kind, each with its own run marker, so a
/// re-run of the PR's CI never wipes what the merge queue reported and a re-run
/// of the merge queue never wipes the PR's CI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunKind {
    /// A run for the PR itself (`pull_request`, `pull_request_target`, …).
    PullRequest,
    /// A merge-queue run (`merge_group`) for the group this PR heads.
    MergeQueue,
}

impl RunKind {
    /// Display order in the comment: the PR's own CI, then the merge queue.
    const ALL: [Self; 2] = [Self::PullRequest, Self::MergeQueue];

    /// The id in the hidden group delimiters.
    fn marker_id(self) -> &'static str {
        match self {
            Self::PullRequest => "pr",
            Self::MergeQueue => "merge-queue",
        }
    }

    /// The visible heading of the block.
    fn heading(self) -> &'static str {
        match self {
            Self::PullRequest => "Pull request CI",
            Self::MergeQueue => "Merge queue",
        }
    }
}

/// What a block's run tested, shown in its heading: the merge-queue group's base
/// branch and commit, or the PR's head commit. Either part may be empty.
///
/// Carried in the block's hidden [`tested_marker`], never read back from the
/// visible heading, because every write regenerates the heading from what the
/// block carries — including the other kind's block, which this process did
/// not write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Tested {
    /// The branch the group merges into; empty for a PR run.
    base: String,
    /// A short commit sha: the merge group's head commit (what the queue run
    /// checks out and tests), or the PR's head.
    sha: String,
}

/// Length of the short sha kept in [`Tested`]. GitHub links a bare sha of 7 or
/// more hex characters to its commit.
const SHORT_SHA: usize = 12;

impl Tested {
    /// The short form of `sha` when it is a hex commit id, else empty: the value
    /// lands in a hidden marker and in a public heading, so anything that is
    /// not plainly a sha is dropped.
    fn short_sha(sha: &str) -> String {
        if sha.len() < 7 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return String::new();
        }
        sha.chars().take(SHORT_SHA).collect()
    }

    /// `base` when it is a plain branch name, else empty. A git ref may hold
    /// `<`, `>` and `` ` ``, which could close the hidden marker or the code
    /// span it is shown in; `@` separates it from the sha in the marker.
    fn clean_base(base: &str) -> String {
        const MAX_BASE: usize = 100;
        let plain = !base.is_empty()
            && base.len() <= MAX_BASE
            && base
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+'));
        if plain {
            base.to_string()
        } else {
            String::new()
        }
    }

    fn is_empty(&self) -> bool {
        self.base.is_empty() && self.sha.is_empty()
    }
}

/// The PR this process reports on, the kind of run it is, and what it tested.
#[derive(Debug, PartialEq, Eq)]
struct PrTarget {
    pr: u64,
    kind: RunKind,
    tested: Tested,
}

/// The PR the current event reports on; `None` outside a PR or a merge queue.
fn pr_target() -> Option<PrTarget> {
    let payload = std::env::var("GITHUB_EVENT_PATH")
        .ok()
        .filter(|s| !s.is_empty())
        .and_then(|path| std::fs::read(path).ok());
    let env = |name| std::env::var(name).unwrap_or_default();
    pr_target_from(
        payload.as_deref(),
        &env("GITHUB_REF"),
        &env("GITHUB_EVENT_NAME"),
        &env("GITHUB_SHA"),
    )
}

/// [`pr_target`] over a raw event payload, `GITHUB_REF`, `GITHUB_EVENT_NAME`
/// and `GITHUB_SHA`. Pure (testable).
///
/// A merge-queue run is one whose event is `merge_group` (the payload has a
/// `merge_group`, or the event name says so). Only then is its queue ref
/// trusted — the payload's `merge_group.head_ref`, else `GITHUB_REF` — to name
/// the PR the group was created for: a `push` to a `gh-readonly-queue/*` branch
/// is neither a merge-queue run nor a PR run. What it tested is the payload's
/// `merge_group.head_sha` (else `GITHUB_SHA`, the same commit on a
/// `merge_group` event) on `merge_group.base_ref` (else the queue ref's base).
/// Otherwise the payload's `pull_request.number`, falling back to a
/// `refs/pull/<N>/merge` ref.
fn pr_target_from(
    payload: Option<&[u8]>,
    git_ref: &str,
    event_name: &str,
    github_sha: &str,
) -> Option<PrTarget> {
    let json = payload.and_then(|b| serde_json::from_slice::<serde_json::Value>(b).ok());
    let merge_group = json.as_ref().and_then(|j| j.get("merge_group"));
    if merge_group.is_some() || event_name == "merge_group" {
        let field = |name: &str| merge_group.and_then(|g| g.get(name)?.as_str());
        let q = field("head_ref")
            .and_then(parse_queue_ref)
            .or_else(|| parse_queue_ref(git_ref))?;
        let base = field("base_ref")
            .map(|b| b.strip_prefix("refs/heads/").unwrap_or(b))
            .unwrap_or(q.base);
        return Some(PrTarget {
            pr: q.pr,
            kind: RunKind::MergeQueue,
            tested: Tested {
                base: Tested::clean_base(base),
                sha: Tested::short_sha(field("head_sha").unwrap_or(github_sha)),
            },
        });
    }
    let pull_request = json.as_ref().and_then(|j| j.get("pull_request"));
    let pr = pull_request
        .and_then(|p| p.get("number")?.as_u64())
        .or_else(|| pr_number_from_ref(git_ref))?;
    let head_sha = pull_request
        .and_then(|p| p.get("head")?.get("sha")?.as_str())
        .map(Tested::short_sha)
        .unwrap_or_default();
    Some(PrTarget {
        pr,
        kind: RunKind::PullRequest,
        tested: Tested {
            base: String::new(),
            sha: head_sha,
        },
    })
}

/// A merge-queue ref, split.
#[derive(Debug)]
struct QueueRef<'a> {
    base: &'a str,
    pr: u64,
}

/// Parse a merge-queue ref, `refs/heads/gh-readonly-queue/<base>/pr-<N>-<sha>`.
/// The base branch may itself contain slashes, so the PR is read from the last
/// segment. The trailing sha is not the commit the group tests, and is not
/// read. Pure.
fn parse_queue_ref(git_ref: &str) -> Option<QueueRef<'_>> {
    let rest = git_ref
        .strip_prefix("refs/heads/")
        .unwrap_or(git_ref)
        .strip_prefix("gh-readonly-queue/")?;
    let (base, leaf) = rest.rsplit_once('/')?;
    let pr_sha = leaf.strip_prefix("pr-")?;
    let digits = pr_sha.split_once('-').map_or(pr_sha, |(d, _)| d);
    Some(QueueRef {
        base,
        pr: parse_pr_number(digits)?,
    })
}

/// A PR number: ASCII digits only. `u64::from_str` also takes a leading `+`.
fn parse_pr_number(digits: &str) -> Option<u64> {
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Parse the PR number out of a `refs/pull/<N>/merge` (or `/head`) ref. Pure.
fn pr_number_from_ref(git_ref: &str) -> Option<u64> {
    parse_pr_number(git_ref.strip_prefix("refs/pull/")?.split('/').next()?)
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

/// Hidden marker carrying a block's [`Tested`], so the heading can be
/// regenerated from the block alone. `@` cannot occur in either part.
fn tested_marker(tested: &Tested) -> String {
    format!("<!-- heph-gha-tested:{}@{} -->", tested.base, tested.sha)
}

/// Every section of every block, in body order. Test shorthand.
#[cfg(test)]
fn parse_sections(body: &str) -> Vec<(String, String)> {
    let groups = parse_groups(body);
    RunKind::ALL
        .into_iter()
        .filter_map(|k| groups.get(k))
        .flat_map(|g| g.sections.iter().cloned())
        .collect()
}

/// The run id of the first block. Test shorthand.
#[cfg(test)]
fn parse_run_id(body: &str) -> Option<String> {
    let groups = parse_groups(body);
    RunKind::ALL
        .into_iter()
        .find_map(|k| groups.get(k))
        .map(|g| g.run_id.clone())
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

/// Hidden delimiters wrapping one [`RunKind`]'s block of the comment.
fn group_open(kind: RunKind) -> String {
    format!("<!-- heph-gha-group:{} -->", kind.marker_id())
}
fn group_close(kind: RunKind) -> String {
    format!("<!-- /heph-gha-group:{} -->", kind.marker_id())
}

/// One [`RunKind`]'s block: the run that last wrote it, what that run tested,
/// and its sections.
#[derive(Debug, PartialEq, Eq)]
struct Group {
    run_id: String,
    tested: Tested,
    sections: Vec<(String, String)>,
}

impl Group {
    fn empty() -> Self {
        Self {
            run_id: String::new(),
            tested: Tested::default(),
            sections: Vec::new(),
        }
    }
}

/// Every block of the comment, by kind.
#[derive(Debug, Default, PartialEq, Eq)]
struct Groups {
    pull_request: Option<Group>,
    merge_queue: Option<Group>,
}

impl Groups {
    fn get(&self, kind: RunKind) -> Option<&Group> {
        match kind {
            RunKind::PullRequest => self.pull_request.as_ref(),
            RunKind::MergeQueue => self.merge_queue.as_ref(),
        }
    }

    fn slot(&mut self, kind: RunKind) -> &mut Option<Group> {
        match kind {
            RunKind::PullRequest => &mut self.pull_request,
            RunKind::MergeQueue => &mut self.merge_queue,
        }
    }
}

/// A run id read back from a body: `<run>-<attempt>`, both ASCII digits, else
/// empty — an unknown run, which a run of the same kind resets. The id is
/// echoed into the public heading, and the body is editable by anyone with
/// write access to the PR.
fn clean_run_id(run: &str) -> String {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    match run.split_once('-') {
        Some((r, a)) if digits(r) && digits(a) => run.to_string(),
        _ => String::new(),
    }
}

/// One hidden marker of the body, as [`parse_groups`] reads it.
enum Marker<'a> {
    GroupOpen(RunKind),
    GroupClose(RunKind),
    Run(&'a str),
    Tested(&'a str),
    /// A section's open tag, by key.
    Step(&'a str),
    /// Any other comment (the container marker, a section's close tag outside
    /// its section).
    Other,
}

impl<'a> Marker<'a> {
    /// Classify the inside of `<!-- … -->`.
    fn classify(inner: &'a str) -> Self {
        let kind = |id: &str| RunKind::ALL.into_iter().find(|k| k.marker_id() == id);
        if let Some(k) = inner.strip_prefix("heph-gha-group:").and_then(kind) {
            Self::GroupOpen(k)
        } else if let Some(k) = inner.strip_prefix("/heph-gha-group:").and_then(kind) {
            Self::GroupClose(k)
        } else if let Some(run) = inner.strip_prefix("heph-gha-run:") {
            Self::Run(run)
        } else if let Some(tested) = inner.strip_prefix("heph-gha-tested:") {
            Self::Tested(tested)
        } else if let Some(key) = inner.strip_prefix("heph-gha-step:") {
            Self::Step(key)
        } else {
            Self::Other
        }
    }
}

/// Parse the blocks out of a comment body, in one pass. Tolerant: a malformed
/// body yields what parsed before the damage, never a panic.
///
/// Group and run markers are recognised **only at the top level**. A section is
/// skipped as a unit, from its open tag to its own close tag (which carries the
/// section's hashed key, so it is unambiguous), because its content embeds raw
/// log tails: a target printing `<!-- /heph-gha-group:pr -->` must not move a
/// block's boundary. A section with no close tag ends its block, and reading
/// resumes at the next open of the other kind's block, if any.
///
/// The limit: a section whose content holds its **own** close tag ends there,
/// and the rest of its content is read as top level. The tag carries a hash of
/// the command, so a target would have to print exactly that; log tails are
/// not escaped.
///
/// A group open with no close ends at the next top-level group open, or at the
/// end of the body. When a kind's block appears twice, the **first** wins. A
/// run marker that is not `<run>-<attempt>` reads as no run.
///
/// A body with no group delimiters predates them: it is one run's sections under
/// a single run marker, and only PR runs ever wrote one (a merge-queue run found
/// no PR number), so it is read as the PR block.
fn parse_groups(body: &str) -> Groups {
    const OPEN: &str = "<!-- ";
    const CLOSE: &str = " -->";
    let mut groups = Groups::default();
    // Top-level markers before any group delimiter: a legacy body.
    let mut legacy = Group::empty();
    let mut delimited = false;
    // The block being read.
    let mut current: Option<(RunKind, Group)> = None;
    // Close the block being read; a kind already read keeps its first block.
    fn finish(groups: &mut Groups, current: &mut Option<(RunKind, Group)>) {
        if let Some((kind, g)) = current.take() {
            groups.slot(kind).get_or_insert(g);
        }
    }
    let mut rest = body;
    // `.get(..)` (not `s[..]` slicing) throughout to satisfy the string-slice lint
    // and stay panic-free on any malformed body.
    while let Some(i) = rest.find(OPEN) {
        let Some(after) = rest.get(i + OPEN.len()..) else {
            break;
        };
        let Some(j) = after.find(CLOSE) else { break };
        let (Some(inner), Some(next)) = (after.get(..j), after.get(j + CLOSE.len()..)) else {
            break;
        };
        rest = next;
        let target = match current.as_mut() {
            Some((_, g)) => g,
            None => &mut legacy,
        };
        match Marker::classify(inner) {
            Marker::GroupOpen(kind) => {
                finish(&mut groups, &mut current);
                delimited = true;
                current = Some((kind, Group::empty()));
            }
            Marker::GroupClose(kind) => {
                if current.as_ref().is_some_and(|(k, _)| *k == kind) {
                    finish(&mut groups, &mut current);
                }
            }
            Marker::Run(run) => {
                if target.run_id.is_empty() {
                    target.run_id = clean_run_id(run);
                }
            }
            Marker::Tested(tested) => {
                let (base, sha) = tested.rsplit_once('@').unwrap_or(("", tested));
                target.tested = Tested {
                    base: Tested::clean_base(base),
                    sha: Tested::short_sha(sha),
                };
            }
            Marker::Step(key) => {
                let close = section_close(key);
                let section = rest
                    .find(&close)
                    .and_then(|k| Some((rest.get(..k)?, rest.get(k + close.len()..)?)));
                let Some((content, next)) = section else {
                    // Unclosed: where its content ends is unknown. It ends its
                    // block; a later block of another kind is still read.
                    let Some((kind, _)) = &current else { break };
                    let other = group_open(match kind {
                        RunKind::PullRequest => RunKind::MergeQueue,
                        RunKind::MergeQueue => RunKind::PullRequest,
                    });
                    finish(&mut groups, &mut current);
                    let Some(resume) = rest.find(&other).and_then(|k| rest.get(k..)) else {
                        break;
                    };
                    rest = resume;
                    continue;
                };
                target
                    .sections
                    .push((key.to_string(), content.trim_matches('\n').to_string()));
                rest = next;
            }
            Marker::Other => {}
        }
    }
    finish(&mut groups, &mut current);
    if !delimited && !legacy.sections.is_empty() {
        groups.pull_request = Some(legacy);
    }
    groups
}

/// The visible heading of a block: what its run tested, and a link to the run
/// (and attempt) that wrote it.
///
/// `run_id` is `<run>-<attempt>` ([`CommentClient::from_env`]); `repo_url` is
/// `<server>/<owner>/<repo>`, empty for no link. The sha is bare so GitHub
/// links it to its commit; the base branch is a code span.
fn group_heading(kind: RunKind, g: &Group, repo_url: &str) -> String {
    let mut h = format!("# {}", kind.heading());
    if !g.tested.base.is_empty() {
        h.push_str(&format!(" · `{}`", g.tested.base));
    }
    if !g.tested.sha.is_empty() {
        h.push_str(&format!(" · {}", g.tested.sha));
    }
    if let Some((run, attempt)) = g.run_id.rsplit_once('-')
        && !repo_url.is_empty()
        && !run.is_empty()
        && !attempt.is_empty()
    {
        h.push_str(&format!(
            " · [run {run}, attempt {attempt}]({repo_url}/actions/runs/{run}/attempts/{attempt})"
        ));
    }
    h
}

/// Serialize the comment body: the container marker (used to find the comment),
/// then each kind's block in [`RunKind::ALL`] order — its delimiters, its run
/// marker (used to detect a new run of that kind on adopt), its tested marker
/// (when it has one), its heading, and each section wrapped in its hidden
/// delimiters.
fn assemble_body(container_marker: &str, repo_url: &str, groups: &Groups) -> String {
    let mut s = String::from(container_marker);
    s.push('\n');
    for kind in RunKind::ALL {
        let Some(g) = groups.get(kind) else { continue };
        s.push_str(&group_open(kind));
        s.push('\n');
        s.push_str(&run_marker(&g.run_id));
        s.push('\n');
        if !g.tested.is_empty() {
            s.push_str(&tested_marker(&g.tested));
            s.push('\n');
        }
        s.push_str(&group_heading(kind, g, repo_url));
        s.push_str("\n\n");
        for (key, content) in &g.sections {
            s.push_str(&format!(
                "{}\n{content}\n{}\n\n",
                section_open(key),
                section_close(key)
            ));
        }
        s.push_str(&group_close(kind));
        s.push_str("\n\n");
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
    /// Every block currently in the comment, including this process's.
    groups: Groups,
    /// The `ETag` of our last response for the comment — the write's, or the
    /// last re-read that returned a body. A re-read sends it as
    /// `If-None-Match`, so an unchanged comment costs a `304`, which GitHub
    /// does not count against the primary rate limit. `None` → a plain `GET`.
    etag: Option<String>,
}

/// Live updates to a **sticky PR comment**. Unlike a check run, an issue comment is
/// never grouped under a workflow's check suite, so it works with the default
/// `GITHUB_TOKEN` (needs `pull-requests: write`). One comment per job (found-or-
/// created via the hidden `container_marker`, so it's never spammed); within it,
/// each heph command owns a `section_key` block, so a job's many steps each keep
/// their own results instead of overwriting one another.
struct CommentClient {
    http: std::sync::OnceLock<reqwest::blocking::Client>,
    cfg: CommentConfig,
    /// Hidden marker (`<!-- heph-gha:<job> -->`) identifying *this job's* comment.
    container_marker: String,
    state: Mutex<CommentState>,
}

/// Everything a [`CommentClient`] needs, with no ambient environment.
///
/// Split out from [`CommentClient::from_env`] so the client is constructible in a
/// test. It previously read `std::env` directly, which meant nothing could build
/// one without mutating global process state — so the entire HTTP layer, where
/// this feature actually lives, had zero tests. Worse, the branch that ships
/// (comment enabled) was the one never exercised: the suite only stayed quiet
/// because `GITHUB_TOKEN` happens to be absent from the test job's env, an
/// undeclared ambient invariant that would have posted comments onto the PR
/// under test the day someone added it.
#[derive(Debug, Clone)]
struct CommentConfig {
    api_url: String,
    repo: String,
    token: String,
    /// The PR to comment on.
    pr: u64,
    /// Whether this is the PR's own CI or its merge-queue run: picks the block
    /// of the comment this process owns.
    kind: RunKind,
    /// What this run tests, for its block's heading.
    tested: Tested,
    /// `<server>/<owner>/<repo>`, to link each block's run from its heading;
    /// empty for no link.
    repo_url: String,
    /// Scopes the comment — one per job leg.
    job_key: String,
    /// Identifies the current workflow run (run id + attempt). When an adopted
    /// comment's block for [`Self::kind`] carries a different run, its sections
    /// are from a prior build and are reset; the other kind's block is kept.
    /// Empty outside Actions (local runs keep reusing the comment).
    run_id: String,
    /// This process's section key within that comment.
    section_key: String,
    /// Per-request timeout. Explicit rather than reqwest's 30s default, because
    /// `on_close` runs inside the host's drain barrier before process exit. A
    /// sync with a deadline lowers it to what is left of the deadline.
    timeout: Duration,
}

/// Budget for the close-time sync, counted from when it holds the comment
/// lock; the write gets at least [`WRITE_FLOOR`] on top.
///
/// `fetch_existing` can walk several pages, and the host awaits `Hook::drain`
/// before exiting — so without a ceiling a slow or hanging GitHub API adds
/// minutes to `heph`'s exit. The comment is best-effort; the step summary is the
/// durable artifact, and it is written whether or not this succeeds.
const CLOSE_SYNC_BUDGET: Duration = Duration::from_secs(10);

/// The least a write is given, however much of the sync's budget the reads
/// used: the write carries the closed render, and skipping it leaves the
/// comment showing a finished build as running.
const WRITE_FLOOR: Duration = Duration::from_secs(2);

/// Per-request timeout for live updates.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

impl CommentClient {
    fn new(cfg: CommentConfig) -> Self {
        Self {
            http: std::sync::OnceLock::new(),
            container_marker: format!("<!-- heph-gha:{} -->", cfg.job_key),
            cfg,
            state: Mutex::new(CommentState::default()),
        }
    }

    /// Build from the Actions env, or `None` outside a PR / without a token.
    /// `job_key` scopes the comment; `section_key` scopes this process's block
    /// within it. `token_env` names the token var (default `GITHUB_TOKEN`).
    fn from_env(job_key: &str, section_key: &str, token_env: Option<String>) -> Option<Self> {
        let nonempty = |v: String| Some(v).filter(|s| !s.is_empty());
        let token_var = token_env
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "GITHUB_TOKEN".to_string());
        let token = std::env::var(&token_var).ok().and_then(nonempty)?;
        let repo = std::env::var("GITHUB_REPOSITORY").ok().and_then(nonempty)?;
        let PrTarget { pr, kind, tested } = pr_target()?;
        let server = std::env::var("GITHUB_SERVER_URL")
            .ok()
            .and_then(nonempty)
            .unwrap_or_else(|| "https://github.com".to_string());
        let repo_url = format!("{}/{repo}", server.trim_end_matches('/'));
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
        Some(Self::new(CommentConfig {
            api_url,
            repo,
            token,
            pr,
            kind,
            tested,
            repo_url,
            job_key: job_key.to_string(),
            run_id,
            section_key: section_key.to_string(),
            timeout: REQUEST_TIMEOUT,
        }))
    }

    fn http(&self) -> &reqwest::blocking::Client {
        self.http.get_or_init(|| {
            reqwest::blocking::Client::builder()
                .timeout(self.cfg.timeout)
                .build()
                .unwrap_or_default()
        })
    }

    /// The timeout for the next request: the per-request timeout, lowered to
    /// what is left of `deadline`. `None` once the deadline has passed.
    ///
    /// Checking the deadline before a request is not enough: the request then
    /// runs for the whole per-request timeout, so a close-time sync of a re-read
    /// and a write could take twice the budget.
    fn request_timeout(&self, deadline: Option<Instant>) -> Option<Duration> {
        let Some(d) = deadline else {
            return Some(self.cfg.timeout);
        };
        let left = d.checked_duration_since(Instant::now())?;
        (!left.is_zero()).then(|| left.min(self.cfg.timeout))
    }

    /// The URL of comment `cid`.
    fn comment_url(&self, cid: u64) -> String {
        format!(
            "{}/repos/{}/issues/comments/{cid}",
            self.cfg.api_url, self.cfg.repo
        )
    }

    /// Find this job's comment (by `container_marker`), returning its id + body.
    /// Pages through the PR's comments, capped to bound work.
    fn fetch_existing(&self, deadline: Option<Instant>) -> Option<(u64, String)> {
        const MAX_PAGES: u32 = 10;
        for page in 1..=MAX_PAGES {
            // The host awaits `Hook::drain` before process exit, so an unbounded
            // page walk here adds its latency straight onto `heph`'s exit.
            let Some(timeout) = self.request_timeout(deadline) else {
                tracing::warn!("listing PR comments hit the deadline; giving up");
                return None;
            };
            let resp = self
                .http()
                .get(format!(
                    "{}/repos/{}/issues/{}/comments?per_page=100&page={page}",
                    self.cfg.api_url, self.cfg.repo, self.cfg.pr
                ))
                .headers(gh_headers(&self.cfg.token))
                .timeout(timeout)
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

    /// Bring the other kinds' blocks in `st` up to date before a write.
    ///
    /// A conditional `GET` of the comment by id, with `If-None-Match` set to
    /// the `ETag` of our last response for it:
    ///
    /// - `304`: nobody wrote since — the blocks in memory are current;
    /// - `200`: take every *other* kind's block from the body (never our own,
    ///   which only this process writes) and keep its `ETag`;
    /// - anything else (an error, a timeout, a deleted comment): keep what we
    ///   have and warn; the write still goes ahead.
    ///
    /// The re-read gets at most half of what is left of `deadline`, so a
    /// hanging `GET` leaves the write its turn.
    fn refresh_others(&self, st: &mut CommentState, cid: u64, deadline: Option<Instant>) {
        let Some(timeout) = self.request_timeout(deadline) else {
            tracing::warn!("no time left to re-read the status comment; writing what we have");
            return;
        };
        let timeout = if deadline.is_some() {
            timeout / 2
        } else {
            timeout
        };
        let mut req = self
            .http()
            .get(self.comment_url(cid))
            .headers(gh_headers(&self.cfg.token))
            .timeout(timeout);
        if let Some(etag) = &st.etag {
            req = req.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        let resp = match req.send().and_then(|r| r.error_for_status()) {
            Ok(r) if r.status() == reqwest::StatusCode::NOT_MODIFIED => return,
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("re-reading the status comment failed: {e}");
                return;
            }
        };
        let etag = response_etag(&resp);
        let body = match resp.json::<serde_json::Value>() {
            Ok(c) => c
                .get("body")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            Err(e) => {
                tracing::warn!("re-reading the status comment failed: {e}");
                return;
            }
        };
        let Some(body) = body else {
            tracing::warn!("re-read status comment has no body; writing what we have");
            return;
        };
        let mut fresh = parse_groups(&body);
        let mut unchanged = true;
        for other in RunKind::ALL.into_iter().filter(|k| *k != self.cfg.kind) {
            let theirs = fresh.slot(other).take();
            unchanged &= *st.groups.slot(other) == theirs;
            *st.groups.slot(other) = theirs;
        }
        // The 304 saving rests on our write's `ETag` matching what a `GET` of
        // the same body returns, which only a real run can show.
        if unchanged && st.etag.is_some() {
            tracing::debug!(
                sent = st.etag.as_deref(),
                got = etag.as_deref(),
                "conditional re-read of the status comment returned 200 with the other \
                 blocks unchanged; the write's ETag likely does not match the GET's"
            );
        }
        st.etag = etag;
    }

    /// Upsert this process's section and write the merged comment.
    ///
    /// `render` is called **under the comment lock**, not before it. That is the
    /// fix for a silent race: the live thread used to render (taking and
    /// releasing the tally lock) and only then take the comment lock, so if
    /// `on_close` landed in that window, the already-rendered "⏳ still running"
    /// body would PATCH over the final one — leaving a finished build showing as
    /// in-progress, permanently, with no error anywhere. Rendering inside the
    /// lock makes render-and-write atomic with respect to other syncs.
    ///
    /// On the first call it adopts any existing comment for this job (inheriting
    /// the other steps' sections). Every later call re-reads the comment first
    /// ([`Self::refresh_others`]), so each write carries the other kind's block
    /// as it was one round trip earlier, not as it was at adopt. The PR's CI and
    /// its merge-queue run can overlap (a PR re-run while the PR is queued);
    /// without the re-read, each live tick of one would write back a stale copy
    /// of the other's block, over the other's final report.
    ///
    /// What this does **not** guarantee: GitHub has no compare-and-swap on a
    /// comment, so a write by the other run landing between our re-read and our
    /// write is overwritten. The window is one round trip per write.
    ///
    /// `budget` bounds the reads, starting **once the lock is held**: a close
    /// that waited for a live tick's hung request must not find its budget
    /// spent and skip the write, leaving the comment on its "⏳ running" render
    /// for good. Each request's timeout is lowered to what is left of it, but
    /// the write is never skipped: it gets at least [`WRITE_FLOOR`] (capped by
    /// the per-request timeout), so a call takes at most `budget` plus that.
    fn sync(&self, render: impl FnOnce() -> String, budget: Option<Duration>) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = budget.map(|b| Instant::now() + b);
        let markdown = render();
        let kind = self.cfg.kind;
        if !st.loaded {
            if let Some((cid, body)) = self.fetch_existing(deadline) {
                st.id = Some(cid);
                // Every block is kept. Our kind's: same run → inherit the other
                // steps' sections; different (or missing) run → it is from a
                // prior build, so start fresh and its stale sections don't pile
                // up. The other kind's block is never ours to reset.
                let mut groups = parse_groups(&body);
                let own = groups.slot(kind);
                if own.as_ref().is_some_and(|g| g.run_id != self.cfg.run_id) {
                    *own = None;
                }
                st.groups = groups;
            }
            st.loaded = true;
        } else if let Some(cid) = st.id {
            self.refresh_others(&mut st, cid, deadline);
        }
        let own = st.groups.slot(kind).get_or_insert_with(Group::empty);
        own.run_id.clone_from(&self.cfg.run_id);
        own.tested.clone_from(&self.cfg.tested);
        upsert_section(&mut own.sections, &self.cfg.section_key, &markdown);
        let body = assemble_body(&self.container_marker, &self.cfg.repo_url, &st.groups);

        let timeout = self
            .request_timeout(deadline)
            .unwrap_or_default()
            .max(WRITE_FLOOR.min(self.cfg.timeout));
        let payload = serde_json::json!({ "body": body });
        let result = match st.id {
            Some(cid) => self
                .http()
                .patch(self.comment_url(cid))
                .headers(gh_headers(&self.cfg.token))
                .timeout(timeout)
                .json(&payload)
                .send()
                .and_then(|r| r.error_for_status())
                .map(|r| st.etag = response_etag(&r)),
            None => self
                .http()
                .post(format!(
                    "{}/repos/{}/issues/{}/comments",
                    self.cfg.api_url, self.cfg.repo, self.cfg.pr
                ))
                .headers(gh_headers(&self.cfg.token))
                .timeout(timeout)
                .json(&payload)
                .send()
                .and_then(|r| r.error_for_status())
                .and_then(|r| {
                    let etag = response_etag(&r);
                    r.json::<serde_json::Value>().map(|v| (etag, v))
                })
                .map(|(etag, v)| {
                    if let Some(new_id) = v.get("id").and_then(serde_json::Value::as_u64) {
                        st.id = Some(new_id);
                        st.etag = etag;
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

/// A response's `ETag`, if it has a usable one.
fn response_etag(resp: &reqwest::blocking::Response) -> Option<String> {
    resp.headers()
        .get(reqwest::header::ETAG)?
        .to_str()
        .ok()
        .map(str::to_string)
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
    /// Set by the first event. Nothing is posted before it: a command that runs
    /// no build (`heph tool …`) emits no event and never closes the stream, so
    /// a comment posted at startup would read "⏳" forever.
    started: AtomicBool,
    /// Live PATCH interval.
    refresh: Duration,
    /// Threshold for "running longest" rows and lock-wait notices.
    slow_after_ms: u64,
    /// Link to the workflow run, when the Actions env provides one.
    run_url: Option<String>,
    /// Where to write the full JSON document, if configured.
    json_path: Option<PathBuf>,
    /// Whether to emit `::error::` workflow commands.
    annotations: bool,
    /// Root addrs already annotated, so a repeated `ResultEnd` for one addr does
    /// not repeat its annotation.
    annotated: Mutex<std::collections::HashSet<String>>,
    /// Last out-of-band flush, for the floor between failure-triggered syncs.
    last_flush: Mutex<Option<Instant>>,
}

/// Minimum gap between out-of-band, failure-triggered comment syncs.
///
/// The first root failure flushes immediately — that is the whole point of
/// diagnosing before the build ends, and waiting up to `refreshSecs` for it
/// wastes the window. Subsequent roots coalesce into the next tick if they land
/// inside this floor, so a build breaking in twenty places does not become twenty
/// API writes. Bounded by *breakage count*, not target count, which is what makes
/// it safe on a 20k-target graph.
const FLUSH_FLOOR: Duration = Duration::from_secs(10);

/// Emit a workflow command on stdout.
///
/// `println!` is restricted in plugin code, and deliberately so — but a workflow
/// command has exactly one channel: the step's stdout, read by the Actions
/// runner. There is no file or API alternative. It is gated on `GITHUB_ACTIONS`
/// so it can only ever fire inside a runner, where stderr is not a tty and the
/// TUI has therefore taken its non-interactive backend, leaving stdout free.
fn emit_workflow_command(line: &str) {
    if std::env::var("GITHUB_ACTIONS").ok().as_deref() != Some("true") {
        return;
    }
    println!("{line}");
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

    /// React to newly-seen root failures: annotate them, and flush the comment
    /// out of band so the failure is visible now rather than up to `refreshSecs`
    /// later.
    ///
    /// Only *roots* trigger any of this. Collateral failures bump a counter — at
    /// 20k targets one broken leaf blocks thousands, and flushing or annotating
    /// each would be thousands of writes describing one bug.
    fn notice_failures(&self) {
        let new_roots: Vec<crate::tally::RootFailure> = {
            let t = self.tally.lock().unwrap_or_else(|e| e.into_inner());
            let mut seen = self.annotated.lock().unwrap_or_else(|e| e.into_inner());
            t.roots()
                .iter()
                .filter(|r| seen.insert(r.addr.clone()))
                .cloned()
                .collect()
        };
        if new_roots.is_empty() {
            return;
        }

        if self.annotations {
            for line in report::annotations(&new_roots, new_roots.len()) {
                emit_workflow_command(&line);
            }
        }

        // Flush immediately on the first failure; coalesce later ones.
        let should_flush = {
            let mut last = self.last_flush.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            match *last {
                Some(prev) if now.duration_since(prev) < FLUSH_FLOOR => false,
                _ => {
                    *last = Some(now);
                    true
                }
            }
        };
        if should_flush {
            self.sync_comment(None);
        }
    }

    /// Write the machine-readable end-of-run surfaces.
    ///
    /// Both are best-effort and independent of the comment: an agent reading
    /// `GITHUB_OUTPUT` must not depend on whether a PR comment was warranted.
    fn write_machine_surfaces(&self) {
        let (elapsed, counters) = {
            let t = self.tally.lock().unwrap_or_else(|e| e.into_inner());
            (t.elapsed_ms(now_unix_ms()), t.counters())
        };
        let json_path_str = self.json_path.as_ref().map(|p| p.display().to_string());

        if let Some(path) = &self.json_path {
            let t = self.tally.lock().unwrap_or_else(|e| e.into_inner());
            let doc = report::full_document(
                &t,
                &counters,
                elapsed,
                &self.title,
                json_path_str.as_deref(),
            );
            match serde_json::to_vec_pretty(&doc) {
                Ok(bytes) => {
                    if let Err(e) = std::fs::write(path, bytes) {
                        tracing::warn!(path = %path.display(), "writing heph json failed: {e}");
                    }
                }
                Err(e) => tracing::warn!("serializing heph json failed: {e}"),
            }
        }

        // `$GITHUB_OUTPUT` is an append target, one `key=value` per line.
        if let Some(out) = std::env::var_os("GITHUB_OUTPUT") {
            let t = self.tally.lock().unwrap_or_else(|e| e.into_inner());
            let lines = report::github_outputs(&t, &counters, elapsed, json_path_str.as_deref());
            drop(t);
            let opened = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&out);
            match opened {
                Ok(mut f) => {
                    use std::io::Write;
                    let body = format!("{}\n", lines.join("\n"));
                    if let Err(e) = f.write_all(body.as_bytes()) {
                        tracing::warn!("writing $GITHUB_OUTPUT failed: {e}");
                    }
                }
                Err(e) => tracing::warn!("opening $GITHUB_OUTPUT failed: {e}"),
            }
        }
    }

    /// Write the live comment.
    ///
    /// Unconditional once the build starts: the comment is created on its first
    /// event and kept current for the whole run, so it is present and findable
    /// from the moment the build begins rather than appearing partway through.
    fn sync_comment(&self, budget: Option<Duration>) {
        let Some(c) = &self.comment else { return };
        c.sync(|| self.render_live(), budget);
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
        // The embed is reserved out of the budget *before* the prose, because a
        // truncated fact is recoverable for a human while a truncated JSON object
        // is unparseable — it would break the agent that depends on it.
        let embed = report::embed_marker(&report::embedded_document(
            &tally,
            &tally.counters(),
            tally.elapsed_ms(now_unix_ms()),
            self.json_path
                .as_ref()
                .map(|p| p.display().to_string())
                .as_deref(),
        ));
        let budget = render::SUMMARY_BUDGET.saturating_sub(embed.len().saturating_add(2));
        let mut out = render::render_final(&tally, &self.ctx(), budget);
        out.push('\n');
        out.push_str(&embed);
        out.push('\n');
        out
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
    /// Build from the plugin's `hook:` options, whatever their `enable` says
    /// (that is the caller's decision). Reads the runner's environment
    /// (`$GITHUB_STEP_SUMMARY`, the PR, the token); spawns nothing until the
    /// first event.
    pub fn new(opts: HookOptions) -> Self {
        let HookOptions {
            enable: _,
            refresh_secs,
            slow_after_secs,
            summary_path,
            json_path,
            annotations,
            token_env,
            comment_key: comment_key_opt,
        } = opts;
        tracing::debug!("gha hook loaded");
        let summary_path =
            summary_path.or_else(|| std::env::var_os("GITHUB_STEP_SUMMARY").map(PathBuf::from));
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

        // One sticky PR comment per job leg; within it, one section per heph
        // command so a job's steps each keep their own results.
        let job_key = match comment_key_opt.filter(|k| !k.is_empty()) {
            Some(explicit) => explicit,
            None => comment_key(&command, std::env::var("GITHUB_JOB").ok(), runner_leg()),
        };
        // Hashed: the raw command can contain the marker delimiter, which used to
        // append a new section on every step until the body hit GitHub's cap.
        let section_key = section_id(if command.is_empty() { "heph" } else { &command });
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
            started: AtomicBool::new(false),
            refresh: Duration::from_secs(refresh_secs.max(1)),
            slow_after_ms: slow_after_secs.max(1).saturating_mul(1000),
            run_url,
            json_path,
            annotations,
            annotated: Mutex::new(std::collections::HashSet::new()),
            last_flush: Mutex::new(None),
        });

        Self { inner }
    }
}

/// The hook's options, the plugin's `hook:` map. Decoding reads nothing but
/// the map, so it runs everywhere — a wrong-typed `refreshSecs` fails the same
/// on a laptop as in a runner — while [`GhaHook::new`] reads the runner's
/// environment.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase", default)]
pub struct HookOptions {
    /// Whether the hook is built at all (default false). Nothing about the
    /// environment turns it on, so a plugin enabled for `release_asset` alone
    /// gets no hook (and no warning on every run), and a CI job says
    /// `enable: true` in its config.
    pub enable: bool,
    /// Live PR-comment PATCH interval (at least 1).
    refresh_secs: u64,
    /// How long a target must run before it is surfaced (at least 1).
    slow_after_secs: u64,
    /// Final step-summary file; `$GITHUB_STEP_SUMMARY` when unset.
    summary_path: Option<PathBuf>,
    json_path: Option<PathBuf>,
    annotations: bool,
    /// The env var holding the API token; `GITHUB_TOKEN` when unset.
    token_env: Option<String>,
    /// Empty is unset.
    comment_key: Option<String>,
}

impl Default for HookOptions {
    fn default() -> Self {
        Self {
            enable: false,
            refresh_secs: 30,
            // 30s, not the previous hardcoded 10s: with a 30-second refresh, a
            // 10s threshold lists half of a cold build as "slow".
            slow_after_secs: 30,
            summary_path: None,
            json_path: None,
            annotations: true,
            token_env: None,
            comment_key: None,
        }
    }
}

/// A hook standing in for one whose configuration failed: it reports the
/// error once, on the first event (or at close, for a command that emits
/// none), through the host's log sink, which is installed by then — unlike at
/// plugin creation, where an error has no channel and an abort kills the host
/// with a bare `SIGABRT`.
pub struct ConfigErrorHook {
    error: String,
    reported: AtomicBool,
}

impl ConfigErrorHook {
    pub fn new(error: &anyhow::Error) -> Self {
        Self {
            error: format!("{error:#}"),
            reported: AtomicBool::new(false),
        }
    }

    fn report(&self) {
        if !self.reported.swap(true, Ordering::AcqRel) {
            tracing::error!(
                "gh plugin: {}; no GitHub Actions report is written. Fix the plugin's \
                 `options:` in .hephconfig",
                self.error
            );
        }
    }
}

impl Hook for ConfigErrorHook {
    fn name(&self) -> String {
        "gha".to_string()
    }

    fn on_event(&self, _ev: &BuildEvent) {
        self.report();
    }

    fn on_close(&self) {
        self.report();
    }
}

impl Inner {
    /// Start live updates, once, on the build's first event.
    ///
    /// Not at construction: the hook is loaded for every command, and one that
    /// runs no build emits no event and gets no `on_close`, so a comment posted
    /// up front stays "⏳" forever. A plain thread (no async runtime) keeps the
    /// hook free of runtime entanglement; it posts the comment, then PATCHes it
    /// every `refreshSecs` until `on_close` sets `stop`.
    fn start(self: &Arc<Self>) {
        // A load first: this runs on every event, and the swap is a write.
        if self.started.load(Ordering::Acquire) || self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        if self.comment.is_none() {
            return;
        }
        let t = Arc::clone(self);
        std::thread::spawn(move || {
            while !t.stop.load(Ordering::Acquire) {
                // Rendered under the comment lock, so a sync that loses the
                // race with `on_close` still renders the closed tally.
                t.sync_comment(None);
                std::thread::sleep(t.refresh);
            }
        });
    }
}

impl Hook for GhaHook {
    fn name(&self) -> String {
        "gha".to_string()
    }

    fn on_event(&self, ev: &BuildEvent) {
        self.inner.start();
        let newly_failed = {
            let mut t = self.inner.tally.lock().unwrap_or_else(|e| e.into_inner());
            let before = t.counters().roots_total;
            t.apply(ev);
            t.counters().roots_total > before
        };
        // Only on the edge, so the ~160k events of a 20k-target build do no work
        // here beyond the fold itself.
        if newly_failed {
            self.inner.notice_failures();
        }
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
        // No event, no build: nothing was posted, and nothing should be now.
        // An ordinary sync: it re-reads the comment like every other, so the
        // last write carries the other kind's block as it is now.
        if self.inner.started.load(Ordering::Acquire) {
            self.inner.sync_comment(Some(CLOSE_SYNC_BUDGET));
        }
        // The summary is the durable report; the comment is only a notification.
        self.inner.write_summary();
        self.inner.write_machine_surfaces();
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
        assert_eq!(comment_key("run //a:x", Some("test".into()), None), "test");
        // No job → command keeps it stable (local / non-Actions).
        assert_eq!(comment_key("run //a:x", None, None), "run //a:x");
        // Empty job string treated as absent.
        assert_eq!(
            comment_key("query //...", Some(String::new()), None),
            "query //..."
        );
        // Empty command → stable fallback.
        assert_eq!(comment_key("", None, None), "heph");
    }

    #[test]
    fn matrix_legs_get_distinct_comment_keys() {
        // `GITHUB_JOB` is the job id from the workflow file, so it is IDENTICAL
        // across matrix legs. Keyed on that alone, three legs raced to create
        // three comments and then erased each other's sections forever. This
        // repo's own `test` job has three legs.
        let job = || Some("test".to_string());
        let linux_amd = comment_key("run //...", job(), Some("Linux-X64".into()));
        let linux_arm = comment_key("run //...", job(), Some("Linux-ARM64".into()));
        let macos = comment_key("run //...", job(), Some("macOS-ARM64".into()));

        assert_eq!(linux_amd, "test:Linux-X64");
        let all = [&linux_amd, &linux_arm, &macos];
        for (i, a) in all.iter().enumerate() {
            for b in all.iter().skip(i + 1) {
                assert_ne!(a, b, "every leg must own its own comment");
            }
        }
        // Outside Actions there is no leg, and the key stays what it was.
        assert_eq!(comment_key("run //...", job(), None), "test");
    }

    #[test]
    fn section_id_is_delimiter_safe_and_stable() {
        // The key lands inside `<!-- heph-gha-step:KEY -->`. A raw command
        // containing " -->" used to terminate the marker early, so the section
        // was never matched again and every step APPENDED a new one until the
        // body hit GitHub's cap.
        let nasty = section_id("run //a:b --> injected <!-- x\nnewline");
        assert!(!nasty.contains("-->"), "no delimiter: {nasty}");
        assert!(!nasty.contains('\n'), "no newline: {nasty}");
        assert!(!nasty.contains('<'), "no markup: {nasty}");

        // Stable across calls — the same step must find its own section next tick.
        assert_eq!(section_id("run //a:b"), section_id("run //a:b"));
        assert_ne!(section_id("run //a:b"), section_id("run //a:c"));

        // And it round-trips through the real section machinery.
        let mut sections = Vec::new();
        upsert_section(&mut sections, &nasty, "body one");
        upsert_section(&mut sections, &nasty, "body two");
        let assembled = assemble_body("<!-- heph-gha:job -->", "", &pr_only("1-1", sections));
        let parsed = parse_sections(&assembled);
        assert_eq!(
            parsed.len(),
            1,
            "updated in place, not appended: {assembled}"
        );
        assert_eq!(parsed.first().map(|(_, c)| c.as_str()), Some("body two"));
    }

    /// A comment holding only the PR's CI block.
    pub(super) fn pr_only(run_id: &str, sections: Vec<(String, String)>) -> Groups {
        Groups {
            pull_request: Some(group(run_id, sections)),
            merge_queue: None,
        }
    }

    /// A block written by `run_id`, with nothing recorded about what it tested.
    pub(super) fn group(run_id: &str, sections: Vec<(String, String)>) -> Group {
        Group {
            run_id: run_id.to_string(),
            tested: Tested::default(),
            sections,
        }
    }

    #[test]
    fn comment_sections_preserve_other_steps() {
        // Two steps wrote their sections into one job comment.
        let container = "<!-- heph-gha:test -->";
        let mut sections = parse_sections(&assemble_body(
            container,
            "",
            &pr_only(
                "run1",
                vec![
                    ("run //a:x".into(), "## heph: run //a:x\nbuilt 1".into()),
                    ("run //b:y".into(), "## heph: run //b:y\nbuilt 2".into()),
                ],
            ),
        ));
        assert_eq!(sections.len(), 2);

        // A third step updates only its own section; the others stay.
        upsert_section(&mut sections, "run //a:x", "## heph: run //a:x\nbuilt 9");
        let body = assemble_body(container, "", &pr_only("run1", sections.clone()));
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
            parse_sections(&assemble_body(container, "", &pr_only("run1", sections))).len(),
            3
        );
    }

    #[test]
    fn both_blocks_round_trip_in_order_under_their_headings() {
        let container = "<!-- heph-gha:test -->";
        let sha = "0123456789ab";
        let groups = Groups {
            pull_request: Some(Group {
                run_id: "10-2".into(),
                tested: Tested {
                    base: String::new(),
                    sha: "fedcba987654".into(),
                },
                sections: vec![
                    ("a".into(), "pr step a".into()),
                    ("b".into(), "pr step b".into()),
                ],
            }),
            merge_queue: Some(Group {
                run_id: "11-1".into(),
                tested: Tested {
                    base: "release/v2".into(),
                    sha: sha.into(),
                },
                sections: vec![("a".into(), "queue step a".into())],
            }),
        };
        let body = assemble_body(container, "https://github.com/o/r", &groups);

        // What each run tested round-trips through the hidden markers: the
        // heading is regenerated from them on every write.
        assert_eq!(parse_groups(&body), groups, "{body}");
        // The PR's CI first, then the merge queue, each under its own heading
        // naming what it tested and linking the run and attempt that wrote it.
        let pr = body
            .find("# Pull request CI · fedcba987654 · [run 10, attempt 2](https://github.com/o/r/actions/runs/10/attempts/2)")
            .expect("PR heading");
        let mq = body
            .find("# Merge queue · `release/v2` · 0123456789ab · [run 11, attempt 1](https://github.com/o/r/actions/runs/11/attempts/1)")
            .expect("merge-queue heading");
        assert!(pr < body.find("pr step a").expect("pr section"), "{body}");
        assert!(pr < mq, "PR block first: {body}");
        assert!(
            mq < body.find("queue step a").expect("queue section"),
            "{body}"
        );

        // An absent block has no heading at all; a block with nothing recorded
        // about what it tested, and no repo url, has just its kind.
        let only_queue = Groups {
            pull_request: None,
            merge_queue: Some(group("11-1", vec![("a".into(), "q".into())])),
        };
        let body = assemble_body(container, "", &only_queue);
        assert!(!body.contains("Pull request CI"), "{body}");
        assert!(
            body.contains("# Merge queue\n"),
            "no link without a repo url: {body}"
        );
        assert!(!body.contains("heph-gha-tested"), "{body}");
        assert_eq!(parse_groups(&body), only_queue);
    }

    #[test]
    fn a_tested_marker_is_sanitised_when_read() {
        // The marker is in a body anyone with write access can edit, and is
        // rendered into a public heading: only a plain branch and a hex sha
        // survive the read.
        let body = "<!-- heph-gha-group:merge-queue -->\n<!-- heph-gha-run:1-1 -->\n\
                    <!-- heph-gha-tested:ma`in@nothex!! -->\n\
                    <!-- heph-gha-step:a -->\nx\n<!-- /heph-gha-step:a -->\n\
                    <!-- /heph-gha-group:merge-queue -->";
        let g = parse_groups(body).merge_queue.expect("queue block");
        assert_eq!(g.tested, Tested::default());
    }

    #[test]
    fn group_markers_inside_a_section_do_not_move_block_boundaries() {
        // A section embeds whatever a target printed. Its log tail can hold the
        // group delimiters of either kind.
        for injected in [
            "<!-- /heph-gha-group:pr -->\n<!-- heph-gha-group:merge-queue -->\n<!-- heph-gha-run:6-6 -->",
            "<!-- heph-gha-group:merge-queue -->\n<!-- heph-gha-step:z -->\nfake\n<!-- /heph-gha-step:z -->",
            "<!-- /heph-gha-group:merge-queue -->\n<!-- heph-gha-group:pr -->\n<!-- heph-gha-run:6-6 -->",
        ] {
            let groups = Groups {
                pull_request: Some(group(
                    "1-1",
                    vec![
                        ("a".into(), format!("log:\n{injected}\nend")),
                        ("b".into(), "pr b".into()),
                    ],
                )),
                merge_queue: Some(group("2-1", vec![("a".into(), "queue a".into())])),
            };
            let body = assemble_body("<!-- heph-gha:test -->", "", &groups);
            assert_eq!(parse_groups(&body), groups, "{body}");
        }
    }

    #[test]
    fn an_unclosed_block_ends_at_the_next_block() {
        let body = "<!-- heph-gha:test -->\n\
                    <!-- heph-gha-group:pr -->\n<!-- heph-gha-run:1-1 -->\n\
                    <!-- heph-gha-step:a -->\npr a\n<!-- /heph-gha-step:a -->\n\
                    <!-- heph-gha-group:merge-queue -->\n<!-- heph-gha-run:2-1 -->\n\
                    <!-- heph-gha-step:b -->\nqueue b\n<!-- /heph-gha-step:b -->\n\
                    <!-- /heph-gha-group:merge-queue -->";
        assert_eq!(
            parse_groups(body),
            Groups {
                pull_request: Some(group("1-1", vec![("a".into(), "pr a".into())])),
                merge_queue: Some(group("2-1", vec![("b".into(), "queue b".into())])),
            }
        );
    }

    #[test]
    fn blocks_out_of_order_are_assembled_pr_first() {
        let body = "<!-- heph-gha:test -->\n\
                    <!-- heph-gha-group:merge-queue -->\n<!-- heph-gha-run:2-1 -->\n\
                    <!-- heph-gha-step:b -->\nqueue b\n<!-- /heph-gha-step:b -->\n\
                    <!-- /heph-gha-group:merge-queue -->\n\
                    <!-- heph-gha-group:pr -->\n<!-- heph-gha-run:1-1 -->\n\
                    <!-- heph-gha-step:a -->\npr a\n<!-- /heph-gha-step:a -->\n\
                    <!-- /heph-gha-group:pr -->";
        let groups = parse_groups(body);
        assert_eq!(
            groups,
            Groups {
                pull_request: Some(group("1-1", vec![("a".into(), "pr a".into())])),
                merge_queue: Some(group("2-1", vec![("b".into(), "queue b".into())])),
            }
        );
        let out = assemble_body("<!-- heph-gha:test -->", "", &groups);
        assert!(
            out.find("pr a").expect("pr") < out.find("queue b").expect("queue"),
            "{out}"
        );
    }

    #[test]
    fn a_duplicated_block_keeps_the_first() {
        let block = |run: &str, text: &str| {
            format!(
                "<!-- heph-gha-group:pr -->\n<!-- heph-gha-run:{run} -->\n\
                 <!-- heph-gha-step:a -->\n{text}\n<!-- /heph-gha-step:a -->\n\
                 <!-- /heph-gha-group:pr -->\n"
            )
        };
        let body = format!(
            "<!-- heph-gha:test -->\n{}{}",
            block("1-1", "first"),
            block("9-9", "second")
        );
        assert_eq!(
            parse_groups(&body),
            Groups {
                pull_request: Some(group("1-1", vec![("a".into(), "first".into())])),
                merge_queue: None,
            }
        );
    }

    #[test]
    fn a_run_marker_that_is_not_a_run_reads_as_no_run() {
        // The run id is echoed into the public heading's link.
        for bad in [
            "1-x",
            "](https://evil)-1",
            "12",
            "-3",
            "4-",
            "+1-2",
            "1-2-3",
        ] {
            let body = format!(
                "<!-- heph-gha-group:pr -->\n<!-- heph-gha-run:{bad} -->\n\
                 <!-- heph-gha-step:a -->\nx\n<!-- /heph-gha-step:a -->\n\
                 <!-- /heph-gha-group:pr -->"
            );
            let g = parse_groups(&body).pull_request.expect("PR block");
            assert_eq!(g.run_id, "", "{bad}");
        }
        assert_eq!(clean_run_id("123-4"), "123-4");
    }

    #[test]
    fn an_unclosed_section_ends_its_block_not_the_parse() {
        // A truncated PR section: its block ends there, and the merge-queue
        // block after it is still read.
        let body = "<!-- heph-gha:test -->\n\
                    <!-- heph-gha-group:pr -->\n<!-- heph-gha-run:1-1 -->\n\
                    <!-- heph-gha-step:a -->\npr a\n<!-- /heph-gha-step:a -->\n\
                    <!-- heph-gha-step:b -->\ntruncated\n\
                    <!-- heph-gha-group:merge-queue -->\n<!-- heph-gha-run:2-1 -->\n\
                    <!-- heph-gha-step:c -->\nqueue c\n<!-- /heph-gha-step:c -->\n\
                    <!-- /heph-gha-group:merge-queue -->";
        assert_eq!(
            parse_groups(body),
            Groups {
                pull_request: Some(group("1-1", vec![("a".into(), "pr a".into())])),
                merge_queue: Some(group("2-1", vec![("c".into(), "queue c".into())])),
            }
        );
    }

    #[test]
    fn a_section_holding_its_own_close_tag_ends_there() {
        // The documented limit: the section ends at the first copy of its own
        // close tag, and the rest of its content is read as top level. Nothing
        // panics, and the other kind's block survives.
        let groups = Groups {
            pull_request: Some(group(
                "1-1",
                vec![(
                    "a".into(),
                    "before\n<!-- /heph-gha-step:a -->\nafter".into(),
                )],
            )),
            merge_queue: Some(group("2-1", vec![("c".into(), "queue c".into())])),
        };
        let body = assemble_body("<!-- heph-gha:test -->", "", &groups);
        let parsed = parse_groups(&body);
        assert_eq!(
            parsed.pull_request,
            Some(group("1-1", vec![("a".into(), "before".into())]))
        );
        assert_eq!(parsed.merge_queue, groups.merge_queue);
    }

    #[test]
    fn both_blocks_at_full_section_budget_fit_the_comment() {
        // Three steps per block, each section rendered at the full live budget,
        // with every marker and heading at its longest.
        let filler = "x".repeat(render::LIVE_SECTION_BUDGET);
        let sections = |n: usize| -> Vec<(String, String)> {
            (0..n)
                .map(|i| (section_id(&format!("step {i}")), filler.clone()))
                .collect()
        };
        let long_run = format!("{}-{}", u64::MAX, u64::MAX);
        let groups = Groups {
            pull_request: Some(Group {
                run_id: long_run.clone(),
                tested: Tested {
                    base: String::new(),
                    sha: "0123456789ab".into(),
                },
                sections: sections(3),
            }),
            merge_queue: Some(Group {
                run_id: long_run,
                tested: Tested {
                    base: "b".repeat(100),
                    sha: "0123456789ab".into(),
                },
                sections: sections(3),
            }),
        };
        let body = assemble_body(
            &format!("<!-- heph-gha:{} -->", "k".repeat(200)),
            "https://github.example.com/some-org/some-repository",
            &groups,
        );
        assert!(
            body.chars().count() <= render::COMMENT_LIMIT,
            "{} chars",
            body.chars().count()
        );
    }

    #[test]
    fn a_body_from_before_the_blocks_is_read_as_the_pr_block() {
        // Before merge-queue runs reported, the body was one run marker and its
        // sections. Only PR runs wrote it, so it is the PR block: a merge-queue
        // run adopting it must keep it, not drop it.
        let legacy = "<!-- heph-gha:test -->\n<!-- heph-gha-run:10-1 -->\n\
                      <!-- heph-gha-step:a -->\nold a\n<!-- /heph-gha-step:a -->";
        let groups = parse_groups(legacy);
        assert_eq!(
            groups.pull_request,
            Some(group("10-1", vec![("a".into(), "old a".into())]))
        );
        assert_eq!(groups.merge_queue, None);
        // Markers and nothing else: no block.
        assert_eq!(
            parse_groups("<!-- heph-gha:test -->\n<!-- heph-gha-run:1-1 -->"),
            Groups::default()
        );
        assert_eq!(parse_groups(""), Groups::default());
    }

    #[test]
    fn pr_and_run_kind_come_from_the_event_and_ref() {
        let target = |pr, kind, base: &str, sha: &str| {
            Some(PrTarget {
                pr,
                kind,
                tested: Tested {
                    base: base.into(),
                    sha: sha.into(),
                },
            })
        };
        let pr = |n| target(n, RunKind::PullRequest, "", "");
        let pr_at = |n, sha| target(n, RunKind::PullRequest, "", sha);
        // `GITHUB_SHA`: the commit a merge-group run checked out.
        let run_sha = "fedcba9876543210fedcba9876543210fedcba98";
        let mq = |n, base| target(n, RunKind::MergeQueue, base, "fedcba987654");
        let at = |payload: &str, git_ref: &str, event: &str| {
            pr_target_from(Some(payload.as_bytes()), git_ref, event, run_sha)
        };
        let pr_target_from = |payload: Option<&[u8]>, git_ref: &str, event: &str| {
            pr_target_from(payload, git_ref, event, run_sha)
        };

        let sha = "0123456789abcdef0123456789abcdef01234567";
        let payload = serde_json::json!({ "pull_request": { "number": 122 } }).to_string();
        assert_eq!(at(&payload, "", "pull_request"), pr(122));
        // The PR's head commit, short.
        let with_head =
            serde_json::json!({ "pull_request": { "number": 122, "head": { "sha": sha } } })
                .to_string();
        assert_eq!(
            at(&with_head, "", "pull_request"),
            pr_at(122, "0123456789ab")
        );
        assert_eq!(at("{}", "", "push"), None);
        assert_eq!(at("not json", "refs/pull/3/merge", "pull_request"), pr(3));
        // Ref fallback.
        assert_eq!(pr_target_from(None, "refs/pull/122/merge", ""), pr(122));
        assert_eq!(pr_target_from(None, "refs/pull/7/head", ""), pr(7));
        assert_eq!(pr_target_from(None, "refs/heads/main", ""), None);
        assert_eq!(pr_target_from(None, "refs/pull/+7/merge", ""), None);

        // Merge queue: the payload's head_ref, else GITHUB_REF.
        let queue_ref = format!("refs/heads/gh-readonly-queue/main/pr-42-{sha}");
        let queue = serde_json::json!({ "merge_group": { "head_ref": queue_ref } }).to_string();
        assert_eq!(at(&queue, "", "merge_group"), mq(42, "main"));
        // The payload alone says it is a merge-group event.
        assert_eq!(at(&queue, "", ""), mq(42, "main"));
        // What was tested is the group's head commit on its base branch, from
        // the payload — not the sha in the queue ref's name.
        let full = serde_json::json!({ "merge_group": {
            "head_ref": queue_ref,
            "head_sha": "abcdef0123456789abcdef0123456789abcdef01",
            "base_ref": "refs/heads/release/v2",
        } })
        .to_string();
        assert_eq!(
            at(&full, "", "merge_group"),
            target(42, RunKind::MergeQueue, "release/v2", "abcdef012345")
        );
        assert_eq!(
            pr_target_from(None, &queue_ref, "merge_group"),
            mq(42, "main")
        );
        // An unparsable head_ref in the payload falls back to GITHUB_REF.
        let bad =
            serde_json::json!({ "merge_group": { "head_ref": "refs/heads/nope" } }).to_string();
        assert_eq!(at(&bad, &queue_ref, "merge_group"), mq(42, "main"));
        // A base branch with slashes, or named like a PR segment.
        assert_eq!(
            pr_target_from(
                None,
                &format!("refs/heads/gh-readonly-queue/release/pr-9-x/pr-17-{sha}"),
                "merge_group"
            ),
            mq(17, "release/pr-9-x")
        );

        // A push to a queue branch is not a merge-queue run — nor a PR run.
        assert_eq!(pr_target_from(None, &queue_ref, "push"), None);
        assert_eq!(at("{}", &queue_ref, "push"), None);

        // Near misses are not merge-queue refs.
        for not_queue in [
            "refs/heads/gh-readonly-queue/pr-42-abc", // no base
            "refs/heads/gh-readonly-queue/main/pr--abc",
            "refs/heads/gh-readonly-queue/main/prx-42-abc",
            "refs/heads/gh-readonly-queue/main/pr-+42-abc",
            "refs/heads/gh-readonly-queue/main/pr-99999999999999999999999-abc", // overflow
            "refs/heads/gh-readonly-queue-x/main/pr-42-abc",
            "refs/heads/feature/gh-readonly-queue/main/pr-42-abc",
        ] {
            assert_eq!(
                pr_target_from(None, not_queue, "merge_group"),
                None,
                "{not_queue}"
            );
        }
    }

    fn hook_writing_to(path: &std::path::Path) -> GhaHook {
        GhaHook::new(HookOptions {
            summary_path: Some(path.to_path_buf()),
            ..HookOptions::default()
        })
    }

    /// The plugin's whole `options:`, from YAML, decoded.
    fn decode(yaml: &str) -> anyhow::Result<GhOptions> {
        GhOptions::decode(&serde_yaml::from_str(yaml)?)
    }

    fn err_of<T>(r: anyhow::Result<T>) -> String {
        match r {
            Ok(_) => panic!("must fail"),
            Err(e) => format!("{e:#}"),
        }
    }

    /// No `hook:`, a bare one, an empty one: the hook's defaults, so no hook.
    #[test]
    fn an_absent_or_empty_hook_is_the_defaults() {
        for yaml in ["{}", "hook:", "hook: {}", "hook: {enable: false}"] {
            assert_eq!(decode(yaml).unwrap().hook, HookOptions::default(), "{yaml}");
        }
        let defaults = HookOptions::default();
        assert!(!defaults.enable);
        assert_eq!((defaults.refresh_secs, defaults.slow_after_secs), (30, 30));
        assert!(defaults.annotations);
        assert!(decode("hook: {enable: true}").unwrap().hook.enable);
    }

    /// The options are decoded whether or not the hook is on, so a wrong-typed
    /// one fails before anyone turns the hook on.
    #[test]
    fn a_bad_hook_option_fails_even_when_disabled() {
        for yaml in [
            "hook: {refreshSecs: soon}",
            "hook: {enable: false, refreshSecs: soon}",
            "hook: {enable: true, refreshSecs: soon}",
        ] {
            let msg = err_of(decode(yaml));
            assert!(msg.contains("refreshSecs"), "{yaml}: {msg}");
        }
    }

    #[test]
    fn enable_must_be_a_bool() {
        for yaml in [
            "hook: {enable: 'yes'}",
            "hook: {enable: 1}",
            "hook: {enable: [true]}",
        ] {
            let msg = err_of(decode(yaml));
            assert!(msg.contains("hook.enable"), "{yaml}: {msg}");
            assert!(msg.contains("expected a boolean"), "{yaml}: {msg}");
        }
    }

    /// The nested options reach the hook.
    #[test]
    fn nested_hook_options_reach_the_hook() {
        let opts = decode("hook:\n  enable: true\n  refreshSecs: 5\n  commentKey: k\n").unwrap();
        assert!(opts.hook.enable);
        assert_eq!(opts.hook.refresh_secs, 5);
        assert_eq!(opts.hook.comment_key.as_deref(), Some("k"));
    }

    #[test]
    fn hook_must_be_a_map() {
        for yaml in ["hook: true", "hook: 30", "hook: enable", "hook: [enable]"] {
            let msg = err_of(decode(yaml));
            assert!(msg.contains("hook: invalid type"), "{yaml}: {msg}");
            assert!(msg.contains("expected struct HookOptions"), "{yaml}: {msg}");
        }
    }

    /// An unknown key is refused at either level, naming the key and what the
    /// level allows. A hook option at the top level (where it used to go) is
    /// one, and so is a server or API URL: they are `release_asset`'s
    /// `server_url` and `api_url` arguments, per call, not plugin options.
    #[test]
    fn unknown_keys_are_refused_at_either_level() {
        let msg = err_of(decode("hook: {refreshSec: 5}"));
        assert!(msg.contains("unknown field `refreshSec`"), "{msg}");
        assert!(msg.contains("`refreshSecs`"), "{msg}");
        for yaml in [
            "serverUrl: https://ghe.example",
            "apiUrl: https://api.ghe.example",
            "refreshSecs: 30",
            "enable: true",
        ] {
            let msg = err_of(decode(yaml));
            let key = yaml.split(':').next().unwrap_or_default();
            assert!(msg.contains(&format!("unknown field `{key}`")), "{msg}");
            assert!(msg.contains("expected `hook`"), "{msg}");
        }
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
            0,
            BuildEventKind::CacheOutcome {
                addr: "//a:x".into(),
                outcome: hcore::events::CacheOutcomeKind::LocalHit,
                reason: None,
                stale_host_path: None,
            },
        ));
        hook.on_event(&ev(
            5,
            BuildEventKind::ResultEnd {
                duplicate: false,
                upstream_file: None,
                path: Vec::new(),
                left_behind: None,
                cancelled: false,
                timed_out: None,
                addr: "//a:x".into(),
                error: None,
                upstream_of: None,
                exit_status: None,
                exit_code: None,
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
        // A flag's value that happens to look like a word is not taken for a
        // label, even when the flag is boolean and it really is one.
        assert_eq!(
            label_from(&["run", "--force", "build", "//..."]),
            "run //..."
        );
        // Entrypoint arguments are never shown.
        assert_eq!(
            label_from(&["run", "//t:go", "--", "//x:y", "secret"]),
            "run //t:go"
        );
    }

    #[test]
    fn command_label_names_what_is_run() {
        // `heph run <LABEL> <MATCHER>`: the label is what is run.
        assert_eq!(label_from(&["run", "test", "//..."]), "run test //...");
        // Global flags before the subcommand, with and without `=`.
        assert_eq!(
            label_from(&["-j", "4", "run", "--no-tui", "//a:b"]),
            "run //a:b"
        );
        assert_eq!(label_from(&["-j=4", "run", "//a:b"]), "run //a:b");
        // `-e EXPR`, in every spelling clap accepts that the label handles.
        let expr = "//a/... && !label(slow)";
        assert_eq!(
            label_from(&["run", "-e", expr]),
            "run -e `//a/... && !label(slow)`"
        );
        assert_eq!(
            label_from(&["run", "--expr", expr]),
            "run -e `//a/... && !label(slow)`"
        );
        assert_eq!(
            label_from(&["run", &format!("--expr={expr}")]),
            "run -e `//a/... && !label(slow)`"
        );
        // An expression cannot break out of its code span or open an HTML
        // comment in the body.
        assert_eq!(
            label_from(&["run", "-e", "//a` <!-- x -->\n"]),
            "run -e `//a !-- x --`"
        );
        // Relative matchers and subcommand paths.
        assert_eq!(label_from(&["run", "build", "./..."]), "run build ./...");
        assert_eq!(
            label_from(&["tool", "cache", "measure-latency"]),
            "tool cache measure-latency"
        );
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

    fn hook_with(summary: &std::path::Path, json: &std::path::Path) -> GhaHook {
        GhaHook::new(HookOptions {
            summary_path: Some(summary.to_path_buf()),
            json_path: Some(json.to_path_buf()),
            ..HookOptions::default()
        })
    }

    #[test]
    fn the_summary_carries_a_parseable_embedded_json_document() {
        let dir = tempfile::tempdir().expect("tempdir");
        let summary = dir.path().join("summary.md");
        let json = dir.path().join("heph.json");
        let hook = hook_with(&summary, &json);

        hook.on_event(&ev(
            0,
            BuildEventKind::Matched {
                addrs: vec!["//a:x".into()],
                complete: true,
            },
        ));
        hook.on_event(&ev(
            1_000,
            BuildEventKind::ResultEnd {
                duplicate: false,
                upstream_file: None,
                path: Vec::new(),
                left_behind: None,
                cancelled: false,
                timed_out: None,
                addr: "//a:x".into(),
                error: Some("target failed".into()),
                upstream_of: None,
                exit_status: Some("exit status: 1".into()),
                exit_code: None,
                log_tail: None,
            },
        ));
        hook.on_close();

        // The embed must survive in the summary AND parse — a truncated JSON
        // object would break the agent that reads it.
        let body = std::fs::read_to_string(&summary).expect("summary written");
        let start = body.find(report::EMBED_OPEN).expect("embed present");
        let rest = body.get(start + report::EMBED_OPEN.len()..).expect("after");
        let end = rest.find(report::EMBED_CLOSE).expect("embed closed");
        let raw = rest.get(..end).expect("json slice");
        assert!(
            raw.len() <= report::EMBED_MAX,
            "embed capped: {}",
            raw.len()
        );
        let v: serde_json::Value = serde_json::from_str(raw).expect("embed parses");
        assert_eq!(v["status"], "failed");
        assert_eq!(v["targets"]["failed"], 1);

        // And the full document is written separately, with the detail the embed
        // deliberately omits.
        let full: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&json).expect("json written"))
                .expect("full doc parses");
        assert_eq!(full["schema"], report::SCHEMA);
        assert_eq!(
            full["failures"][0]["exit_status"], "exit status: 1",
            "detail lives in the file, not the embed"
        );
    }

    #[test]
    fn a_collateral_cone_does_not_reach_the_json_failure_list() {
        let dir = tempfile::tempdir().expect("tempdir");
        let json = dir.path().join("heph.json");
        let hook = hook_with(&dir.path().join("s.md"), &json);
        hook.on_event(&ev(
            10,
            BuildEventKind::ResultEnd {
                duplicate: false,
                upstream_file: None,
                path: Vec::new(),
                left_behind: None,
                cancelled: false,
                timed_out: None,
                addr: "//base:proto".into(),
                error: Some("boom".into()),
                upstream_of: None,
                exit_status: None,
                exit_code: None,
                log_tail: None,
            },
        ));
        for i in 0..4_117 {
            hook.on_event(&ev(
                20,
                BuildEventKind::ResultEnd {
                    duplicate: false,
                    upstream_file: None,
                    path: Vec::new(),
                    left_behind: None,
                    cancelled: false,
                    timed_out: None,
                    addr: format!("//svc:t{i}"),
                    error: Some("dependency failed".into()),
                    upstream_of: Some("//base:proto".into()),
                    exit_status: None,
                    exit_code: None,
                    log_tail: None,
                },
            ));
        }
        hook.on_close();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&json).expect("json")).expect("parses");
        assert_eq!(
            v["failures"].as_array().map(Vec::len),
            Some(1),
            "one root, not 4,118 entries"
        );
        assert_eq!(v["targets"]["blocked"], 4_117);
        assert_eq!(v["failures"][0]["blocked_count"], 4_117);
    }
}
