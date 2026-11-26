//! `heph tool cache …` — remote cache maintenance subcommands.

use anyhow::Context;
use std::io::Write;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::engine::CacheLatency;
use crate::tui::LogSink;
use crate::tui::session::SessionSlot;
use std::sync::Arc;

#[derive(clap::Args, Clone)]
pub struct CacheArgs {
    #[command(subcommand)]
    pub command: CacheCommands,
}

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
    let results = if set.is_empty() {
        Vec::new()
    } else {
        set.measure_latency().await
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
}
