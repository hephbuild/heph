//! `heph tool cache …` — remote cache maintenance subcommands.

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::tui::LogSink;

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
    MeasureLatency,
}

impl CacheArgs {
    pub fn execute(&self, _sink: LogSink, _global: &GlobalOptions) -> anyhow::Result<()> {
        match &self.command {
            CacheCommands::MeasureLatency => bootstrap::block_on(measure_latency())?,
        }
    }
}

async fn measure_latency() -> anyhow::Result<()> {
    let (engine, _shutdown) = bootstrap::new_engine()?;
    let set = engine.remote_caches();
    if set.is_empty() {
        println!("No remote caches configured.");
        return Ok(());
    }

    let results = set.measure_latency().await;
    println!("Remote cache latency (fastest first):");
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
        println!("  {latency}  [{flags}]  {}  ({})", r.name, r.uri);
    }
    Ok(())
}
