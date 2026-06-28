//! Anonymous, opt-out usage telemetry.
//!
//! Reports coarse usage shape to PostHog (EU region) to guide development.
//! Every event is *personless* (`$process_person_profile: false` — no person
//! profile is ever created) and carries only os (`$os`/`$os_version`, stamped by
//! posthog-rs), arch, version, an anonymous repo fingerprint (a one-way hash of
//! the git root commit — see [`repo`]), plus aggregate run counters — never
//! target addresses, labels, filesystem paths, or any user identifier. The `distinct_id` is a random per-install UUID stored in the
//! machine's config dir (or the constant `"ci"` on CI runners, where ephemeral
//! machines would otherwise mint a fake install per job); it identifies an
//! installation, not a person.
//!
//! At exit the event is appended to a local SQLite spool (one insert), then the
//! command tries to flush the spool (this run's event plus any backlog) on a
//! worker thread, bounded to 500ms past the end of its real work. Whatever
//! doesn't make that deadline stays spooled and rides along with the next run's
//! flush — so the common case sends this call, exit is never blocked for long,
//! and nothing is lost. On CI (ephemeral runner, no next run) the flush blocks
//! to completion instead. See [`spool`].
//!
//! Disabled by `telemetry.enabled: false` in `.hephconfig2`, or by setting the
//! `HEPH_DISABLE_TELEMETRY` environment variable (the latter also keeps the
//! project's own test/CI binary invocations from reporting). Everything is
//! strictly best-effort: any failure — offline, timeout, disabled — is swallowed
//! and never affects the command's exit.

mod collector;
mod repo;
mod spool;

pub use collector::{QueryExprCounts, TelemetryCollector, TelemetrySnapshot};

use clap::ArgMatches;
use clap::parser::ValueSource;
use hcore::events::BuildEventKind;
use posthog_rs::{ClientOptionsBuilder, Event};
use spool::{FLUSH_BATCH, Spool, SpooledEvent};
use std::path::PathBuf;
use std::time::Duration;

/// Process-global counters. The engine bumps these from its hot paths without
/// threading a handle through every request; the CLI snapshots them once at
/// exit. Always counted (cheap relaxed atomics) — the opt-out only gates
/// whether the snapshot is ever *sent*.
static COLLECTOR: TelemetryCollector = TelemetryCollector::new();

/// Fold one build event into the global counters. Wired into
/// `RequestState::emit`, so every command that resolves targets is covered with
/// no per-command code.
pub fn observe_event(kind: &BuildEventKind) {
    COLLECTOR.observe_event(kind);
}

/// Record one resolved target's artifact count + known per-artifact sizes.
pub fn record_artifacts(count: u64, sizes: &[u64]) {
    COLLECTOR.record_artifacts(count, sizes);
}

/// Record one target's execute wall time (ms) — cache misses only.
pub fn record_execute_ms(ms: u64) {
    COLLECTOR.record_execute_ms(ms);
}

/// Record the total target count of a whole-graph (`//...`) enumeration.
pub fn record_graph_size(total: u64) {
    COLLECTOR.record_graph_size(total);
}

/// Record that an approval-gated target asked the user for a decision.
pub fn record_approval_requested() {
    COLLECTOR.record_approval_requested();
}

/// Record the resolved outcome of an approval prompt (granted vs denied).
pub fn record_approval_decision(granted: bool) {
    COLLECTOR.record_approval_decision(granted);
}

/// Record that an interactive sandbox shell session was actually entered.
pub fn record_shell_session() {
    COLLECTOR.record_shell_session();
}

/// Record one execute's chosen sandbox backend (`fuse` vs unpack-copy).
pub fn record_sandbox(fuse: bool) {
    COLLECTOR.record_sandbox(fuse);
}

/// Record one local-cache blob spilling from the primary store to the FS store.
pub fn record_cache_spill() {
    COLLECTOR.record_cache_spill();
}

/// Record the per-node shape of a parsed `--expr` query matcher tree.
pub fn record_query_expr(counts: &QueryExprCounts) {
    COLLECTOR.record_query_expr(counts);
}

/// Record the interactive-TUI decision (`should_use_tui`'s result).
pub fn record_interactive(interactive: bool) {
    COLLECTOR.record_interactive(interactive);
}

/// Read the global counters (largest-seen for `graph_size`, sums elsewhere).
pub fn snapshot() -> TelemetrySnapshot {
    COLLECTOR.snapshot()
}

/// The engine's enabled plugins (built-ins plus whatever the config turned on)
/// plus how many remote caches are configured. Plugin *type* names and a count
/// only — never user data (URIs, addresses, …).
#[derive(Debug, Clone, Default)]
struct Plugins {
    providers: Vec<String>,
    drivers: Vec<String>,
    remote_cache_count: usize,
    /// Distinct remote cache backend kinds (`s3`, `gcs`, `azure`, `http`,
    /// `file`, `memory`, `other`) — scheme only, never the URI.
    remote_cache_backends: Vec<String>,
}

/// Set once, at engine construction. Read by the reporter at exit.
static PLUGINS: std::sync::OnceLock<Plugins> = std::sync::OnceLock::new();

/// Record the enabled provider + driver names plus the remote cache backend
/// kinds (one entry per configured cache; the count and the distinct set are
/// derived here). First write wins (a process builds at most one engine for the
/// command it runs); later calls are ignored.
pub fn record_plugins(
    mut providers: Vec<String>,
    mut drivers: Vec<String>,
    mut remote_cache_backends: Vec<String>,
) {
    providers.sort();
    drivers.sort();
    let remote_cache_count = remote_cache_backends.len();
    remote_cache_backends.sort();
    remote_cache_backends.dedup();
    // Best-effort set-once; a second engine in one process keeps the first set.
    drop(PLUGINS.set(Plugins {
        providers,
        drivers,
        remote_cache_count,
        remote_cache_backends,
    }));
}

/// Public, write-only PostHog project API key. Not a secret — safe to ship in
/// the binary; it can only ingest events, never read them.
const POSTHOG_API_KEY: &str = "phc_zsqncVWq9QKVCNfhT4cD8oDNBF9FWbLjGjpxBzPzDHkC";
/// EU ingestion host — the project lives in PostHog's EU region.
const POSTHOG_HOST: &str = "https://eu.posthog.com";
/// Env var that force-disables telemetry regardless of config.
const DISABLE_ENV: &str = "HEPH_DISABLE_TELEMETRY";

/// Vendor-specific env vars that, when present, strongly imply a CI runner.
/// `CI` / `CONTINUOUS_INTEGRATION` are the generic ones most systems set; the
/// rest cover vendors that don't (or set `CI` to an unexpected value).
const CI_ENV_VARS: &[&str] = &[
    "CI",
    "CONTINUOUS_INTEGRATION",
    "BUILD_NUMBER",
    "GITHUB_ACTIONS",
    "GITLAB_CI",
    "CIRCLECI",
    "TRAVIS",
    "JENKINS_URL",
    "TEAMCITY_VERSION",
    "BUILDKITE",
    "DRONE",
    "APPVEYOR",
    "BITBUCKET_BUILD_NUMBER",
    "TF_BUILD",
    "CODEBUILD_BUILD_ID",
];

/// Best-effort guess at whether we're running inside CI. `CI` and
/// `CONTINUOUS_INTEGRATION` are treated as truthy unless explicitly `false`/`0`
/// (some shells export `CI=false`); any other vendor marker counts as present.
pub fn is_ci() -> bool {
    ci_from(|name| std::env::var(name).ok())
}

/// CI detection over an injected env lookup, so the parsing rules are testable
/// without mutating the process environment.
fn ci_from(lookup: impl Fn(&str) -> Option<String>) -> bool {
    CI_ENV_VARS.iter().any(|name| match lookup(name) {
        Some(v) => {
            let v = v.trim();
            // A vendor marker present at all signals CI even if empty; a generic
            // `CI=false`/`CI=0` is the one case we treat as opt-out.
            v.is_empty() || (!v.eq_ignore_ascii_case("false") && v != "0")
        }
        None => false,
    })
}

/// Whether telemetry should run, combining the config flag with the env kill
/// switch. The env var wins so CI and the project's own e2e binaries can opt out
/// without touching a config file.
pub fn is_enabled(config_enabled: bool) -> bool {
    config_enabled && std::env::var_os(DISABLE_ENV).is_none()
}

/// Machine-level config dir holding the install id and the spool:
/// `$XDG_CONFIG_HOME/heph` or `~/.config/heph`. Created on demand. Deliberately
/// *not* the per-repo `.heph3` home — the install id must survive cache nukes
/// and be shared across repos.
fn config_dir() -> anyhow::Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok_or_else(|| anyhow::anyhow!("no XDG_CONFIG_HOME or HOME"))?;
    let dir = base.join("heph");
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("creating config dir {}: {e}", dir.display()))?;
    Ok(dir)
}

/// The anonymous install id: a random UUID minted on first use and persisted at
/// `<dir>/telemetry-id`. A pure coin-flip value — derived from nothing (no
/// hostname, MAC, user), it identifies an *installation* only. On CI the caller
/// uses the constant `"ci"` instead (see [`distinct_id`]): ephemeral runners
/// would mint a fresh "install" per job and drown the real numbers.
fn install_id(dir: &std::path::Path) -> anyhow::Result<String> {
    let path = dir.join("telemetry-id");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim();
        if !existing.is_empty() {
            return Ok(existing.to_string());
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    std::fs::write(&path, &id)
        .map_err(|e| anyhow::anyhow!("writing install id {}: {e}", path.display()))?;
    Ok(id)
}

/// The event `distinct_id`: `"ci"` on CI runners, the persistent install UUID
/// otherwise.
fn distinct_id(dir: &std::path::Path) -> anyhow::Result<String> {
    if is_ci() {
        return Ok("ci".to_string());
    }
    install_id(dir)
}

/// Record this invocation and try to send it: build the event from the clap
/// matches + command result, spool it (one SQLite insert), then flush. Non-CI
/// flushes within a 500ms post-work budget and defers the rest to the next run;
/// CI (ephemeral, no next run) blocks to completion. The single entry point the
/// CLI calls at exit; everything below is best-effort and never affects the
/// command's outcome.
/// Kick off background computation of the repo fingerprint so the exit path
/// never runs git. Fire-and-forget: spawns a detached thread and returns at once.
/// Call once at startup when telemetry is enabled; the result is cached and read
/// back (best-effort) by [`record_invocation`]. If the command exits before the
/// warmer finishes, the attr is simply absent this run and lands on the next.
pub fn prewarm() {
    let spawned = std::thread::Builder::new()
        .name("heph-telemetry-warm".to_string())
        .spawn(|| match config_dir() {
            Ok(dir) => repo::prewarm(&dir),
            Err(e) => tracing::debug!(error = %format!("{e:#}"), "telemetry prewarm skipped"),
        });
    // A failure to even spawn the warmer is ignored — strictly best-effort. The
    // JoinHandle is intentionally dropped (detached); we never join it.
    if let Err(e) = spawned {
        tracing::debug!(error = %format!("{e:#}"), "telemetry prewarm thread not spawned");
    }
}

pub fn record_invocation(
    matches: &ArgMatches,
    cmd: &clap::Command,
    error: Option<&anyhow::Error>,
    took: Duration,
) {
    // Full subcommand path ("inspect deps", "tool gc") plus the args set at every
    // nesting level, walked in lockstep with the clap command model so positional
    // selectors (an addr / label) are partitioned out of `flags` rather than
    // mixed in with the real options.
    let mut parts = Vec::new();
    let mut flags = Vec::new();
    let mut positionals = Vec::new();

    let mut level_m = matches;
    let mut level_c = Some(cmd);
    loop {
        match level_c {
            Some(c) => {
                let (f, p) = split_set_args(level_m, c);
                flags.extend(f);
                positionals.extend(p);
            }
            // Model desync (should not happen): keep the ids as flags rather
            // than silently dropping them.
            None => flags.extend(
                level_m
                    .ids()
                    .filter(|id| {
                        level_m.value_source(id.as_str()) == Some(ValueSource::CommandLine)
                    })
                    .map(|id| id.as_str().to_string()),
            ),
        }
        match level_m.subcommand() {
            Some((name, sub_m)) => {
                parts.push(name);
                level_c = level_c.and_then(|c| c.find_subcommand(name));
                level_m = sub_m;
            }
            None => break,
        }
    }

    let command = if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join(" ")
    };
    flags.sort();
    flags.dedup();

    // Selector shape from the positional args present: two positionals (`arg1` +
    // `arg2`) is a label + package matcher; one is a single address. Structure
    // only — the actual label/address values are never read.
    let request_shape = if positionals.iter().any(|p| p == "arg2") {
        "label_matcher"
    } else if positionals.iter().any(|p| p == "arg1") {
        "addr"
    } else {
        "none"
    };

    let failure = error.map(classify_failure);

    if let Err(e) = try_enqueue(ReportContext {
        command: &command,
        request_shape,
        flags: &flags,
        success: failure.is_none(),
        failure,
        duration_ms: took.as_millis() as u64,
    }) {
        tracing::debug!(error = %format!("{e:#}"), "telemetry enqueue skipped");
    }

    // CI has no next run to defer to, so block; otherwise cap the wait.
    if is_ci() {
        flush_blocking();
    } else {
        flush_bounded(Duration::from_millis(500));
    }
}

/// Partition the args explicitly set on the command line at one match level into
/// `(flags, positionals)`, using the clap command model to tell options from
/// positionals. Names only — never the values, which can carry addresses/labels.
fn split_set_args(m: &ArgMatches, cmd: &clap::Command) -> (Vec<String>, Vec<String>) {
    let positional_ids: std::collections::HashSet<&str> = cmd
        .get_arguments()
        .filter(|a| a.is_positional())
        .map(|a| a.get_id().as_str())
        .collect();
    let mut flags = Vec::new();
    let mut positionals = Vec::new();
    for id in m.ids() {
        if m.value_source(id.as_str()) != Some(ValueSource::CommandLine) {
            continue;
        }
        let name = id.as_str();
        if positional_ids.contains(name) {
            positionals.push(name.to_string());
        } else {
            flags.push(name.to_string());
        }
    }
    (flags, positionals)
}

/// Coarse, non-PII failure class: user-cancelled vs a target that genuinely
/// failed vs anything else.
fn classify_failure(e: &anyhow::Error) -> &'static str {
    use hcore::hmemoizer::downcast_chain_ref;
    use hplugin::error::{CancelledError, TargetFailure, UpstreamFailed};
    if downcast_chain_ref::<CancelledError>(e).is_some() {
        "cancelled"
    } else if downcast_chain_ref::<TargetFailure>(e).is_some()
        || downcast_chain_ref::<UpstreamFailed>(e).is_some()
    {
        "target_failure"
    } else {
        "error"
    }
}

/// The shape of a CLI invocation, carrying no user-identifying values.
struct ReportContext<'a> {
    /// Full subcommand path, e.g. `"run"` or `"inspect deps"` (canonical names
    /// from the clap model).
    command: &'a str,
    /// Selector shape: `"label_matcher"` (two args — a label + package matcher),
    /// `"addr"` (one arg — a single address), or `"none"`. Structure only, never
    /// the actual label/address value.
    request_shape: &'a str,
    /// Names of the options that were explicitly set — derived from the clap
    /// matches (positional selectors excluded; see `request_shape`), so this
    /// stays exhaustive for every command without per-command code. Names only;
    /// never the values (an addr/label could be PII).
    flags: &'a [String],
    /// Whether the command ultimately succeeded.
    success: bool,
    /// Coarse failure class on error: `"cancelled"`, `"target_failure"`, or
    /// `"error"`. `None` on success.
    failure: Option<&'a str>,
    /// Command wall time, parse-to-exit.
    duration_ms: u64,
}

fn try_enqueue(ctx: ReportContext<'_>) -> anyhow::Result<()> {
    let stats = COLLECTOR.snapshot();
    let plugins = PLUGINS.get().cloned().unwrap_or_default();

    let dir = config_dir()?;
    // Read-only: the startup warmer (see `prewarm`) does the git/API work off the
    // hot path, so the exit path never blocks. Absent on a cold cache; lands next.
    let fingerprint = repo::cached(&dir);
    let props = build_props(&ctx, &stats, plugins, fingerprint.as_deref());

    let event = SpooledEvent {
        uuid: uuid::Uuid::new_v4().to_string(),
        distinct_id: distinct_id(&dir)?,
        event: "cli_command".to_string(),
        props,
        created_ms: hcore::events::now_unix_ms() as i64,
    };
    Spool::open(&dir.join("telemetry-spool.db"))?.enqueue(&event)
}

/// Assemble the event property bag. `$os` / `$os_version` are intentionally
/// absent — posthog-rs stamps them (via `os_info`) on the capture path.
fn build_props(
    ctx: &ReportContext<'_>,
    stats: &TelemetrySnapshot,
    plugins: Plugins,
    repo_fingerprint: Option<&str>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut props = serde_json::Map::new();
    let mut put = |k: &str, v: serde_json::Value| drop(props.insert(k.to_string(), v));
    // Personless: never create/update a person profile for this distinct_id.
    put("$process_person_profile", false.into());
    // A fresh id per invocation, distinct from the install id — lets one event
    // be correlated/deduped without identifying the machine.
    put("run_id", uuid::Uuid::new_v4().to_string().into());
    // Environment — coarse and non-identifying. `$os` / `$os_version` are
    // stamped by posthog-rs itself (via os_info) on the capture path, so we
    // only add arch here.
    put("arch", std::env::consts::ARCH.into());
    put("version", hcore::version::VERSION.into());
    // Semver segments, broken out for filtering/grouping in PostHog. Absent when
    // the version doesn't parse (rather than reporting junk).
    if let Some(v) = hcore::version::parse(hcore::version::VERSION) {
        put("version_major", v.major.into());
        put("version_minor", v.minor.into());
        put("version_patch", v.patch.into());
        if let Some(pre) = v.pre_release {
            put("version_pre_release", pre.into());
        }
        if let Some(build) = v.build {
            put("version_build", build.into());
        }
    }
    put("ci", is_ci().into());
    // Stable per-repo id (one-way hash of the git root commit) grouping events by
    // project without identifying it. Absent when not in a git repo / git is
    // unavailable / a shallow clone can't reach the root and the GitHub API
    // fallback didn't apply.
    if let Some(fp) = repo_fingerprint {
        put("repo_fingerprint", fp.into());
    }
    // Command + selector shape + the set of flags used (names only).
    put("command", ctx.command.into());
    put("request_shape", ctx.request_shape.into());
    put("success", ctx.success.into());
    put("flags", ctx.flags.into());
    if let Some(failure) = ctx.failure {
        put("failure", failure.into());
    }
    put("duration_ms", ctx.duration_ms.into());
    // The interactive-TUI decision, recorded by `should_use_tui` itself (the one
    // place that makes it). Absent for commands that never decide.
    if let Some(interactive) = stats.interactive {
        put("interactive", interactive.into());
    }
    // Query selector shape — per-node counts of the parsed matcher tree from the
    // real query parser, never the expression text. Absent when no `--expr` ran.
    if let Some(q) = stats.query_expr {
        put("query_expr_addr", q.addr.into());
        put("query_expr_label", q.label.into());
        put("query_expr_package", q.package.into());
        put("query_expr_package_prefix", q.package_prefix.into());
        put("query_expr_tree_output", q.tree_output.into());
        put("query_expr_and", q.and.into());
        put("query_expr_or", q.or.into());
        put("query_expr_not", q.not.into());
    }
    // Number of config profiles layered via `HEPH_PROFILES` (count only — names
    // can be project-specific). Comma-separated, empty entries dropped, matching
    // the config loader.
    let profile_count = std::env::var("HEPH_PROFILES")
        .ok()
        .map(|v| v.split(',').filter(|s| !s.trim().is_empty()).count())
        .unwrap_or(0);
    put("config_profile_count", profile_count.into());
    // Aggregate run counters.
    put("result_targets", stats.targets.into());
    put("local_cache_hits", stats.local_cache_hits.into());
    put("local_cache_misses", stats.local_cache_misses.into());
    put("executes", stats.executes.into());
    // Approx p99 execute wall time — only when something actually executed.
    if stats.executes > 0 {
        put("p99_execute_ms", stats.p99_execute_ms.into());
    }
    put("artifact_count", stats.artifacts.into());
    put("total_artifact_size_bytes", stats.artifact_bytes.into());
    // Size distribution — only present when at least one artifact had a known
    // size, so max/p99 are never a misleading 0.
    if stats.sized_artifacts > 0 {
        put("max_artifact_size_bytes", stats.max_artifact_bytes.into());
        put("p99_artifact_size_bytes", stats.p99_artifact_bytes.into());
    }
    // Whole-graph (`//...`) query size — only present when one actually ran, so
    // the attr is absent rather than a misleading 0 on every other invocation.
    if stats.graph_size > 0 {
        put("graph_size", stats.graph_size.into());
    }
    // Remote (shared) cache outcomes this run.
    put("remote_cache_hits", stats.remote_cache_hits.into());
    put("remote_cache_misses", stats.remote_cache_misses.into());
    put("remote_cache_writes", stats.remote_cache_writes.into());
    // Approval gate: how many targets prompted, and how those prompts resolved.
    put("approvals_requested", stats.approvals_requested.into());
    put("approvals_granted", stats.approvals_granted.into());
    put("approvals_denied", stats.approvals_denied.into());
    // Interactive sandbox shell sessions actually entered.
    put("shell_sessions", stats.shell_sessions.into());
    // Per-execute sandbox backend split: FUSE mount vs unpack-copy.
    put("sandbox_fuse", stats.sandbox_fuse.into());
    put("sandbox_os", stats.sandbox_os.into());
    // Local-cache blobs that spilled from the primary store onto the FS store.
    put("cache_spills", stats.cache_spills.into());
    // Enabled plugins (built-ins + config), as queryable arrays of type names.
    put("providers", plugins.providers.into());
    put("drivers", plugins.drivers.into());
    // Remote (shared) cache shape: how many are configured and which backend
    // kinds they use (scheme only — never URIs).
    put("remote_cache_count", plugins.remote_cache_count.into());
    put(
        "remote_cache_backends",
        plugins.remote_cache_backends.into(),
    );

    props
}

/// Flush the spool synchronously, blocking until sent (or failed). Used on CI,
/// where the runner — and its spool — is ephemeral: a deferred flush would
/// never happen, so the exit pays the full POST to get the data out at all.
fn flush_blocking() {
    if let Err(e) = flush_once() {
        tracing::debug!(error = %format!("{e:#}"), "telemetry flush skipped");
    }
}

/// Try to flush the spool (this run's event plus any backlog) on a worker
/// thread, but wait at most `deadline` for it. Called at exit, so the budget is
/// measured from after the command's real work is done. If the send doesn't
/// finish in time we return and let the process exit — the thread is abandoned
/// mid-flight, rows are deleted only after a successful send, so the unsent rows
/// simply retry on the next run (per-event UUIDs dedupe any row that did reach
/// PostHog before we gave up).
fn flush_bounded(deadline: Duration) {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("telemetry-flush".to_string())
        .spawn(move || drop(tx.send(flush_once())));
    if let Err(e) = spawned {
        tracing::debug!(error = %e, "telemetry flush thread not started");
        return;
    }
    match rx.recv_timeout(deadline) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::debug!(error = %format!("{e:#}"), "telemetry flush skipped"),
        Err(_) => tracing::debug!("telemetry flush exceeded deadline; deferring to next run"),
    }
}

fn flush_once() -> anyhow::Result<()> {
    let dir = config_dir()?;
    let spool = Spool::open(&dir.join("telemetry-spool.db"))?;
    let batch = spool.take_batch(FLUSH_BATCH)?;
    if batch.is_empty() {
        return Ok(());
    }

    let mut ids = Vec::with_capacity(batch.len());
    let mut events = Vec::with_capacity(batch.len());
    for (id, row) in batch {
        ids.push(id);
        events.push(rebuild_event(row)?);
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("building telemetry runtime: {e}"))?;
    rt.block_on(async {
        let options = ClientOptionsBuilder::default()
            .api_key(POSTHOG_API_KEY.to_string())
            .host(POSTHOG_HOST)
            .request_timeout_seconds(10)
            // We never want PostHog inferring location from the request IP.
            .disable_geoip(true)
            .build()
            .map_err(|e| anyhow::anyhow!("building telemetry client options: {e}"))?;
        let client = posthog_rs::client(options).await;
        client
            .capture_batch(events, false)
            .await
            .map_err(|e| anyhow::anyhow!("telemetry batch capture failed: {e}"))
    })?;

    // Send succeeded: drop the rows. A crash between send and delete re-sends
    // next run; the per-event UUID dedupes server-side.
    spool.delete(&ids)
}

/// Turn a spooled row back into a PostHog event, stamped with its original
/// capture time and stable UUID.
fn rebuild_event(row: SpooledEvent) -> anyhow::Result<Event> {
    let mut event = Event::new(row.event.as_str(), row.distinct_id.as_str());
    let uuid = uuid::Uuid::parse_str(&row.uuid)
        .map_err(|e| anyhow::anyhow!("parsing spooled event uuid: {e}"))?;
    event.set_uuid(uuid);
    if let Some(ts) = chrono::DateTime::from_timestamp_millis(row.created_ms) {
        event
            .set_timestamp(ts)
            .map_err(|e| anyhow::anyhow!("stamping spooled event timestamp: {e}"))?;
    }
    for (k, v) in row.props {
        event
            .insert_prop(k, v)
            .map_err(|e| anyhow::anyhow!("setting spooled event prop: {e}"))?;
    }
    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_kill_switch_overrides_enabled() {
        // SAFETY: single-threaded test; no other thread reads the env here.
        unsafe { std::env::set_var(DISABLE_ENV, "1") };
        assert!(!is_enabled(true));
        // SAFETY: as above.
        unsafe { std::env::remove_var(DISABLE_ENV) };
        assert!(is_enabled(true));
        // Config opt-out alone also disables.
        assert!(!is_enabled(false));
    }

    #[test]
    fn ci_detection_rules() {
        // Nothing set → not CI.
        assert!(!ci_from(|_| None));
        // Generic CI truthy.
        assert!(ci_from(|n| (n == "CI").then(|| "true".to_string())));
        assert!(ci_from(|n| (n == "CI").then(|| "1".to_string())));
        // Generic CI explicitly off.
        assert!(!ci_from(|n| (n == "CI").then(|| "false".to_string())));
        assert!(!ci_from(|n| (n == "CI").then(|| "0".to_string())));
        // A vendor marker present (even empty) signals CI.
        assert!(ci_from(|n| (n == "GITHUB_ACTIONS").then(String::new)));
        assert!(ci_from(|n| (n == "BUILDKITE").then(|| "yes".to_string())));
    }

    #[test]
    fn build_props_omits_os_and_keeps_arch() {
        let snapshot = TelemetrySnapshot {
            targets: 0,
            local_cache_hits: 0,
            local_cache_misses: 0,
            artifacts: 0,
            artifact_bytes: 0,
            max_artifact_bytes: 0,
            p99_artifact_bytes: 0,
            sized_artifacts: 0,
            executes: 0,
            p99_execute_ms: 0,
            graph_size: 0,
            remote_cache_hits: 0,
            remote_cache_misses: 0,
            remote_cache_writes: 0,
            approvals_requested: 0,
            approvals_granted: 0,
            approvals_denied: 0,
            shell_sessions: 0,
            sandbox_fuse: 0,
            sandbox_os: 0,
            cache_spills: 0,
            query_expr: None,
            interactive: None,
        };
        let ctx = ReportContext {
            command: "run",
            request_shape: "addr",
            flags: &[],
            success: true,
            failure: None,
            duration_ms: 1,
        };
        let props = build_props(&ctx, &snapshot, Plugins::default(), Some("deadbeef"));

        // posthog-rs stamps `$os` / `$os_version` itself, so build_props must
        // not carry an `os` of its own (nor the `$`-prefixed keys).
        assert!(!props.contains_key("os"), "os must be left to posthog-rs");
        assert!(!props.contains_key("$os"));
        assert!(!props.contains_key("$os_version"));
        // arch is ours to report and stays.
        assert_eq!(
            props.get("arch").and_then(|v| v.as_str()),
            Some(std::env::consts::ARCH)
        );
        // The repo fingerprint is carried through when present.
        assert_eq!(
            props.get("repo_fingerprint").and_then(|v| v.as_str()),
            Some("deadbeef")
        );
        // ...and omitted entirely when it can't be determined.
        let absent = build_props(&ctx, &snapshot, Plugins::default(), None);
        assert!(!absent.contains_key("repo_fingerprint"));
    }

    #[test]
    fn split_set_args_separates_positionals_from_options() {
        use clap::{Arg, ArgAction, Command};
        let cmd = Command::new("run")
            .arg(Arg::new("arg1").index(1))
            .arg(Arg::new("force").long("force").action(ArgAction::SetTrue));
        let m = cmd
            .clone()
            .try_get_matches_from(["run", "//pkg:target", "--force"])
            .expect("parse");
        let (flags, positionals) = split_set_args(&m, &cmd);
        // The positional selector must NOT bleed into `flags` — that was the bug.
        assert_eq!(flags, vec!["force".to_string()]);
        assert_eq!(positionals, vec!["arg1".to_string()]);
    }

    #[test]
    fn split_set_args_omits_unset_args() {
        use clap::{Arg, ArgAction, Command};
        let cmd = Command::new("run")
            .arg(Arg::new("arg1").index(1))
            .arg(Arg::new("force").long("force").action(ArgAction::SetTrue));
        // Nothing on the command line → both sides empty (defaults aren't "set").
        let m = cmd.clone().try_get_matches_from(["run"]).expect("parse");
        let (flags, positionals) = split_set_args(&m, &cmd);
        assert!(flags.is_empty());
        assert!(positionals.is_empty());
    }

    #[test]
    fn build_props_carries_query_expr_and_new_counters() {
        let snapshot = TelemetrySnapshot {
            targets: 0,
            local_cache_hits: 0,
            local_cache_misses: 0,
            artifacts: 0,
            artifact_bytes: 0,
            max_artifact_bytes: 0,
            p99_artifact_bytes: 0,
            sized_artifacts: 0,
            executes: 0,
            p99_execute_ms: 0,
            graph_size: 0,
            remote_cache_hits: 2,
            remote_cache_misses: 3,
            remote_cache_writes: 1,
            approvals_requested: 4,
            approvals_granted: 3,
            approvals_denied: 1,
            shell_sessions: 5,
            sandbox_fuse: 6,
            sandbox_os: 7,
            cache_spills: 8,
            query_expr: Some(QueryExprCounts {
                addr: 1,
                label: 2,
                package: 0,
                package_prefix: 1,
                tree_output: 0,
                and: 1,
                or: 0,
                not: 1,
            }),
            interactive: Some(true),
        };
        let ctx = ReportContext {
            command: "run",
            request_shape: "addr",
            flags: &[],
            success: true,
            failure: None,
            duration_ms: 1,
        };
        let props = build_props(&ctx, &snapshot, Plugins::default(), None);
        let n = |k: &str| props.get(k).and_then(|v| v.as_u64());
        assert_eq!(n("query_expr_addr"), Some(1));
        assert_eq!(n("query_expr_label"), Some(2));
        assert_eq!(n("query_expr_package"), Some(0));
        assert_eq!(n("query_expr_package_prefix"), Some(1));
        assert_eq!(n("query_expr_and"), Some(1));
        assert_eq!(n("query_expr_not"), Some(1));
        assert_eq!(n("remote_cache_hits"), Some(2));
        assert_eq!(n("remote_cache_misses"), Some(3));
        assert_eq!(n("remote_cache_writes"), Some(1));
        assert_eq!(n("approvals_requested"), Some(4));
        assert_eq!(n("approvals_granted"), Some(3));
        assert_eq!(n("approvals_denied"), Some(1));
        assert_eq!(n("shell_sessions"), Some(5));
        assert_eq!(n("sandbox_fuse"), Some(6));
        assert_eq!(n("sandbox_os"), Some(7));
        assert_eq!(n("cache_spills"), Some(8));
        assert_eq!(
            props.get("interactive").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert!(props.contains_key("config_profile_count"));

        // No `--expr` and no TUI decision → those attrs are absent, not zero.
        let bare = TelemetrySnapshot {
            query_expr: None,
            interactive: None,
            ..snapshot
        };
        let props = build_props(&ctx, &bare, Plugins::default(), None);
        assert!(!props.contains_key("query_expr_addr"));
        assert!(!props.contains_key("query_expr_label"));
        assert!(!props.contains_key("interactive"));
    }

    #[test]
    fn install_id_is_stable_and_random() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = install_id(dir.path()).expect("mint");
        // Valid UUID, persisted, and stable across reads.
        uuid::Uuid::parse_str(&first).expect("uuid");
        assert_eq!(install_id(dir.path()).expect("reread"), first);

        // A different dir mints a different id.
        let other = tempfile::tempdir().expect("tempdir");
        assert_ne!(install_id(other.path()).expect("mint"), first);
    }

    #[test]
    fn install_id_remints_on_empty_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("telemetry-id"), "  \n").expect("write");
        let id = install_id(dir.path()).expect("mint");
        uuid::Uuid::parse_str(&id).expect("uuid");
    }

    /// Live end-to-end flush against PostHog EU. Network — excluded from normal
    /// runs; exercise manually with `cargo test telemetry -- --ignored`.
    #[test]
    #[ignore = "live network test — run manually"]
    fn flush_live_drains_spool() {
        let dir = tempfile::tempdir().expect("tempdir");
        // SAFETY: single-threaded #[ignore]d test, run manually in isolation.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", dir.path()) };

        let cfg = config_dir().expect("config dir");
        let spool = Spool::open(&cfg.join("telemetry-spool.db")).expect("open");
        let mut props = serde_json::Map::new();
        props.insert("$process_person_profile".into(), false.into());
        props.insert("command".into(), "live_test".into());
        spool
            .enqueue(&SpooledEvent {
                uuid: uuid::Uuid::new_v4().to_string(),
                distinct_id: "live-test".into(),
                event: "cli_command".into(),
                props,
                created_ms: hcore::events::now_unix_ms() as i64,
            })
            .expect("enqueue");
        drop(spool);

        flush_once().expect("flush");

        let spool = Spool::open(&cfg.join("telemetry-spool.db")).expect("reopen");
        assert!(
            spool.take_batch(10).expect("take").is_empty(),
            "flush must delete sent rows"
        );
    }

    #[test]
    fn rebuild_event_roundtrips() {
        let mut props = serde_json::Map::new();
        props.insert("command".into(), "run".into());
        props.insert("targets".into(), 42.into());
        let row = SpooledEvent {
            uuid: uuid::Uuid::new_v4().to_string(),
            distinct_id: "install-1".into(),
            event: "cli_command".into(),
            props,
            created_ms: 1_700_000_000_000,
        };
        // Must rebuild without error; a malformed uuid must fail.
        rebuild_event(row.clone()).expect("rebuild");
        let bad = SpooledEvent {
            uuid: "not-a-uuid".into(),
            ..row
        };
        assert!(rebuild_event(bad).is_err());
    }
}
