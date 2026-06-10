//! Anonymous, opt-out usage telemetry.
//!
//! Reports coarse usage shape to PostHog (EU region) to guide development. Every
//! event is *personless* ([`posthog_rs::Event::new_anon`] → a random per-event
//! id with no person profile) and carries only os/arch/version plus aggregate
//! run counters — never target addresses, labels, filesystem paths, or any user
//! identifier.
//!
//! Disabled by `telemetry.enabled: false` in `.hephconfig2`, or by setting the
//! `HEPH_DISABLE_TELEMETRY` environment variable (the latter also keeps the
//! project's own test/CI binary invocations from reporting). Reporting is
//! strictly best-effort: any failure — offline, timeout, disabled — is swallowed
//! and never affects the command's exit.

mod collector;

pub use collector::{TelemetryCollector, TelemetrySnapshot};

use crate::engine::event::BuildEventKind;
use posthog_rs::{ClientOptionsBuilder, Event};

/// Process-global counters. The engine bumps these from its hot paths without
/// threading a handle through every request; the CLI snapshots them once at
/// exit. Always counted (cheap relaxed atomics) — the opt-out only gates
/// whether [`snapshot`] is ever *sent*.
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

/// Record the total target count of a whole-graph (`//...`) query.
pub fn record_graph_size(total: u64) {
    COLLECTOR.record_graph_size(total);
}

/// Read the global counters. Taken once, at process exit, by the CLI reporter.
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
/// Cap the capture round-trip so telemetry never noticeably delays process exit.
const CAPTURE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
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

/// The shape of a CLI invocation, carrying no user-identifying values.
pub struct ReportContext<'a> {
    /// Subcommand name, e.g. `"run"` (canonical, from the clap model).
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
}

/// Build a personless event from the run context + counters and POST it to
/// PostHog. Best-effort: failures are logged at debug and otherwise ignored.
pub async fn report(ctx: ReportContext<'_>, stats: TelemetrySnapshot) {
    if let Err(e) = try_report(ctx, stats).await {
        tracing::debug!(error = %format!("{e:#}"), "telemetry report skipped");
    }
}

async fn try_report(ctx: ReportContext<'_>, stats: TelemetrySnapshot) -> anyhow::Result<()> {
    let options = ClientOptionsBuilder::default()
        .api_key(POSTHOG_API_KEY.to_string())
        .host(POSTHOG_HOST)
        .request_timeout_seconds(2)
        // We never want PostHog inferring location from the request IP.
        .disable_geoip(true)
        .build()
        .map_err(|e| anyhow::anyhow!("building telemetry client options: {e}"))?;

    let client = posthog_rs::client(options).await;

    let mut event = Event::new_anon("cli_command");
    // Environment — coarse and non-identifying.
    event.insert_prop("os", std::env::consts::OS)?;
    event.insert_prop("arch", std::env::consts::ARCH)?;
    event.insert_prop("version", crate::version::VERSION)?;
    event.insert_prop("ci", is_ci())?;
    // Command + selector shape + the set of flags used (names only, as a
    // queryable array).
    event.insert_prop("command", ctx.command)?;
    event.insert_prop("request_shape", ctx.request_shape)?;
    event.insert_prop("success", ctx.success)?;
    event.insert_prop("flags", ctx.flags)?;
    // Aggregate run counters.
    event.insert_prop("targets", stats.targets)?;
    event.insert_prop("local_cache_hits", stats.local_cache_hits)?;
    event.insert_prop("local_cache_misses", stats.local_cache_misses)?;
    event.insert_prop("artifacts", stats.artifacts)?;
    event.insert_prop("artifact_bytes", stats.artifact_bytes)?;
    // Whole-graph (`//...`) query size — only present when one actually ran, so
    // the attr is absent rather than a misleading 0 on every other invocation.
    if stats.graph_size > 0 {
        event.insert_prop("graph_size", stats.graph_size)?;
    }
    // Enabled plugins (built-ins + config), as queryable arrays of type names.
    let plugins = PLUGINS.get().cloned().unwrap_or_default();
    event.insert_prop("providers", plugins.providers)?;
    event.insert_prop("drivers", plugins.drivers)?;

    tokio::time::timeout(CAPTURE_TIMEOUT, client.capture(event))
        .await
        .map_err(|_elapsed| anyhow::anyhow!("telemetry capture timed out"))?
        .map_err(|e| anyhow::anyhow!("telemetry capture failed: {e}"))?;
    Ok(())
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
}
