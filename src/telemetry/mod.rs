//! Anonymous, opt-out usage telemetry.
//!
//! Reports coarse usage shape to PostHog (EU region) to guide development.
//! Every event is *personless* (`$process_person_profile: false` — no person
//! profile is ever created) and carries only os/arch/version plus aggregate run
//! counters — never target addresses, labels, filesystem paths, or any user
//! identifier. The `distinct_id` is a random per-install UUID stored in the
//! machine's config dir (or the constant `"ci"` on CI runners, where ephemeral
//! machines would otherwise mint a fake install per job); it identifies an
//! installation, not a person.
//!
//! Commands never pay network latency: at exit the event is appended to a local
//! SQLite spool (one insert), and the *next* invocation flushes pending rows in
//! a detached background thread while the command does its real work. See
//! [`spool`].
//!
//! Disabled by `telemetry.enabled: false` in `.hephconfig2`, or by setting the
//! `HEPH_DISABLE_TELEMETRY` environment variable (the latter also keeps the
//! project's own test/CI binary invocations from reporting). Everything is
//! strictly best-effort: any failure — offline, timeout, disabled — is swallowed
//! and never affects the command's exit.

mod collector;
mod spool;

pub use collector::{TelemetryCollector, TelemetrySnapshot};

use crate::engine::event::BuildEventKind;
use posthog_rs::{ClientOptionsBuilder, Event};
use spool::{FLUSH_BATCH, Spool, SpooledEvent};
use std::path::PathBuf;

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

/// Record one resolved target's artifact count + total byte size.
pub fn record_artifacts(count: u64, bytes: u64) {
    COLLECTOR.record_artifacts(count, bytes);
}

/// Record the total target count of a whole-graph (`//...`) enumeration.
pub fn record_graph_size(total: u64) {
    COLLECTOR.record_graph_size(total);
}

/// Read the global counters (largest-seen for `graph_size`, sums elsewhere).
pub fn snapshot() -> TelemetrySnapshot {
    COLLECTOR.snapshot()
}

/// Names of the providers + drivers the engine has registered (built-ins plus
/// whatever the config enabled). Plugin *type* names only — never user data.
#[derive(Debug, Clone, Default)]
struct Plugins {
    providers: Vec<String>,
    drivers: Vec<String>,
}

/// Set once, at engine construction. Read by the reporter at exit.
static PLUGINS: std::sync::OnceLock<Plugins> = std::sync::OnceLock::new();

/// Record the enabled provider + driver names. First write wins (a process
/// builds at most one engine for the command it runs); later calls are ignored.
pub fn record_plugins(mut providers: Vec<String>, mut drivers: Vec<String>) {
    providers.sort();
    drivers.sort();
    // Best-effort set-once; a second engine in one process keeps the first set.
    drop(PLUGINS.set(Plugins { providers, drivers }));
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

/// The shape of a CLI invocation, carrying no user-identifying values.
pub struct ReportContext<'a> {
    /// Full subcommand path, e.g. `"run"` or `"inspect deps"` (canonical names
    /// from the clap model).
    pub command: &'a str,
    /// Selector shape: `"label_matcher"` (two args — a label + package matcher),
    /// `"addr"` (one arg — a single address), or `"none"`. Structure only, never
    /// the actual label/address value.
    pub request_shape: &'a str,
    /// Names of the args/flags that were explicitly set — derived from the clap
    /// matches, so this stays exhaustive for every command without per-command
    /// code. Names only; never the values (an addr/label could be PII).
    pub flags: &'a [String],
    /// Whether the command ultimately succeeded.
    pub success: bool,
    /// Coarse failure class on error: `"cancelled"`, `"target_failure"`, or
    /// `"error"`. `None` on success.
    pub failure: Option<&'a str>,
    /// Command wall time, parse-to-exit.
    pub duration_ms: u64,
}

/// Append this invocation's event to the local spool. Called once at process
/// exit — a single SQLite insert, no network. Best-effort: failures are logged
/// at debug and otherwise ignored.
pub fn enqueue_invocation(ctx: ReportContext<'_>) {
    if let Err(e) = try_enqueue(ctx) {
        tracing::debug!(error = %format!("{e:#}"), "telemetry enqueue skipped");
    }
}

fn try_enqueue(ctx: ReportContext<'_>) -> anyhow::Result<()> {
    let stats = COLLECTOR.snapshot();
    let plugins = PLUGINS.get().cloned().unwrap_or_default();

    let mut props = serde_json::Map::new();
    let mut put = |k: &str, v: serde_json::Value| drop(props.insert(k.to_string(), v));
    // Personless: never create/update a person profile for this distinct_id.
    put("$process_person_profile", false.into());
    // Environment — coarse and non-identifying.
    put("os", std::env::consts::OS.into());
    put("arch", std::env::consts::ARCH.into());
    put("version", crate::version::VERSION.into());
    put("ci", is_ci().into());
    // Command + selector shape + the set of flags used (names only).
    put("command", ctx.command.into());
    put("request_shape", ctx.request_shape.into());
    put("success", ctx.success.into());
    put("flags", ctx.flags.into());
    if let Some(failure) = ctx.failure {
        put("failure", failure.into());
    }
    put("duration_ms", ctx.duration_ms.into());
    // Aggregate run counters.
    put("targets", stats.targets.into());
    put("local_cache_hits", stats.local_cache_hits.into());
    put("local_cache_misses", stats.local_cache_misses.into());
    put("artifacts", stats.artifacts.into());
    put("artifact_bytes", stats.artifact_bytes.into());
    // Whole-graph (`//...`) query size — only present when one actually ran, so
    // the attr is absent rather than a misleading 0 on every other invocation.
    if stats.graph_size > 0 {
        put("graph_size", stats.graph_size.into());
    }
    // Enabled plugins (built-ins + config), as queryable arrays of type names.
    put("providers", plugins.providers.into());
    put("drivers", plugins.drivers.into());

    let dir = config_dir()?;
    let event = SpooledEvent {
        uuid: uuid::Uuid::new_v4().to_string(),
        distinct_id: distinct_id(&dir)?,
        event: "cli_command".to_string(),
        props,
        created_ms: crate::engine::event::now_unix_ms() as i64,
    };
    Spool::open(&dir.join("telemetry-spool.db"))?.enqueue(&event)
}

/// Flush the spool synchronously, blocking until sent (or failed). Used on CI,
/// where the runner — and its spool — is ephemeral: a deferred flush would
/// never happen, so the exit pays the POST to get the data out at all.
pub fn flush_blocking() {
    if let Err(e) = flush_once() {
        tracing::debug!(error = %format!("{e:#}"), "telemetry flush skipped");
    }
}

/// Flush previously spooled events to PostHog on a detached background thread.
/// Called once at startup, *before* the command runs, so the POST overlaps real
/// work instead of delaying exit. The thread dies with the process if the
/// command finishes first — rows are deleted only after a successful send, so
/// an interrupted flush is retried next run, and the per-event UUID lets
/// PostHog dedupe any row that was sent but not yet deleted.
pub fn flush_in_background() {
    std::thread::Builder::new()
        .name("telemetry-flush".to_string())
        .spawn(|| {
            if let Err(e) = flush_once() {
                tracing::debug!(error = %format!("{e:#}"), "telemetry flush skipped");
            }
        })
        .map(drop)
        .unwrap_or_else(|e| tracing::debug!(error = %e, "telemetry flush thread not started"));
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
                created_ms: crate::engine::event::now_unix_ms() as i64,
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
