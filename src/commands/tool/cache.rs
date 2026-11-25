//! `heph tool cache …` — remote cache maintenance subcommands.

use anyhow::Context;
use std::io::Write;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::tool::codegen::CodegenExit;
use crate::engine::CacheLatency;
use crate::engine::Engine;
use crate::engine::cache_snapshot::{
    Consumption, ExportOutcome, ExportReport, NewerJournalError, RECORDER_WAIT, RecorderAliveError,
    SnapshotCancelled, Tally,
};
use crate::tui::LogSink;
use crate::tui::session::SessionSlot;
use hcore::hasync::{Cancellable, StdCancellationToken};
use hcore::units::human_bytes;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(clap::Args, Clone)]
pub struct CacheArgs {
    #[command(subcommand)]
    pub command: CacheCommands,
}

/// The `--help` epilogue of `snapshot` (D15).
const SNAPSHOT_EXIT_CODES_HELP: &str = "Exit codes:\n  \
    0    written or unchanged, including an incomplete run and revisions left out\n  \
    1    no cache snapshot dir configured\n  \
    2    error (I/O, a full disk, an unwritable dir); a usage error also exits 2\n  \
    3    a heph process is still recording after 10s\n  \
    4    a journal from a newer heph (deleting it is safe)\n  \
    130  cancelled";

/// The `--json` schema version of `snapshot`. Adding a field keeps it;
/// removing or retyping one bumps it.
const SNAPSHOT_JSON_SCHEMA: u32 = 1;

#[derive(clap::Subcommand, Clone)]
pub enum CacheCommands {
    /// Measure and persist remote cache latency ordering
    ///
    /// Probes each configured remote cache once and prints its round-trip
    /// latency, fastest first. The order is persisted (keyed by the cache
    /// definitions) so subsequent builds read caches fastest-first without
    /// re-probing; this command forces a fresh measurement.
    ///
    /// Example: `heph tool cache measure-latency`
    #[command(name = "measure-latency")]
    MeasureLatency {
        #[command(flatten)]
        out: output::JsonArgs<LatencyRow<'static>>,
    },
    /// Write this job's working set to the cache snapshot dir
    ///
    /// Every build with a snapshot dir configured (`cacheSnapshot: { dir }`
    /// in `.hephconfig`, or `HEPH_CACHE_SNAPSHOT`) records the revisions it
    /// used. This writes exactly those into a new generation of the dir, for
    /// the CI's cache step to save, and consumes the records. Run it once
    /// every build of the job has exited; it waits 10s for one still running.
    ///
    /// Example: `heph tool cache snapshot --json`
    #[command(after_help = SNAPSHOT_EXIT_CODES_HELP)]
    Snapshot(SnapshotArgs),
}

#[derive(clap::Args, Clone, Debug)]
pub struct SnapshotArgs {
    /// Print the report as one JSON document
    #[arg(long)]
    pub json: bool,
}

/// One remote cache in `heph tool cache measure-latency --json`, fastest
/// first.
#[derive(Debug, serde::Serialize, output::Record)]
pub struct LatencyRow<'a> {
    /// The cache's name in the config.
    name: &'a str,
    /// Where it lives.
    uri: &'a str,
    /// Whether the cache is configured to be read from.
    readable: bool,
    /// Whether the cache is configured to be written to.
    writable: bool,
    /// Round-trip of one probe request, in milliseconds; `null` when the
    /// cache was unreachable.
    latency_ms: Option<f64>,
}

impl<'a> From<&'a CacheLatency> for LatencyRow<'a> {
    fn from(r: &'a CacheLatency) -> Self {
        Self {
            name: &r.name,
            uri: &r.uri,
            readable: r.readable,
            writable: r.writable,
            latency_ms: r.latency.map(|d| d.as_secs_f64() * 1000.0),
        }
    }
}

impl CacheArgs {
    pub fn execute(
        &self,
        _sink: LogSink,
        session: &Arc<SessionSlot>,
        global: &GlobalOptions,
    ) -> anyhow::Result<()> {
        match &self.command {
            CacheCommands::MeasureLatency { out } => {
                bootstrap::block_on(measure_latency(out, session, global))?
            }
            CacheCommands::Snapshot(args) => snapshot(args, global),
        }
    }
}

async fn measure_latency(
    json: &output::JsonArgs<LatencyRow<'static>>,
    session: &Arc<SessionSlot>,
    global: &GlobalOptions,
) -> anyhow::Result<()> {
    let (engine, _shutdown) = bootstrap::new_engine(global, session)?;
    let set = engine.remote_caches();
    // The snapshot tier is not measured: it is local disk, read first.
    let results = if set.has_user_caches() {
        set.measure_latency().await
    } else {
        Vec::new()
    };
    if json.enabled() {
        return json.list_iter(
            results.iter().map(LatencyRow::from),
            &output::never_cancelled,
        );
    }

    let mut out = std::io::stdout().lock();
    if results.is_empty() {
        return writeln!(out, "No remote caches configured.").context("writing the latencies");
    }
    writeln!(out, "Remote cache latency (fastest first):").context("writing the latencies")?;
    for r in &results {
        let latency = match r.latency {
            Some(d) => format!("{d:>12.2?}"),
            None => format!("{:>12}", "unreachable"),
        };
        // `rw` flags: which permissions the cache is configured with.
        let flags = format!(
            "{}{}",
            if r.readable { 'r' } else { '-' },
            if r.writable { 'w' } else { '-' },
        );
        writeln!(out, "  {latency}  [{flags}]  {}  ({})", r.name, r.uri)
            .context("writing the latencies")?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// snapshot
// ---------------------------------------------------------------------------

/// No snapshot dir is configured: exit 1.
#[derive(Debug, thiserror::Error)]
#[error(
    "no cache snapshot dir is configured: set `cacheSnapshot: {{ dir: <dir> }}` in .hephconfig \
     or HEPH_CACHE_SNAPSHOT"
)]
struct NotConfigured;

/// `heph tool cache snapshot`: print the report, or end with the exit code
/// the failure maps to (D15). The exit travels as a [`CodegenExit`], whose
/// 0/1/2 numbering this command shares.
fn snapshot(args: &SnapshotArgs, global: &GlobalOptions) -> anyhow::Result<()> {
    let res = bootstrap::block_on(async {
        let engine = bootstrap::new_snapshot_engine(global)?;
        let ctoken = StdCancellationToken::new();
        cancel_on_sigint(&ctoken);
        take_snapshot(&engine, RECORDER_WAIT, &ctoken).await
    })
    .and_then(|r| r);
    match res {
        Ok((dir, report)) => {
            if args.json {
                println!("{}", json_report(&dir, &report)?);
            } else {
                for line in human_report(&dir, &report) {
                    println!("{line}");
                }
            }
            Ok(())
        }
        Err(e) => {
            let (code, reason) = exit_code(&e);
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "schema": SNAPSHOT_JSON_SCHEMA,
                        "error": format!("{e:#}"),
                        "reason": reason,
                    })
                );
            }
            Err(CodegenExit {
                code,
                error: Some(e),
            }
            .into())
        }
    }
}

/// Cancel `ctoken` on Ctrl-C. Armed here, before the task is spawned, so a
/// Ctrl-C that lands before a worker polls the task still cancels.
fn cancel_on_sigint(ctoken: &StdCancellationToken) {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(sigint) => sigint,
        Err(e) => {
            tracing::warn!(error = %e, "listening for ctrl-c; it will kill heph instead of cancelling");
            return;
        }
    };
    let ctoken = ctoken.clone();
    tokio::spawn(async move {
        if sigint.recv().await.is_some() {
            ctoken.cancel();
        }
    });
}

/// The snapshot itself, over `engine`'s home and snapshot dir: take the
/// locks (waiting `wait` for live recorders), export, consume the journals.
/// Returns the dir written and the report.
async fn take_snapshot(
    engine: &Engine,
    wait: Duration,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<(PathBuf, ExportReport)> {
    let dir = engine
        .cache_snapshot_dir()
        .ok_or(NotConfigured)?
        .to_path_buf();
    let consumption = Consumption::acquire(engine.home.as_path(), wait, ctoken).await?;
    let report = consumption.export(engine, &dir, ctoken).await?;
    Ok((dir, report))
}

/// The exit code (D15) and the `--json` `reason` for a failed snapshot: the
/// first typed error in the chain decides; anything else is an error (2).
fn exit_code(e: &anyhow::Error) -> (u8, &'static str) {
    for cause in e.chain() {
        if cause.is::<NotConfigured>() {
            return (1, "not_configured");
        }
        if cause.is::<RecorderAliveError>() {
            return (3, "recorder_alive");
        }
        if cause.is::<NewerJournalError>() {
            return (4, "newer_journal");
        }
        if cause.is::<SnapshotCancelled>() {
            return (130, "cancelled");
        }
    }
    (2, "error")
}

/// The `--json` document: `schema`, `dir`, `complete`, then the export's
/// report, field for field. Integer bytes, the top revisions by bytes, the
/// dropped addrs sorted; no timestamps.
#[derive(Serialize)]
struct JsonReport<'a> {
    schema: u32,
    dir: &'a Path,
    /// False when the run was incomplete (`incomplete` says why).
    complete: bool,
    #[serde(flatten)]
    report: &'a ExportReport,
}

fn json_report(dir: &Path, report: &ExportReport) -> anyhow::Result<String> {
    serde_json::to_string(&JsonReport {
        schema: SNAPSHOT_JSON_SCHEMA,
        dir,
        complete: report.incomplete.is_empty(),
        report,
    })
    .map_err(Into::into)
}

/// The human report, one fact per line.
fn human_report(dir: &Path, report: &ExportReport) -> Vec<String> {
    let mut out = Vec::new();
    out.push(match &report.outcome {
        ExportOutcome::Written { generation } => {
            format!("snapshot: wrote {generation} in {}", dir.display())
        }
        ExportOutcome::Unchanged => {
            format!(
                "snapshot: unchanged, the working set is empty ({})",
                dir.display()
            )
        }
    });
    let tally = |label: &str, t: &Tally| {
        format!(
            "  {label:<14} {:>6} revisions  {}",
            t.count,
            human_bytes(t.bytes)
        )
    };
    if matches!(report.outcome, ExportOutcome::Written { .. }) {
        out.push(tally("kept", &report.kept));
        out.push(tally("new", &report.new));
        out.push(tally("carried over", &report.carried_over));
        out.push(tally("dropped", &report.dropped));
    }
    for (why, t) in &report.left_out {
        out.push(tally(&format!("left out: {why}"), t));
    }
    let s = &report.scratch;
    if [s.new, s.kept, s.carried_over, s.dropped]
        .iter()
        .any(|t| t.count > 0)
        || s.skipped > 0
    {
        let slots = |label: &str, t: &Tally| {
            format!(
                "  scratch {label:<7} {:>5} slots      {}",
                t.count,
                human_bytes(t.bytes)
            )
        };
        out.push(slots("kept", &s.kept));
        out.push(slots("new", &s.new));
        if s.carried_over.count > 0 {
            out.push(slots("carried", &s.carried_over));
        }
        out.push(slots("dropped", &s.dropped));
        if s.skipped > 0 {
            out.push(format!(
                "  scratch skipped {:>5} slots (opted out of the remote cache, or gone)",
                s.skipped
            ));
        }
    }
    out.push(format!("journals: {} consumed", report.journals.len()));
    for i in &report.incomplete {
        out.push(format!(
            "incomplete: {} ({}); {} carried over",
            i.reason,
            i.journal.display(),
            human_bytes(report.carried_over.bytes)
        ));
    }
    for e in &report.cleanup_errors {
        out.push(format!("cleanup failed: {e}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_keys_subset_of_fields() -> anyhow::Result<()> {
        let r = CacheLatency {
            name: "r2".to_string(),
            uri: "s3://bucket".to_string(),
            readable: true,
            writable: false,
            latency: Some(std::time::Duration::from_micros(1500)),
        };
        let row = LatencyRow::from(&r);
        assert_eq!(row.latency_ms, Some(1.5));
        output::check_keys(&row)
    }

    #[test]
    fn each_failure_maps_to_its_exit_code() {
        let cases: [(anyhow::Error, u8); 5] = [
            (NotConfigured.into(), 1),
            (anyhow::anyhow!("disk full"), 2),
            (
                anyhow::Error::from(RecorderAliveError {
                    recorders: Vec::new(),
                    waited: Duration::ZERO,
                })
                .context("taking the journals"),
                3,
            ),
            (
                anyhow::Error::from(NewerJournalError {
                    path: PathBuf::from("/w/1-a.log"),
                    version: 9,
                })
                .context("reading the journals"),
                4,
            ),
            (SnapshotCancelled.into(), 130),
        ];
        for (e, want) in cases {
            assert_eq!(exit_code(&e).0, want, "{e:#}");
        }
    }
}
