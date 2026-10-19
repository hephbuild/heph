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
//! At exit the event is appended to a local SQLite spool (one insert) and the
//! command returns — it never waits on the network. When a flush is due (the
//! oldest spooled event is older than [`FLUSH_MAX_AGE`], or the spool holds
//! [`FLUSH_AT_EVENTS`]; see `flush_due`), the command spawns a detached
//! `heph __telemetry-flush` and does not wait for it. On CI the flush runs
//! synchronously instead: a CI runner tears down its process tree with the job
//! (a detached flusher would be killed mid-send) and its spool with the machine
//! (there is no next run to pick up the backlog), so CI pays the POST to get the
//! data out at all. See [`spool`] for the flush protocol. Every flush is
//! bounded in time: [`FLUSH_DEADLINE`] for the detached flusher (with a
//! watchdog behind it), [`CI_FLUSH_BUDGET`] for CI's synchronous one.
//!
//! Disabled by `telemetry.enabled: false` in `.hephconfig2`, or by setting the
//! `HEPH_DISABLE_TELEMETRY` environment variable (the latter also keeps the
//! project's own test/CI binary invocations from reporting). An opted-out
//! command records nothing; if a spool is left from before the opt-out — events
//! recorded while the user consented — it sends it once and deletes it
//! ([`flush_on_opt_out`]), through the same detached flusher and bounds (CI's
//! synchronous one on CI). One attempt: sent or not, the spool is gone after
//! it. Everything is strictly best-effort: any failure —
//! offline, timeout, disabled — is swallowed and never affects the command's
//! exit. `docs/TELEMETRY.md` is the user-facing account of all of this.

mod collector;
mod repo;
mod spool;

pub use collector::{QueryExprCounts, TelemetryCollector, TelemetrySnapshot};

use anyhow::Context as _;
use clap::ArgMatches;
use clap::parser::ValueSource;
use hcore::events::BuildEventKind;
use posthog_rs::{ClientOptionsBuilder, Event};
use spool::{FLUSH_BATCH, MAX_ROWS, Spool, SpoolState, SpooledEvent};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The hidden `argv[1]` that runs a process as the detached flusher
/// ([`flush_main`]). Dispatched in `main` before logging, clap or the
/// supervisor, like the other `__` helpers. Its arguments are
/// `[--opt-out] [<root>]`: [`OPT_OUT_FLAG`] for the final flush of an opted-out
/// command, and the workspace root of the command that spawned it, whose
/// config the flusher re-checks for an opt-out.
pub const FLUSH_SUBCOMMAND: &str = "__telemetry-flush";

/// The flusher argument that makes it the final flush of an opted-out
/// command: send the spool once, then delete it whatever happened.
const OPT_OUT_FLAG: &str = "--opt-out";

/// How long the detached flusher may spend sending before it gives up, rows
/// unsent staying spooled for the next one. A send in flight at the deadline
/// is abandoned, not waited for.
pub const FLUSH_DEADLINE: Duration = Duration::from_secs(30);

/// The detached flusher's hard backstop: past this, a watchdog thread exits
/// the process whatever it is doing (a SQLite busy wait, a send that ignored
/// its deadline). Comfortably past [`FLUSH_DEADLINE`], so it only fires when
/// the deadline itself failed to.
const FLUSH_WATCHDOG: Duration = Duration::from_secs(35);

/// Everything CI's synchronous flush may take — waiting for the flusher lock
/// and every send together. The flush it replaced was a single request with a
/// 10 s timeout; this keeps CI's worst case there.
pub const CI_FLUSH_BUDGET: Duration = Duration::from_secs(10);

/// How often a flush waiting for the flusher lock ([`LockMode::Wait`]) retries.
const LOCK_POLL: Duration = Duration::from_millis(25);

/// The longest last-flush error kept in the spool, in characters.
const MAX_FLUSH_ERROR_CHARS: usize = 500;

/// Env var that points the flush at another ingestion endpoint instead of
/// PostHog EU. **Loopback only** (`http(s)://127.0.0.1`, `[::1]` or
/// `localhost`, any port and path): it exists so a test or a benchmark can
/// stand up a local sink and exercise the real send path without touching
/// production. It is not a way to ship the events anywhere else, so it cannot
/// be used to redirect them off the machine. A value that is not a loopback
/// URL disables sending altogether (the flush fails and records why) rather
/// than falling back to PostHog. `heph tool telemetry status` shows when it
/// is set.
pub const ENDPOINT_ENV: &str = "HEPH_TELEMETRY_ENDPOINT";

/// A flush is due once the oldest spooled event is this old. Short enough that
/// an occasional user's events arrive the same session; long enough that a run
/// of commands shares one flusher instead of spawning one each.
pub const FLUSH_MAX_AGE: Duration = Duration::from_secs(5 * 60);

/// A flush is due once the spool holds this many events, whatever their age:
/// one full [`FLUSH_BATCH`], so a flusher spawned for size sends one request.
/// It sits at a quarter of the spool cap ([`MAX_ROWS`]), so a burst of
/// commands — a script calling heph in a loop — has the flusher spawned with
/// three batches of headroom before the cap drops anything.
pub const FLUSH_AT_EVENTS: u64 = FLUSH_BATCH as u64;

/// Minimum gap between two flusher spawns. A flusher that cannot reach
/// PostHog leaves the spool due, and without this every command run offline
/// would spawn another one.
pub const FLUSH_RETRY_INTERVAL: Duration = Duration::from_secs(60);

/// The spool file, in [`config_dir`].
const SPOOL_FILE: &str = "telemetry-spool.db";
/// The flusher's exclusive lock, in [`config_dir`].
const FLUSH_LOCK_FILE: &str = "telemetry-flush.lock";

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

/// Record a cache hit whose bytes turned out to be unreadable, so the target was
/// rebuilt. Not derivable from the event stream: the collector holds no per-addr
/// state, so it cannot tell that an `ExecuteStart` belongs to an addr it already
/// counted as a hit.
pub fn record_cache_hit_rebuilt() {
    COLLECTOR.record_cache_hit_rebuilt();
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
/// *not* the per-repo `.heph` home — the install id must survive cache nukes
/// and be shared across repos.
fn config_dir() -> anyhow::Result<PathBuf> {
    let dir = config_dir_path()?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("creating config dir {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Where [`config_dir`] is, without creating it — for the paths that must not
/// leave anything on disk (an opt-out, the status command, the flusher).
fn config_dir_path() -> anyhow::Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok_or_else(|| anyhow::anyhow!("no XDG_CONFIG_HOME or HOME"))?;
    Ok(base.join("heph"))
}

/// Opted out: send what was spooled before the opt-out — recorded while the
/// user consented — once, then delete the spool. Called by every command that
/// runs with telemetry disabled (either switch); records nothing itself. With
/// no spool on disk (the steady state once opted out) it is one `stat` and
/// creates nothing.
///
/// Off CI the send is handed to a detached `heph __telemetry-flush --opt-out`
/// and never waited on; on CI, where a detached process dies with the job, it
/// runs inline within [`CI_FLUSH_BUDGET`]. Either way the spool is deleted
/// after that one attempt, sent or not ([`run_final_flush`]). The spool is
/// machine-wide, so an opt-out in one workspace also ends another, still
/// opted-in workspace's spool (after sending it).
pub fn flush_on_opt_out() {
    let outcome = config_dir_path().and_then(|dir| {
        hand_off_opt_out(
            &dir,
            is_ci(),
            hcore::events::now_unix_ms() as i64,
            || spawn_flusher(FlusherArgs::opt_out()),
            production_sender,
        )
    });
    match outcome {
        Ok(OptOutHandOff::NoSpool) => {}
        Ok(done) => tracing::debug!(?done, "telemetry disabled: spool handed off"),
        Err(e) => tracing::debug!(error = %format!("{e:#}"), "telemetry opt-out flush failed"),
    }
}

/// What [`hand_off_opt_out`] did.
#[derive(Debug, PartialEq, Eq)]
enum OptOutHandOff {
    /// No spool on disk; nothing to do.
    NoSpool,
    /// The spool held no events; deleted without sending.
    Removed,
    /// A flusher was spawned in the last [`FLUSH_RETRY_INTERVAL`] (or another
    /// command just claimed this hand-off); it is left to finish first.
    Pending,
    /// The opt-out flusher was spawned.
    Spawned,
    /// CI: flushed inline and deleted.
    Flushed(FlushOutcome),
}

/// [`flush_on_opt_out`] over the config dir, with the spawn and the sender
/// injected.
fn hand_off_opt_out(
    dir: &Path,
    ci: bool,
    now_ms: i64,
    spawn: impl FnOnce() -> anyhow::Result<()>,
    sender: impl FnOnce() -> anyhow::Result<Sender>,
) -> anyhow::Result<OptOutHandOff> {
    let path = dir.join(SPOOL_FILE);
    if !path.exists() {
        return Ok(OptOutHandOff::NoSpool);
    }
    if ci {
        let deadline = Instant::now() + CI_FLUSH_BUDGET;
        return run_final_flush(dir, deadline, sender).map(OptOutHandOff::Flushed);
    }
    // A spool that cannot be read cannot be sent either; it is deleted rather
    // than left for every later command to trip over.
    let opened = Spool::open(&path).and_then(|spool| spool.state().map(|state| (spool, state)));
    let (spool, state) = match opened {
        Ok(opened) => opened,
        Err(e) => {
            spool::remove(&path)?;
            return Err(e.context("unreadable telemetry spool deleted"));
        }
    };
    if state.count == 0 {
        drop(spool);
        spool::remove(&path)?;
        return Ok(OptOutHandOff::Removed);
    }
    // One flusher at a time, as for a due flush: a recent spawn (a normal
    // flusher, or an opt-out one still within its deadline) holds this off,
    // and of two opted-out commands only the one that claims the spawn starts
    // a flusher.
    let spawned_recently = state.last_spawn_ms.is_some_and(|last| {
        (0..millis(FLUSH_RETRY_INTERVAL)).contains(&now_ms.saturating_sub(last))
    });
    if spawned_recently || !spool.claim_spawn(state.last_spawn_ms, now_ms)? {
        return Ok(OptOutHandOff::Pending);
    }
    drop(spool);
    spawn()?;
    Ok(OptOutHandOff::Spawned)
}

/// The final flush of an opted-out spool: send it once, within `deadline`,
/// then delete it — whether the send succeeded, failed, or never started (no
/// sender, the flusher lock held past the deadline). Nothing is recorded: the
/// outcome would recreate the spool. No spool on disk: does nothing and
/// creates nothing.
///
/// The spool is deleted while the flusher lock is held (when it could be
/// taken), so a normal flusher never drains a spool mid-delete. Returns the
/// send's error, if any, once the spool is gone.
fn run_final_flush(
    dir: &Path,
    deadline: Instant,
    sender: impl FnOnce() -> anyhow::Result<Sender>,
) -> anyhow::Result<FlushOutcome> {
    let path = dir.join(SPOOL_FILE);
    if !path.exists() {
        return Ok(FlushOutcome::Sent(0));
    }
    let lock = lock_flusher(dir, LockMode::Wait, deadline);
    let sent = match &lock {
        Ok(Some(_)) => sender()
            .and_then(|send| drain(dir, deadline, send))
            .map(FlushOutcome::Sent),
        Ok(None) => Ok(FlushOutcome::Busy),
        Err(e) => Err(anyhow::anyhow!("{e:#}")),
    };
    let removed = spool::remove(&path);
    drop(lock);
    let sent = sent?;
    removed?;
    Ok(sent)
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

/// Record this invocation: build the event from the clap matches + command
/// result and spool it (one SQLite insert). Then, off CI, spawn the detached
/// flusher if a flush is due (`flush_due`) and return without waiting; on CI,
/// send synchronously (see the module docs). The single entry point the CLI
/// calls at exit; everything below is best-effort and never affects the
/// command's outcome.
///
/// `root` is the command's workspace root, handed to the flusher so it can
/// re-check that workspace's config opt-out before sending.
pub fn record_invocation(
    matches: &ArgMatches,
    cmd: &clap::Command,
    error: Option<&anyhow::Error>,
    took: Duration,
    root: Option<&Path>,
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

    let spool = match try_enqueue(ReportContext {
        command: &command,
        request_shape,
        flags: &flags,
        success: failure.is_none(),
        failure,
        duration_ms: took.as_millis() as u64,
    }) {
        Ok(spool) => Some(spool),
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), "telemetry enqueue skipped");
            None
        }
    };

    // CI: send now, blocking (see the module docs). Elsewhere: hand off to a
    // detached flusher when one is due, and never wait.
    if is_ci() {
        drop(spool);
        flush_blocking();
    } else if let Some(spool) = spool {
        match claim_flush(&spool, hcore::events::now_unix_ms() as i64) {
            Ok(true) => {
                let args = FlusherArgs {
                    opt_out: false,
                    root: root.map(Path::to_path_buf),
                };
                if let Err(e) = spawn_flusher(args) {
                    tracing::debug!(error = %format!("{e:#}"), "telemetry flusher not spawned");
                }
            }
            Ok(false) => {}
            Err(e) => tracing::debug!(error = %format!("{e:#}"), "telemetry flush check failed"),
        }
    }
}

/// Whether a flush is due for a spool in state `s` at `now_ms`: there is
/// something to send, no flusher was spawned in the last
/// [`FLUSH_RETRY_INTERVAL`], and either the oldest event is at least
/// [`FLUSH_MAX_AGE`] old or [`FLUSH_AT_EVENTS`] are waiting.
///
/// A last spawn recorded in the *future* (the clock stepped back) does not hold
/// flushes off: it would otherwise suppress them until the clock caught up.
fn flush_due(s: SpoolState, now_ms: i64) -> bool {
    if s.count == 0 {
        return false;
    }
    let spawned_recently = s.last_spawn_ms.is_some_and(|last| {
        (0..millis(FLUSH_RETRY_INTERVAL)).contains(&now_ms.saturating_sub(last))
    });
    if spawned_recently {
        return false;
    }
    s.count >= FLUSH_AT_EVENTS
        || s.oldest_ms
            .is_some_and(|oldest| now_ms.saturating_sub(oldest) >= millis(FLUSH_MAX_AGE))
}

/// The earliest time (ms since epoch) at which [`flush_due`] holds for a
/// spool in state `s` if nothing more is spooled; `None` when it is empty. A
/// time at or before `now_ms` means the next command spawns a flusher.
fn next_flush_due(s: SpoolState, now_ms: i64) -> Option<i64> {
    if s.count == 0 {
        return None;
    }
    let oldest = s.oldest_ms?;
    let ready = if s.count >= FLUSH_AT_EVENTS {
        oldest
    } else {
        oldest.saturating_add(millis(FLUSH_MAX_AGE))
    };
    // A spawn stamp from the future does not hold flushes off (see
    // `flush_due`), so it does not delay the answer either.
    let retry = s
        .last_spawn_ms
        .filter(|&last| last <= now_ms)
        .map(|last| last.saturating_add(millis(FLUSH_RETRY_INTERVAL)));
    Some(retry.map_or(ready, |r| ready.max(r)))
}

/// `d` in whole milliseconds, saturating.
fn millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// Decide whether this command should spawn the flusher, and if so claim the
/// spawn so a concurrent command that reached the same decision does not spawn
/// a second one.
fn claim_flush(spool: &Spool, now_ms: i64) -> anyhow::Result<bool> {
    let state = spool.state()?;
    if !flush_due(state, now_ms) {
        return Ok(false);
    }
    spool.claim_spawn(state.last_spawn_ms, now_ms)
}

/// Start `heph __telemetry-flush` detached, and do not wait for it.
///
/// Detached in every sense: its own session (`setsid` — no controlling
/// terminal, so closing the terminal does not SIGHUP it, and not in heph's
/// process group, so a Ctrl-C to heph does not reach it), stdio on `/dev/null`,
/// cwd `/` (it must not pin the workspace), and never waited on. It is
/// deliberately *not* spawned through `proc_exec`: that registers every child
/// with the process supervisor, which kills them when heph exits — and this one
/// exists to outlive heph. Nor through `hexecrunner`: that seam is for a
/// target's command and the runner its cache key names; this is heph re-running
/// itself, like the supervisor sidecar.
fn spawn_flusher(args: FlusherArgs) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("locating heph")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg(FLUSH_SUBCOMMAND);
    if args.opt_out {
        cmd.arg(OPT_OUT_FLAG);
    }
    if let Some(root) = args.root {
        cmd.arg(root);
    }
    // The `Child` is dropped un-waited: the flusher outlives this process and
    // is reparented once heph exits, and reaped by whatever it is reparented
    // to (init, or a subreaper). A container whose PID 1 never reaps keeps it
    // as a zombie until the container exits; see docs/TELEMETRY.md.
    drop(spawn_detached(cmd)?);
    Ok(())
}

/// Spawn `cmd` detached as [`spawn_flusher`] describes — its own session,
/// stdio on `/dev/null`, cwd `/` — and return without waiting. The program is
/// the caller's, so a test can stand in for heph.
fn spawn_detached(mut cmd: std::process::Command) -> anyhow::Result<std::process::Child> {
    use std::os::unix::process::CommandExt as _;
    use std::process::Stdio;

    cmd.current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "pre_exec installer + the setsid call it installs"
    )]
    // SAFETY: the closure runs between fork and exec and calls only setsid(2),
    // which is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn().context("spawning telemetry flusher")
}

/// The flusher's arguments, after [`FLUSH_SUBCOMMAND`]: `[--opt-out] [<root>]`.
#[derive(Debug, Default, PartialEq, Eq)]
struct FlusherArgs {
    /// Spawned by an opted-out command ([`OPT_OUT_FLAG`]): the final flush.
    opt_out: bool,
    /// The spawning command's workspace root.
    root: Option<PathBuf>,
}

impl FlusherArgs {
    fn opt_out() -> Self {
        Self {
            opt_out: true,
            root: None,
        }
    }

    /// Parse the arguments after [`FLUSH_SUBCOMMAND`]. Lenient: the flusher has
    /// no way to report a usage error, so anything past the root is ignored.
    fn parse(mut args: impl Iterator<Item = std::ffi::OsString>) -> Self {
        let mut first = args.next();
        let opt_out = first.as_deref() == Some(std::ffi::OsStr::new(OPT_OUT_FLAG));
        if opt_out {
            first = args.next();
        }
        Self {
            opt_out,
            root: first.map(PathBuf::from),
        }
    }
}

/// Entry point of `heph __telemetry-flush`: send the spool and exit 0, whatever
/// happens.
///
/// Normally it sends what it can and leaves the rest spooled for the next
/// flusher. Opted out — spawned with [`OPT_OUT_FLAG`] by an opted-out command,
/// or finding an opt-out in force when it starts (the `HEPH_DISABLE_TELEMETRY`
/// kill switch, or the config opt-out of the workspace whose command spawned
/// it, checked with `config_enabled`) — it is the final flush instead: one
/// attempt, then the spool is deleted ([`run_final_flush`]). The events were
/// recorded while the user consented, so they are sent; nothing is kept.
///
/// Bounded twice: sending stops at [`FLUSH_DEADLINE`], and a watchdog thread
/// exits the process at [`FLUSH_WATCHDOG`] whatever it is doing — deleting the
/// spool first, on a final flush.
pub fn flush_main(config_enabled: impl FnOnce(&Path) -> bool) -> ! {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let args = FlusherArgs::parse(std::env::args_os().skip(2));
    let env_disabled = std::env::var_os(DISABLE_ENV).is_some();
    let final_flush = Arc::new(AtomicBool::new(args.opt_out || env_disabled));
    let watchdog_final = Arc::clone(&final_flush);
    spawn_watchdog(FLUSH_WATCHDOG, move || {
        if let Ok(dir) = config_dir_path() {
            if watchdog_final.load(Ordering::SeqCst) {
                // The one attempt is over; nothing may stay behind.
                drop(spool::remove(&dir.join(SPOOL_FILE)));
            } else {
                // Best-effort: say why this flush never recorded an outcome.
                // The spool's own busy timeout bounds this write.
                record_outcome(
                    &dir,
                    &Err(anyhow::anyhow!(
                        "the flusher was stopped by its {}s watchdog",
                        FLUSH_WATCHDOG.as_secs()
                    )),
                );
            }
        }
        std::process::exit(0)
    });
    let opted_out = final_flush.load(Ordering::SeqCst)
        || args.root.as_deref().is_some_and(|r| !config_enabled(r));
    final_flush.store(opted_out, Ordering::SeqCst);
    let outcome = run_flusher(
        opted_out,
        config_dir_path,
        production_sender,
        Instant::now() + FLUSH_DEADLINE,
    );
    if let Err(e) = outcome {
        tracing::debug!(error = %format!("{e:#}"), "telemetry flush failed");
    }
    std::process::exit(0)
}

/// Run `on_expire` on a detached thread once `after` has passed. The flusher's
/// backstop: `on_expire` exits the process.
fn spawn_watchdog(after: Duration, on_expire: impl FnOnce() + Send + 'static) {
    let spawned = std::thread::Builder::new()
        .name("heph-telemetry-watchdog".to_string())
        .spawn(move || {
            std::thread::sleep(after);
            on_expire();
        });
    if let Err(e) = spawned {
        tracing::debug!(error = %format!("{e:#}"), "telemetry flush watchdog not spawned");
    }
}

/// [`flush_main`] minus the process. Opted out, the final flush
/// ([`run_final_flush`]): one attempt, the spool deleted after it, nothing
/// recorded. Otherwise flushes `dir` until `deadline` and records the outcome
/// there.
fn run_flusher(
    opted_out: bool,
    dir: impl FnOnce() -> anyhow::Result<PathBuf>,
    sender: impl FnOnce() -> anyhow::Result<Sender>,
    deadline: Instant,
) -> anyhow::Result<FlushOutcome> {
    let dir = dir()?;
    if opted_out {
        return run_final_flush(&dir, deadline, sender);
    }
    let outcome = sender().and_then(|send| run_flush(&dir, LockMode::Try, deadline, send));
    record_outcome(&dir, &outcome);
    outcome
}

/// Store a finished flush's outcome in the spool, for the status command. A
/// flush that found another one running ([`FlushOutcome::Busy`]) did nothing
/// and records nothing. Best-effort.
fn record_outcome(dir: &Path, outcome: &anyhow::Result<FlushOutcome>) {
    let error = match outcome {
        Ok(FlushOutcome::Busy) => return,
        Ok(FlushOutcome::Sent(_)) => None,
        Err(e) => Some(
            format!("{e:#}")
                .chars()
                .take(MAX_FLUSH_ERROR_CHARS)
                .collect::<String>(),
        ),
    };
    let recorded = Spool::open(&dir.join(SPOOL_FILE)).and_then(|spool| {
        spool.record_flush(hcore::events::now_unix_ms() as i64, error.as_deref())
    });
    if let Err(e) = recorded {
        tracing::debug!(error = %format!("{e:#}"), "telemetry flush outcome not recorded");
    }
}

/// How a flush takes the flusher lock.
#[derive(Debug, Clone, Copy)]
enum LockMode {
    /// Give up if another flusher holds it: that one is sending these rows.
    /// The detached flusher.
    Try,
    /// Wait for it, up to the flush's deadline. CI, which must not exit with
    /// its own event unsent just because another process was mid-flush when it
    /// looked — but must not wait on that process forever either.
    Wait,
}

/// What a flush did.
#[derive(Debug, PartialEq, Eq)]
enum FlushOutcome {
    /// Another flusher holds the lock (past the deadline, for
    /// [`LockMode::Wait`]); nothing was sent.
    Busy,
    /// This many rows were sent and removed.
    Sent(usize),
}

/// Sends one batch, returning once the endpoint confirmed it. `Send +
/// 'static` because it runs on its own thread (see [`SendWorker`]).
type Sender = Box<dyn FnMut(Vec<SpooledEvent>) -> anyhow::Result<()> + Send>;

/// Take the flusher lock as `mode` says, giving up at `deadline`. `false`:
/// another flusher holds it.
fn take_lock(
    lock: &std::fs::File,
    path: &Path,
    mode: LockMode,
    deadline: Instant,
) -> anyhow::Result<bool> {
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(true),
            Err(std::fs::TryLockError::WouldBlock) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if matches!(mode, LockMode::Try) || left.is_zero() {
                    return Ok(false);
                }
                std::thread::sleep(LOCK_POLL.min(left));
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("locking {}", path.display()));
            }
        }
    }
}

/// Runs the sender on a thread of its own, so a send still in flight at the
/// deadline is abandoned rather than waited for: the flush returns, and the
/// thread finishes (or dies with the process) on its own. Its late result is
/// discarded, so an abandoned batch is never deleted — it stays spooled and
/// is sent again, under the same UUIDs, by a later flush.
struct SendWorker {
    batches: std::sync::mpsc::Sender<Vec<SpooledEvent>>,
    results: std::sync::mpsc::Receiver<anyhow::Result<()>>,
}

impl SendWorker {
    fn spawn(mut send: Sender) -> anyhow::Result<Self> {
        let (batches, batch_rx) = std::sync::mpsc::channel::<Vec<SpooledEvent>>();
        let (result_tx, results) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("heph-telemetry-send".to_string())
            .spawn(move || {
                for batch in batch_rx {
                    if result_tx.send(send(batch)).is_err() {
                        break;
                    }
                }
            })
            .context("spawning telemetry sender thread")?;
        Ok(Self { batches, results })
    }

    /// Send one batch; an error if it failed, or if `deadline` came first.
    fn send(&self, batch: Vec<SpooledEvent>, deadline: Instant) -> anyhow::Result<()> {
        use std::sync::mpsc::RecvTimeoutError;
        self.batches
            .send(batch)
            .map_err(|e| anyhow::anyhow!("telemetry sender thread is gone: {e}"))?;
        match self
            .results
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                anyhow::bail!("telemetry flush deadline exceeded mid-send")
            }
            Err(RecvTimeoutError::Disconnected) => {
                anyhow::bail!("telemetry sender thread died mid-send")
            }
        }
    }
}

/// Send everything spooled in `dir`, batch by batch, under the flusher lock —
/// the protocol in [`spool`]'s docs. Stops at the first failed send (those rows
/// stay spooled), and at `deadline`: no batch starts after it, and one in
/// flight when it passes is abandoned (see [`SendWorker`]). Bounded in rounds
/// too: the spool never holds more than [`MAX_ROWS`], so this drains it with a
/// batch to spare even while commands keep appending.
///
/// An abandoned batch's lock is released with the flush, so the next flusher
/// may send those rows while the abandoned send is still in flight; the
/// duplicate carries the same UUIDs, which PostHog dedupes on.
fn run_flush(
    dir: &Path,
    mode: LockMode,
    deadline: Instant,
    send: Sender,
) -> anyhow::Result<FlushOutcome> {
    let Some(lock) = lock_flusher(dir, mode, deadline)? else {
        return Ok(FlushOutcome::Busy);
    };
    let sent = drain(dir, deadline, send)?;
    // `lock` drops here, after the last delete committed.
    drop(lock);
    Ok(FlushOutcome::Sent(sent))
}

/// Open and take the flusher lock in `dir` as `mode` says; `None` when
/// another flusher holds it (past `deadline`, for [`LockMode::Wait`]). The
/// lock is held until the returned file drops.
fn lock_flusher(
    dir: &Path,
    mode: LockMode,
    deadline: Instant,
) -> anyhow::Result<Option<std::fs::File>> {
    let lock_path = dir.join(FLUSH_LOCK_FILE);
    let lock = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    Ok(take_lock(&lock, &lock_path, mode, deadline)?.then_some(lock))
}

/// The body of [`run_flush`], under a flusher lock the caller holds: send
/// and delete batch by batch until the spool is empty, a send fails, or
/// `deadline` passes. Returns how many rows were sent.
fn drain(dir: &Path, deadline: Instant, send: Sender) -> anyhow::Result<usize> {
    let spool = Spool::open(&dir.join(SPOOL_FILE))?;
    let rounds = usize::try_from(MAX_ROWS)
        .unwrap_or(usize::MAX)
        .div_ceil(FLUSH_BATCH)
        + 1;
    let mut sent = 0;
    let mut send = Some(send);
    let mut worker: Option<SendWorker> = None;
    for _ in 0..rounds {
        let batch = spool.take_batch(FLUSH_BATCH)?;
        if batch.is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("telemetry flush deadline exceeded after {sent} events");
        }
        // The thread starts with the first batch, so an empty spool costs none.
        if let Some(send) = send.take() {
            worker = Some(SendWorker::spawn(send)?);
        }
        let Some(worker) = worker.as_ref() else {
            anyhow::bail!("telemetry sender thread missing");
        };
        let (ids, events): (Vec<i64>, Vec<SpooledEvent>) = batch.into_iter().unzip();
        worker.send(events, deadline)?;
        // Confirmed: drop exactly these rows. A crash before this commits
        // re-sends them next time under the same UUIDs (deduped server-side).
        spool.delete(&ids)?;
        sent += ids.len();
    }
    Ok(sent)
}

/// The sender for the configured endpoint ([`endpoint`]). Fails — and nothing
/// is sent — when [`ENDPOINT_ENV`] is set to something that is not a loopback
/// URL.
fn production_sender() -> anyhow::Result<Sender> {
    let endpoint = endpoint()?;
    Ok(Box::new(posthog_sender(endpoint.url)))
}

/// Where events go: PostHog EU, or the loopback URL in [`ENDPOINT_ENV`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct Endpoint {
    url: String,
    /// Whether [`ENDPOINT_ENV`] chose it.
    overridden: bool,
}

/// The endpoint for this process's environment.
fn endpoint() -> anyhow::Result<Endpoint> {
    endpoint_from(std::env::var_os(ENDPOINT_ENV).as_deref())
}

/// [`endpoint`] over an injected [`ENDPOINT_ENV`] value. Set but invalid —
/// empty, not UTF-8, not a URL, not http(s), not loopback — is an error, never
/// a fallback to PostHog: whoever set it meant events not to go there.
fn endpoint_from(raw: Option<&std::ffi::OsStr>) -> anyhow::Result<Endpoint> {
    let Some(raw) = raw else {
        return Ok(Endpoint {
            url: POSTHOG_HOST.to_string(),
            overridden: false,
        });
    };
    let raw = raw
        .to_str()
        .with_context(|| format!("{ENDPOINT_ENV} is not valid UTF-8"))?;
    let url =
        reqwest::Url::parse(raw).with_context(|| format!("{ENDPOINT_ENV}={raw:?} is not a URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("{ENDPOINT_ENV}={raw:?} is not an http(s) URL");
    }
    let host = url
        .host_str()
        .unwrap_or_default()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if !loopback {
        anyhow::bail!(
            "{ENDPOINT_ENV}={raw:?} is not a loopback address; the override only \
             points telemetry at a local sink (127.0.0.1, [::1] or localhost)"
        );
    }
    Ok(Endpoint {
        url: url.to_string(),
        overridden: true,
    })
}

/// The production sender: one PostHog client (and the runtime it needs) for the
/// whole flush, built on the first batch.
fn posthog_sender(host: String) -> impl FnMut(Vec<SpooledEvent>) -> anyhow::Result<()> + Send {
    let mut client: Option<(tokio::runtime::Runtime, posthog_rs::Client)> = None;
    move |rows| {
        let events = rows
            .into_iter()
            .map(rebuild_event)
            .collect::<anyhow::Result<Vec<_>>>()?;
        if client.is_none() {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| anyhow::anyhow!("building telemetry runtime: {e}"))?;
            let options = ClientOptionsBuilder::default()
                .api_key(POSTHOG_API_KEY.to_string())
                .host(host.as_str())
                .request_timeout_seconds(10)
                // We never want PostHog inferring location from the request IP.
                .disable_geoip(true)
                .build()
                .map_err(|e| anyhow::anyhow!("building telemetry client options: {e}"))?;
            let c = rt.block_on(posthog_rs::client(options));
            client = Some((rt, c));
        }
        let Some((rt, c)) = client.as_ref() else {
            anyhow::bail!("telemetry client unavailable");
        };
        // `capture_batch_immediate`, not `capture_batch`: as of posthog-rs 0.21
        // the latter is fire-and-forget (returns `()`, enqueues onto a
        // background transport). We must not delete the spooled rows until the
        // send is *confirmed*, or a failed flush silently loses the batch. The
        // returned `CaptureSummary` is discarded on purpose: its
        // `not_persisted()` is only ever non-zero under the non-default
        // `capture-v1` feature, so on the v0 pipeline an `Ok` already means the
        // whole batch persisted.
        rt.block_on(c.capture_batch_immediate(events, false))
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("telemetry batch capture failed: {e}"))
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

/// Spool this invocation's event; returns the open spool for the flush check.
fn try_enqueue(ctx: ReportContext<'_>) -> anyhow::Result<Spool> {
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
    let spool = Spool::open(&dir.join(SPOOL_FILE))?;
    spool.enqueue(&event)?;
    Ok(spool)
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
    put("version", hcore::version::current().into());
    // Semver segments, broken out for filtering/grouping in PostHog. Absent when
    // the version doesn't parse (rather than reporting junk).
    if let Some(v) = hcore::version::parse(hcore::version::current()) {
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
    // Announced hits that then had to be rebuilt — a cache promising bytes it
    // could not produce. Only sent when it happened, so the common 0 costs
    // nothing on the wire.
    if stats.cache_hits_rebuilt > 0 {
        put("cache_hits_rebuilt", stats.cache_hits_rebuilt.into());
    }
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
/// where the runner — and its spool — is ephemeral: a detached flusher would be
/// killed with the job and a deferred one would never happen, so the exit pays
/// the full POST to get the data out at all. Waits for the flusher lock rather
/// than skipping, so a concurrent heph on the same runner cannot leave this
/// run's event behind — but lock wait and sends together stop at
/// [`CI_FLUSH_BUDGET`].
fn flush_blocking() {
    let deadline = Instant::now() + CI_FLUSH_BUDGET;
    let outcome = config_dir().and_then(|dir| {
        let outcome =
            production_sender().and_then(|send| run_flush(&dir, LockMode::Wait, deadline, send));
        record_outcome(&dir, &outcome);
        outcome
    });
    if let Err(e) = outcome {
        tracing::debug!(error = %format!("{e:#}"), "telemetry flush skipped");
    }
}

/// Why telemetry is off; empty when it is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DisabledBy {
    /// `HEPH_DISABLE_TELEMETRY` is set.
    Env,
    /// `telemetry.enabled: false` in the workspace config.
    Config,
}

/// How this process would deliver its events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FlushMode {
    /// Spooled, and sent by a detached `heph __telemetry-flush` once due.
    Detached,
    /// CI: every command sends synchronously at exit.
    Synchronous,
    /// Telemetry is disabled; nothing is recorded or sent.
    Off,
}

/// What `heph tool telemetry status` reports. The `--json` form is this
/// struct; its keys are a stable interface (documented in
/// `docs/TELEMETRY.md`): add fields, never rename or remove them. Times are
/// milliseconds since the Unix epoch, ages are milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Status {
    /// Whether this process would record and send events.
    pub enabled: bool,
    /// Every opt-out in force (both can be).
    pub disabled_by: Vec<DisabledBy>,
    /// Whether this looks like a CI runner.
    pub ci: bool,
    pub flush_mode: FlushMode,
    /// Where events are sent: PostHog EU, or the [`ENDPOINT_ENV`] value.
    pub endpoint: String,
    /// Whether [`ENDPOINT_ENV`] is set.
    pub endpoint_overridden: bool,
    /// Why the [`ENDPOINT_ENV`] value was rejected (nothing is sent then).
    pub endpoint_error: Option<String>,
    /// The spool file; `None` when neither `XDG_CONFIG_HOME` nor `HOME` is set.
    pub spool_path: Option<String>,
    /// Events waiting to be sent.
    pub pending_events: u64,
    pub oldest_event_ms: Option<i64>,
    pub oldest_event_age_ms: Option<i64>,
    /// When the last flush that ran to completion finished.
    pub last_flush_ms: Option<i64>,
    /// Whether that flush sent everything; `None` before the first one.
    pub last_flush_ok: Option<bool>,
    /// Why it failed.
    pub last_flush_error: Option<String>,
    /// When a command last spawned a flusher.
    pub last_flusher_spawn_ms: Option<i64>,
    /// When the next command will spawn a flusher, if nothing more is spooled;
    /// at or before `now_ms` means the next command does. `None` with nothing
    /// pending, on CI (every command flushes), and when disabled.
    pub next_flush_due_ms: Option<i64>,
    /// The clock these were computed against.
    pub now_ms: i64,
}

/// The telemetry status of this process, given the workspace config's
/// opt-in. Reads the spool; creates nothing.
pub fn status(config_enabled: bool) -> anyhow::Result<Status> {
    status_from(
        StatusInputs {
            env_disabled: std::env::var_os(DISABLE_ENV).is_some(),
            config_enabled,
            ci: is_ci(),
            now_ms: hcore::events::now_unix_ms() as i64,
        },
        std::env::var_os(ENDPOINT_ENV).as_deref(),
        config_dir_path().ok(),
    )
}

/// The process facts [`status`] reads, injectable for tests.
#[derive(Debug, Clone, Copy)]
struct StatusInputs {
    env_disabled: bool,
    config_enabled: bool,
    ci: bool,
    now_ms: i64,
}

fn status_from(
    inputs: StatusInputs,
    endpoint_raw: Option<&std::ffi::OsStr>,
    dir: Option<PathBuf>,
) -> anyhow::Result<Status> {
    let mut disabled_by = Vec::new();
    if inputs.env_disabled {
        disabled_by.push(DisabledBy::Env);
    }
    if !inputs.config_enabled {
        disabled_by.push(DisabledBy::Config);
    }
    let enabled = disabled_by.is_empty();
    let flush_mode = match (enabled, inputs.ci) {
        (false, _) => FlushMode::Off,
        (true, true) => FlushMode::Synchronous,
        (true, false) => FlushMode::Detached,
    };
    let (endpoint, endpoint_overridden, endpoint_error) = match endpoint_from(endpoint_raw) {
        Ok(e) => (e.url, e.overridden, None),
        Err(e) => (
            endpoint_raw
                .map(|v| v.to_string_lossy().into_owned())
                .unwrap_or_default(),
            true,
            Some(format!("{e:#}")),
        ),
    };

    let spool_path = dir.map(|d| d.join(SPOOL_FILE));
    // An absent spool is an empty one; opening it would create it.
    let (state, last_flush) = match spool_path.as_deref().filter(|p| p.exists()) {
        Some(path) => {
            let spool = Spool::open(path)?;
            (spool.state()?, spool.last_flush()?)
        }
        None => (
            SpoolState {
                count: 0,
                oldest_ms: None,
                last_spawn_ms: None,
            },
            None,
        ),
    };
    let next_flush_due_ms = match flush_mode {
        FlushMode::Detached => next_flush_due(state, inputs.now_ms),
        FlushMode::Synchronous | FlushMode::Off => None,
    };
    Ok(Status {
        enabled,
        disabled_by,
        ci: inputs.ci,
        flush_mode,
        endpoint,
        endpoint_overridden,
        endpoint_error,
        spool_path: spool_path.map(|p| p.display().to_string()),
        pending_events: state.count,
        oldest_event_ms: state.oldest_ms,
        oldest_event_age_ms: state
            .oldest_ms
            .map(|t| inputs.now_ms.saturating_sub(t).max(0)),
        last_flush_ms: last_flush.as_ref().map(|f| f.at_ms),
        last_flush_ok: last_flush.as_ref().map(|f| f.error.is_none()),
        last_flush_error: last_flush.and_then(|f| f.error),
        last_flusher_spawn_ms: state.last_spawn_ms,
        next_flush_due_ms,
        now_ms: inputs.now_ms,
    })
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
            cache_hits_rebuilt: 0,
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
            cache_hits_rebuilt: 0,
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

        let outcome = run_flush(
            &cfg,
            LockMode::Wait,
            Instant::now() + CI_FLUSH_BUDGET,
            Box::new(posthog_sender(POSTHOG_HOST.to_string())),
        )
        .expect("flush");
        assert_eq!(outcome, FlushOutcome::Sent(1));

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

#[cfg(test)]
mod flush_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const MIN: i64 = 60_000;
    const NOW: i64 = 1_800_000_000_000;

    fn state(count: u64, oldest_age_ms: i64, last_spawn_ago_ms: Option<i64>) -> SpoolState {
        SpoolState {
            count,
            oldest_ms: (count > 0).then_some(NOW - oldest_age_ms),
            last_spawn_ms: last_spawn_ago_ms.map(|ago| NOW - ago),
        }
    }

    fn ev(n: usize) -> SpooledEvent {
        SpooledEvent {
            uuid: uuid::Uuid::new_v4().to_string(),
            distinct_id: "install".into(),
            event: format!("e{n}"),
            props: serde_json::Map::new(),
            created_ms: NOW,
        }
    }

    /// [`ev`] stamped now: the PostHog client refuses events from the future,
    /// which [`NOW`] is.
    fn ev_sendable(n: usize) -> SpooledEvent {
        SpooledEvent {
            created_ms: hcore::events::now_unix_ms() as i64,
            ..ev(n)
        }
    }

    fn spool_in(dir: &Path) -> Spool {
        Spool::open(&dir.join(SPOOL_FILE)).expect("open spool")
    }

    fn remaining(dir: &Path) -> Vec<String> {
        spool_in(dir)
            .take_batch(usize::MAX)
            .expect("take")
            .into_iter()
            .map(|(_, e)| e.uuid)
            .collect()
    }

    /// A deadline no test reaches.
    fn far() -> Instant {
        Instant::now() + Duration::from_secs(600)
    }

    /// A sender that confirms every batch.
    fn ok_sender() -> Sender {
        Box::new(|_| Ok(()))
    }

    /// The thresholds: five minutes of age, or a full batch of events.
    #[test]
    fn flush_is_due_at_five_minutes_or_a_full_batch() {
        assert_eq!(FLUSH_MAX_AGE, Duration::from_secs(5 * 60));
        // Nothing spooled: never.
        assert!(!flush_due(state(0, 0, None), NOW));
        // Fresh and few: not yet.
        assert!(!flush_due(state(1, 0, None), NOW));
        assert!(!flush_due(
            state(FLUSH_AT_EVENTS - 1, 5 * MIN - 1, None),
            NOW
        ));
        // Oldest event exactly five minutes old: due.
        assert!(flush_due(state(1, 5 * MIN, None), NOW));
        // A full batch, however fresh: due.
        assert!(flush_due(state(FLUSH_AT_EVENTS, 0, None), NOW));
    }

    /// A flusher spawned less than the retry interval ago holds the next one
    /// off — an offline machine spawns one per minute, not one per command —
    /// and a spawn stamp from the future (clock stepped back) does not.
    #[test]
    fn a_recent_spawn_holds_off_the_next() {
        let retry = i64::try_from(FLUSH_RETRY_INTERVAL.as_millis()).expect("ms");
        assert!(!flush_due(state(FLUSH_AT_EVENTS, 10 * MIN, Some(0)), NOW));
        assert!(!flush_due(
            state(FLUSH_AT_EVENTS, 10 * MIN, Some(retry - 1)),
            NOW
        ));
        assert!(flush_due(
            state(FLUSH_AT_EVENTS, 10 * MIN, Some(retry)),
            NOW
        ));
        assert!(flush_due(
            state(FLUSH_AT_EVENTS, 10 * MIN, Some(-10 * MIN)),
            NOW
        ));
    }

    /// Of several commands that find a flush due at once, exactly one claims
    /// the spawn; once the retry interval has passed, the next one can.
    #[test]
    fn only_one_command_claims_a_due_flush() {
        let dir = tempfile::tempdir().expect("tempdir");
        let spool = spool_in(dir.path());
        let mut old = ev(0);
        old.created_ms = NOW - 6 * MIN;
        spool.enqueue(&old).expect("enqueue");
        let other = spool_in(dir.path());

        assert!(claim_flush(&spool, NOW).expect("claim"));
        assert!(!claim_flush(&other, NOW).expect("second claim"));
        assert!(!claim_flush(&spool, NOW + 1).expect("still recent"));
        let later = NOW + i64::try_from(FLUSH_RETRY_INTERVAL.as_millis()).expect("ms");
        assert!(claim_flush(&other, later).expect("after the interval"));
    }

    /// A due check on a fresh spool claims nothing and writes no spawn stamp.
    #[test]
    fn a_flush_that_is_not_due_claims_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let spool = spool_in(dir.path());
        spool.enqueue(&ev(0)).expect("enqueue");
        assert!(!claim_flush(&spool, NOW).expect("claim"));
        assert_eq!(spool.state().expect("state").last_spawn_ms, None);
    }

    /// One flusher at a time: while another holds the lock, a detached flusher
    /// sends nothing and leaves every row in place.
    #[test]
    fn a_held_flusher_lock_makes_the_next_flusher_back_off() {
        let dir = tempfile::tempdir().expect("tempdir");
        spool_in(dir.path()).enqueue(&ev(0)).expect("enqueue");
        let held = std::fs::File::create(dir.path().join(FLUSH_LOCK_FILE)).expect("lock file");
        held.lock().expect("hold the lock");

        let calls = Arc::new(Mutex::new(0));
        let counted = Arc::clone(&calls);
        let outcome = run_flush(
            dir.path(),
            LockMode::Try,
            far(),
            Box::new(move |_| {
                *counted.lock().expect("lock") += 1;
                Ok(())
            }),
        )
        .expect("flush");
        assert_eq!(outcome, FlushOutcome::Busy);
        assert_eq!(*calls.lock().expect("lock"), 0);
        assert_eq!(remaining(dir.path()).len(), 1);

        // Released: the next flusher runs. Retried briefly, because a `flock`
        // belongs to the open file, and a sibling test forking `git` while
        // `held` was open shares it until that child execs.
        drop(held);
        assert_eq!(flush_when_free(dir.path()), FlushOutcome::Sent(1));
    }

    /// CI's wait for the flusher lock is bounded: with the lock held past the
    /// deadline it gives up as `Busy`, about when the deadline passes, rather
    /// than waiting for the holder to finish.
    #[test]
    fn a_waiting_flush_gives_up_on_the_lock_at_its_deadline() {
        let dir = tempfile::tempdir().expect("tempdir");
        spool_in(dir.path()).enqueue(&ev(0)).expect("enqueue");
        let held = std::fs::File::create(dir.path().join(FLUSH_LOCK_FILE)).expect("lock file");
        held.lock().expect("hold the lock");

        let started = Instant::now();
        let outcome = run_flush(
            dir.path(),
            LockMode::Wait,
            started + Duration::from_millis(300),
            ok_sender(),
        )
        .expect("flush");
        let took = started.elapsed();
        assert_eq!(outcome, FlushOutcome::Busy);
        assert!(
            took >= Duration::from_millis(300),
            "gave up early: {took:?}"
        );
        assert!(
            took < Duration::from_secs(5),
            "waited past the deadline: {took:?}"
        );
        assert_eq!(remaining(dir.path()).len(), 1);
    }

    /// A send still in flight at the deadline is abandoned, not waited for:
    /// the flush returns about when the deadline passes, with an error, and
    /// the batch stays spooled for the next flush.
    #[test]
    fn a_send_that_hangs_past_the_deadline_is_abandoned() {
        let dir = tempfile::tempdir().expect("tempdir");
        for n in 0..3 {
            spool_in(dir.path()).enqueue(&ev(n)).expect("enqueue");
        }
        let started = Instant::now();
        let err = run_flush(
            dir.path(),
            LockMode::Wait,
            started + Duration::from_millis(200),
            Box::new(|_| {
                std::thread::sleep(Duration::from_secs(30));
                Ok(())
            }),
        )
        .expect_err("the deadline passed mid-send");
        let took = started.elapsed();
        assert!(
            format!("{err:#}").contains("deadline exceeded"),
            "unexpected error: {err:#}"
        );
        assert!(
            took < Duration::from_secs(5),
            "waited for the send: {took:?}"
        );
        assert_eq!(remaining(dir.path()).len(), 3, "nothing deleted");
    }

    /// A deadline already past sends nothing at all.
    #[test]
    fn a_flush_past_its_deadline_starts_no_batch() {
        let dir = tempfile::tempdir().expect("tempdir");
        spool_in(dir.path()).enqueue(&ev(0)).expect("enqueue");
        let err = run_flush(
            dir.path(),
            LockMode::Wait,
            Instant::now(),
            Box::new(|_| panic!("no batch may start after the deadline")),
        )
        .expect_err("past the deadline");
        assert!(format!("{err:#}").contains("deadline exceeded"));
        assert_eq!(remaining(dir.path()).len(), 1);
    }

    /// `run_flush` in [`LockMode::Try`], retried while the lock is transiently
    /// held by a forked-but-not-yet-exec'd child of a sibling test.
    fn flush_when_free(dir: &Path) -> FlushOutcome {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let outcome = run_flush(dir, LockMode::Try, far(), ok_sender()).expect("flush");
            if outcome != FlushOutcome::Busy || Instant::now() > deadline {
                return outcome;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The whole spool goes out in batches, each row exactly once, and rows a
    /// command appends while a batch is in flight are not deleted with it —
    /// they go out in a later batch.
    #[test]
    fn a_flush_sends_every_row_exactly_once_including_ones_appended_mid_send() {
        let dir = tempfile::tempdir().expect("tempdir");
        let spool = spool_in(dir.path());
        let mut expected = Vec::new();
        for n in 0..(FLUSH_BATCH + 7) {
            let e = ev(n);
            expected.push(e.uuid.clone());
            spool.enqueue(&e).expect("enqueue");
        }
        let late = ev(999);
        expected.push(late.uuid.clone());

        let sent = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&sent);
        let mut late = Some(late);
        let appender = spool_in(dir.path());
        let outcome = run_flush(
            dir.path(),
            LockMode::Wait,
            far(),
            Box::new(move |batch| {
                if let Some(e) = late.take() {
                    appender.enqueue(&e).expect("append mid-send");
                }
                recorded
                    .lock()
                    .expect("lock")
                    .extend(batch.into_iter().map(|e| e.uuid));
                Ok(())
            }),
        )
        .expect("flush");

        let mut sent = sent.lock().expect("lock").clone();
        assert_eq!(outcome, FlushOutcome::Sent(sent.len()));
        sent.sort();
        expected.sort();
        assert_eq!(sent, expected, "every row sent, none twice");
        assert!(remaining(dir.path()).is_empty());
    }

    /// A send that fails deletes nothing: the batch is still there for the
    /// next flush, which sends it.
    #[test]
    fn a_failed_send_loses_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        for n in 0..3 {
            spool_in(dir.path()).enqueue(&ev(n)).expect("enqueue");
        }
        let before = remaining(dir.path());
        let err = run_flush(
            dir.path(),
            LockMode::Wait,
            far(),
            Box::new(|_| anyhow::bail!("offline")),
        )
        .expect_err("send failed");
        assert!(format!("{err:#}").contains("offline"));
        assert_eq!(remaining(dir.path()), before);
        assert_eq!(
            run_flush(dir.path(), LockMode::Wait, far(), ok_sender()).expect("retry"),
            FlushOutcome::Sent(3)
        );
    }

    /// The flusher dies right after PostHog accepted a batch, before deleting
    /// it — the one window no protocol closes. Nothing is lost, and the resend
    /// carries the very same UUIDs, which is what PostHog dedupes on.
    #[test]
    fn a_crash_between_send_and_delete_resends_the_same_uuids() {
        let dir = tempfile::tempdir().expect("tempdir");
        for n in 0..3 {
            spool_in(dir.path()).enqueue(&ev(n)).expect("enqueue");
        }
        let first = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&first);
        // The send thread dies after the server accepted the batch.
        let err = run_flush(
            dir.path(),
            LockMode::Wait,
            far(),
            Box::new(move |batch| {
                recorded
                    .lock()
                    .expect("lock")
                    .extend(batch.into_iter().map(|e| e.uuid));
                panic!("the flusher dies after the server accepted the batch");
            }),
        )
        .expect_err("the sender died");
        assert!(format!("{err:#}").contains("died"), "{err:#}");

        let resent = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&resent);
        run_flush(
            dir.path(),
            LockMode::Wait,
            far(),
            Box::new(move |batch| {
                recorded
                    .lock()
                    .expect("lock")
                    .extend(batch.into_iter().map(|e| e.uuid));
                Ok(())
            }),
        )
        .expect("next flush");
        let mut first = first.lock().expect("lock").clone();
        let mut resent = resent.lock().expect("lock").clone();
        first.sort();
        resent.sort();
        assert_eq!(first.len(), 3);
        assert_eq!(resent, first, "the resend is the same events, by UUID");
        assert!(remaining(dir.path()).is_empty());
    }

    /// `run_flusher` with no opt-out, retried past a transient `Busy` (see
    /// `flush_when_free`).
    fn flusher_when_free(
        cfg: &Path,
        sender: impl Fn() -> anyhow::Result<Sender>,
    ) -> anyhow::Result<FlushOutcome> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let outcome = run_flusher(false, || Ok(cfg.to_path_buf()), &sender, far());
            if !matches!(outcome, Ok(FlushOutcome::Busy)) || Instant::now() > deadline {
                return outcome;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The spool file and its `-wal`/`-shm` sidecars are all gone.
    fn assert_spool_gone(dir: &Path) {
        let left: Vec<_> = std::fs::read_dir(dir)
            .expect("ls")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(SPOOL_FILE))
            .collect();
        assert!(left.is_empty(), "spool left behind: {left:?}");
    }

    /// A PostHog stand-in on loopback: answers every request `200` and hands
    /// each request body to the test.
    fn loopback_sink() -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind sink");
        let url = format!("http://{}", listener.local_addr().expect("sink addr"));
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                drop(stream.set_read_timeout(Some(Duration::from_secs(10))));
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                // Headers, then the body by `Content-Length`.
                let body = loop {
                    let Ok(n) = stream.read(&mut chunk) else {
                        break None;
                    };
                    if n == 0 {
                        break None;
                    }
                    buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
                    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let head =
                        String::from_utf8_lossy(buf.get(..end).unwrap_or_default()).to_lowercase();
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + len {
                        break buf.get(end + 4..end + 4 + len).map(<[u8]>::to_vec);
                    }
                };
                let Some(body) = body else { continue };
                let reply = br#"{"status":1}"#;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    reply.len()
                );
                drop(stream.write_all(head.as_bytes()));
                drop(stream.write_all(reply));
                if tx
                    .send(String::from_utf8_lossy(&body).into_owned())
                    .is_err()
                {
                    return;
                }
            }
        });
        (url, rx)
    }

    /// Opted out, the flusher sends what was spooled — through the real
    /// PostHog client, to a loopback sink — then deletes the spool, and
    /// records nothing (that would recreate it).
    #[test]
    fn an_opted_out_flusher_sends_the_spool_to_the_sink_then_deletes_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let spooled: Vec<SpooledEvent> = (0..3).map(ev_sendable).collect();
        let spool = spool_in(dir.path());
        for e in &spooled {
            spool.enqueue(e).expect("enqueue");
        }
        drop(spool);
        let (url, bodies) = loopback_sink();

        let outcome = run_flusher(
            true,
            || Ok(dir.path().to_path_buf()),
            || Ok(Box::new(posthog_sender(url)) as Sender),
            Instant::now() + FLUSH_DEADLINE,
        )
        .expect("final flush");
        assert_eq!(outcome, FlushOutcome::Sent(3));

        let body = bodies
            .recv_timeout(Duration::from_secs(10))
            .expect("the sink got no request");
        for e in &spooled {
            assert!(body.contains(&e.uuid), "{} missing from {body}", e.uuid);
        }
        assert_spool_gone(dir.path());
    }

    /// Opted out with the sink unreachable: the one send fails, and the spool
    /// is deleted anyway — no retry, nothing left on disk.
    #[test]
    fn an_opted_out_flusher_deletes_the_spool_when_the_sink_is_unreachable() {
        let dir = tempfile::tempdir().expect("tempdir");
        spool_in(dir.path())
            .enqueue(&ev_sendable(0))
            .expect("enqueue");
        // Discard: nothing listens there, so the connect fails at once.
        let err = run_flusher(
            true,
            || Ok(dir.path().to_path_buf()),
            || Ok(Box::new(posthog_sender("http://127.0.0.1:9".to_string())) as Sender),
            Instant::now() + FLUSH_DEADLINE,
        )
        .expect_err("the send fails");
        assert!(format!("{err:#}").contains("capture failed"), "{err:#}");
        assert_spool_gone(dir.path());
    }

    /// The final flush deletes the spool however its one attempt ends: no
    /// sender (an endpoint override that is not loopback), or another flusher
    /// holding the lock past the deadline.
    #[test]
    fn an_opted_out_flusher_deletes_the_spool_however_the_attempt_ends() {
        let dir = tempfile::tempdir().expect("tempdir");
        spool_in(dir.path()).enqueue(&ev(0)).expect("enqueue");
        let err = run_final_flush(dir.path(), far(), || {
            endpoint_from(Some(std::ffi::OsStr::new("https://example.com"))).map(|_| ok_sender())
        })
        .expect_err("rejected endpoint");
        assert!(format!("{err:#}").contains("loopback"), "{err:#}");
        assert_spool_gone(dir.path());

        spool_in(dir.path()).enqueue(&ev(1)).expect("enqueue");
        let held = std::fs::File::create(dir.path().join(FLUSH_LOCK_FILE)).expect("lock file");
        held.lock().expect("hold the lock");
        let outcome = run_final_flush(
            dir.path(),
            Instant::now() + Duration::from_millis(50),
            || panic!("no sender may be built without the lock"),
        )
        .expect("final flush");
        assert_eq!(outcome, FlushOutcome::Busy);
        assert_spool_gone(dir.path());
    }

    /// With no spool on disk — the steady state once opted out — the final
    /// flush builds no sender and creates nothing, not even the lock file.
    #[test]
    fn an_opted_out_flusher_with_no_spool_creates_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = run_flusher(
            true,
            || Ok(dir.path().to_path_buf()),
            || panic!("no sender may be built with nothing spooled"),
            far(),
        )
        .expect("final flush");
        assert_eq!(outcome, FlushOutcome::Sent(0));
        assert_eq!(std::fs::read_dir(dir.path()).expect("ls").count(), 0);
    }

    /// The opted-out command's side: no spool is one stat and creates
    /// nothing; an empty one is deleted without a flusher; one with events
    /// spawns exactly one opt-out flusher (a second command finds it pending);
    /// on CI it is flushed inline and deleted.
    #[test]
    fn an_opted_out_command_hands_the_spool_off_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let no_spawn = || -> anyhow::Result<()> { panic!("no flusher may be spawned") };
        let no_sender = || -> anyhow::Result<Sender> { panic!("no sender may be built") };

        let none = hand_off_opt_out(dir.path(), false, NOW, no_spawn, no_sender).expect("none");
        assert_eq!(none, OptOutHandOff::NoSpool);
        assert_eq!(std::fs::read_dir(dir.path()).expect("ls").count(), 0);

        drop(spool_in(dir.path()));
        let empty = hand_off_opt_out(dir.path(), false, NOW, no_spawn, no_sender).expect("empty");
        assert_eq!(empty, OptOutHandOff::Removed);
        assert_spool_gone(dir.path());

        spool_in(dir.path()).enqueue(&ev(0)).expect("enqueue");
        let mut spawns = 0;
        let first = hand_off_opt_out(
            dir.path(),
            false,
            NOW,
            || {
                spawns += 1;
                Ok(())
            },
            no_sender,
        )
        .expect("first");
        assert_eq!(first, OptOutHandOff::Spawned);
        assert_eq!(spawns, 1);
        let second =
            hand_off_opt_out(dir.path(), false, NOW + 1, no_spawn, no_sender).expect("second");
        assert_eq!(second, OptOutHandOff::Pending);
        // The spawned flusher owns the spool now; nothing deleted it yet.
        assert_eq!(remaining(dir.path()).len(), 1);

        let sent = Arc::new(Mutex::new(0));
        let counted = Arc::clone(&sent);
        let ci = hand_off_opt_out(dir.path(), true, NOW + 2, no_spawn, move || {
            Ok(Box::new(move |rows: Vec<SpooledEvent>| {
                *counted.lock().expect("lock") += rows.len();
                Ok(())
            }) as Sender)
        })
        .expect("ci");
        assert_eq!(ci, OptOutHandOff::Flushed(FlushOutcome::Sent(1)));
        assert_eq!(*sent.lock().expect("lock"), 1);
        assert_spool_gone(dir.path());
    }

    /// `__telemetry-flush [--opt-out] [<root>]`.
    #[test]
    fn flusher_args_parse() {
        let parse = |a: &[&str]| FlusherArgs::parse(a.iter().map(std::ffi::OsString::from));
        assert_eq!(parse(&[]), FlusherArgs::default());
        assert_eq!(
            parse(&["/ws"]),
            FlusherArgs {
                opt_out: false,
                root: Some("/ws".into())
            }
        );
        assert_eq!(parse(&["--opt-out"]), FlusherArgs::opt_out());
        assert_eq!(
            parse(&["--opt-out", "/ws"]),
            FlusherArgs {
                opt_out: true,
                root: Some("/ws".into())
            }
        );
    }

    /// Enabled, the flusher sends, and records each outcome for the status
    /// command: a failure with its reason (rows kept), then a success that
    /// clears it.
    #[test]
    fn an_enabled_flusher_sends_and_records_the_outcome() {
        let dir = tempfile::tempdir().expect("tempdir");
        spool_in(dir.path()).enqueue(&ev(0)).expect("enqueue");

        let err = flusher_when_free(dir.path(), || {
            Ok(Box::new(|_| anyhow::bail!("connection refused")))
        })
        .expect_err("send failed");
        assert!(format!("{err:#}").contains("connection refused"));
        let last = spool_in(dir.path()).last_flush().expect("read");
        assert!(
            last.as_ref()
                .and_then(|l| l.error.as_deref())
                .is_some_and(|e| e.contains("connection refused")),
            "failure recorded: {last:?}"
        );
        assert_eq!(remaining(dir.path()).len(), 1);

        let outcome = flusher_when_free(dir.path(), || Ok(ok_sender())).expect("flusher");
        assert_eq!(outcome, FlushOutcome::Sent(1));
        let last = spool_in(dir.path()).last_flush().expect("read");
        assert!(last.is_some_and(|l| l.error.is_none()), "success recorded");
    }

    /// A sender that cannot be built (an endpoint override that is not
    /// loopback) sends nothing, keeps every row, and records why.
    #[test]
    fn a_rejected_endpoint_sends_nothing_and_says_why() {
        let dir = tempfile::tempdir().expect("tempdir");
        spool_in(dir.path()).enqueue(&ev(0)).expect("enqueue");
        let err = flusher_when_free(dir.path(), || {
            endpoint_from(Some(std::ffi::OsStr::new("https://example.com"))).map(|_| ok_sender())
        })
        .expect_err("rejected endpoint");
        assert!(format!("{err:#}").contains("loopback"), "{err:#}");
        assert_eq!(remaining(dir.path()).len(), 1);
        let last = spool_in(dir.path()).last_flush().expect("read");
        assert!(
            last.and_then(|l| l.error)
                .is_some_and(|e| e.contains(ENDPOINT_ENV))
        );
    }

    /// The endpoint override accepts a loopback http(s) URL and nothing else;
    /// unset means PostHog EU.
    #[test]
    fn the_endpoint_override_is_loopback_only() {
        let parse = |v: &str| endpoint_from(Some(std::ffi::OsStr::new(v)));
        assert_eq!(
            endpoint_from(None).expect("default"),
            Endpoint {
                url: POSTHOG_HOST.to_string(),
                overridden: false
            }
        );
        for ok in [
            "http://127.0.0.1:8123",
            "http://127.0.0.1:8123/ingest",
            "https://localhost",
            "http://LOCALHOST:1",
            "http://[::1]:9",
            "http://127.1.2.3",
        ] {
            let e = parse(ok).unwrap_or_else(|e| panic!("{ok} rejected: {e:#}"));
            assert!(e.overridden, "{ok}");
        }
        for bad in [
            "",
            "127.0.0.1:8123",
            "ftp://127.0.0.1",
            "file:///tmp/x",
            "https://eu.posthog.com",
            "http://10.0.0.1",
            "http://localhost.evil.com",
            "http://127.0.0.1.nip.io",
            "http://[::2]",
            "http://0.0.0.0",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} must be rejected");
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            assert!(endpoint_from(Some(std::ffi::OsStr::from_bytes(b"http://\xff"))).is_err());
        }
    }

    /// `next_flush_due` names exactly the moment `flush_due` turns true, for
    /// each reason a flush can be held off.
    #[test]
    fn next_flush_due_is_when_flush_due_turns_true() {
        let retry = millis(FLUSH_RETRY_INTERVAL);
        let cases = [
            // Young and few: due when the oldest turns five minutes old.
            state(1, MIN, None),
            // A full batch: due already.
            state(FLUSH_AT_EVENTS, 0, None),
            // Due by age but a flusher was just spawned: the retry floor.
            state(1, 10 * MIN, Some(10_000)),
            // Both held: the later of the two.
            state(1, 4 * MIN, Some(30_000)),
        ];
        for s in cases {
            let due = next_flush_due(s, NOW).expect("something pending");
            let at = due.max(NOW);
            assert!(flush_due(s, at), "{s:?}: not due at {at}");
            if due > NOW {
                assert!(!flush_due(s, due - 1), "{s:?}: due before {due}");
            }
        }
        assert_eq!(next_flush_due(state(0, 0, None), NOW), None);
        assert_eq!(
            next_flush_due(state(1, 4 * MIN, Some(30_000)), NOW),
            Some(NOW + MIN)
        );
        assert_eq!(
            next_flush_due(state(1, 10 * MIN, Some(10_000)), NOW),
            Some(NOW - 10_000 + retry)
        );
        // A spawn stamp from the future is ignored, as `flush_due` ignores it.
        assert_eq!(
            next_flush_due(state(FLUSH_AT_EVENTS, 0, Some(-10 * MIN)), NOW),
            Some(NOW)
        );
    }

    fn inputs() -> StatusInputs {
        StatusInputs {
            env_disabled: false,
            config_enabled: true,
            ci: false,
            now_ms: NOW,
        }
    }

    /// The `--json` shape of `heph tool telemetry status` is a stable
    /// interface: exactly these keys, and these values for a spool with one
    /// pending event, a failed flush and a recent spawn.
    #[test]
    fn status_json_shape_is_stable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let spool = spool_in(dir.path());
        let mut old = ev(0);
        old.created_ms = NOW - 2 * MIN;
        spool.enqueue(&old).expect("enqueue");
        spool
            .record_flush(NOW - MIN, Some("offline"))
            .expect("record");
        assert!(spool.claim_spawn(None, NOW - MIN).expect("claim"));

        let status = status_from(inputs(), None, Some(dir.path().to_path_buf())).expect("status");
        let json = serde_json::to_value(&status).expect("json");
        let mut keys: Vec<&str> = json
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "ci",
                "disabled_by",
                "enabled",
                "endpoint",
                "endpoint_error",
                "endpoint_overridden",
                "flush_mode",
                "last_flush_error",
                "last_flush_ms",
                "last_flush_ok",
                "last_flusher_spawn_ms",
                "next_flush_due_ms",
                "now_ms",
                "oldest_event_age_ms",
                "oldest_event_ms",
                "pending_events",
                "spool_path",
            ]
        );
        let spool_path = dir.path().join(SPOOL_FILE).display().to_string();
        assert_eq!(
            json,
            serde_json::json!({
                "ci": false,
                "disabled_by": [],
                "enabled": true,
                "endpoint": POSTHOG_HOST,
                "endpoint_error": null,
                "endpoint_overridden": false,
                "flush_mode": "detached",
                "last_flush_error": "offline",
                "last_flush_ms": NOW - MIN,
                "last_flush_ok": false,
                "last_flusher_spawn_ms": NOW - MIN,
                "next_flush_due_ms": NOW + 3 * MIN,
                "now_ms": NOW,
                "oldest_event_age_ms": 2 * MIN,
                "oldest_event_ms": NOW - 2 * MIN,
                "pending_events": 1,
                "spool_path": spool_path,
            })
        );
    }

    /// Disabled, status says by what (both switches can be), reports the
    /// flush mode as off, and — with no spool on disk — creates none.
    #[test]
    fn status_reports_every_opt_out_and_creates_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = dir.path().join("heph");
        let status = status_from(
            StatusInputs {
                env_disabled: true,
                config_enabled: false,
                ..inputs()
            },
            None,
            Some(cfg.clone()),
        )
        .expect("status");
        assert!(!status.enabled);
        assert_eq!(status.disabled_by, [DisabledBy::Env, DisabledBy::Config]);
        assert_eq!(status.flush_mode, FlushMode::Off);
        assert_eq!(status.pending_events, 0);
        assert_eq!(status.next_flush_due_ms, None);
        assert!(!cfg.exists(), "status must not create the config dir");

        let ci = status_from(
            StatusInputs {
                ci: true,
                ..inputs()
            },
            None,
            Some(cfg),
        )
        .expect("status");
        assert_eq!(ci.flush_mode, FlushMode::Synchronous);
        assert!(ci.disabled_by.is_empty());
    }

    /// A rejected endpoint override shows up in status with the reason, the
    /// raw value, and as an override.
    #[test]
    fn status_shows_a_rejected_endpoint_override() {
        let status = status_from(
            inputs(),
            Some(std::ffi::OsStr::new("https://example.com")),
            None,
        )
        .expect("status");
        assert_eq!(status.endpoint, "https://example.com");
        assert!(status.endpoint_overridden);
        assert!(
            status
                .endpoint_error
                .is_some_and(|e| e.contains("loopback"))
        );
        assert_eq!(status.spool_path, None);
    }

    /// Env var carrying the marker path to [`detached_child_helper`].
    const CHILD_MARKER_ENV: &str = "HEPH_TELEMETRY_TEST_CHILD_MARKER";

    /// Not a test on its own: the stand-in for heph that
    /// [`spawn_detached_does_not_wait_and_detaches_the_child`] spawns (this
    /// test binary re-run with only this test selected). A no-op unless
    /// [`CHILD_MARKER_ENV`] is set. Waits a second, so the spawner has long
    /// returned, then records its session id and cwd and publishes the marker
    /// atomically.
    #[test]
    fn detached_child_helper() {
        let Some(marker) = std::env::var_os(CHILD_MARKER_ENV).map(PathBuf::from) else {
            return;
        };
        std::thread::sleep(Duration::from_secs(1));
        // SAFETY: getsid(0) reads the calling process's session id; it has no
        // preconditions.
        let sid = unsafe { libc::getsid(0) };
        let cwd = std::env::current_dir().expect("cwd");
        let tmp = marker.with_extension("tmp");
        std::fs::write(&tmp, format!("{sid}\n{}\n", cwd.display())).expect("write marker");
        std::fs::rename(&tmp, &marker).expect("publish marker");
    }

    /// The flusher hand-off: `spawn_detached` returns while the child is
    /// still running, and the child runs in a session of its own (so the
    /// terminal's SIGHUP and a Ctrl-C to heph's process group miss it) with
    /// cwd `/` (so it pins no workspace).
    #[test]
    fn spawn_detached_does_not_wait_and_detaches_the_child() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("marker");
        let mut cmd = std::process::Command::new(std::env::current_exe().expect("test exe"));
        cmd.args([
            "--exact",
            "telemetry::flush_tests::detached_child_helper",
            "--test-threads=1",
        ])
        .env(CHILD_MARKER_ENV, &marker);

        let mut child = spawn_detached(cmd).expect("spawn");
        assert!(
            !marker.exists(),
            "spawn_detached waited for the child to finish"
        );

        let deadline = Instant::now() + Duration::from_secs(60);
        while !marker.exists() {
            assert!(
                Instant::now() < deadline,
                "the child never wrote its marker"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let report = std::fs::read_to_string(&marker).expect("marker");
        let mut lines = report.lines();
        let sid: i32 = lines.next().expect("sid").parse().expect("sid number");
        let cwd = lines.next().expect("cwd");

        // SAFETY: as in the helper.
        let our_sid = unsafe { libc::getsid(0) };
        assert_ne!(sid, our_sid, "the child shares the spawner's session");
        assert_eq!(
            i64::from(sid),
            i64::from(child.id()),
            "the child leads its own session"
        );
        assert_eq!(cwd, "/");
        // Reap it; it has exited or is about to.
        drop(child.wait());
    }
}
