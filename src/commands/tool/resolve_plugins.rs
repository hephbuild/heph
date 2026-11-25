use anyhow::Context;
use std::io::Write;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::tui::LogSink;
use crate::tui::session::SessionSlot;
use std::sync::Arc;

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Re-download every plugin even if already cached (clears
    /// `~/.heph/plugins/<os>-<arch>/` first).
    #[arg(long)]
    pub force: bool,
    #[command(flatten)]
    pub out: output::JsonArgs<Resolved>,
}

/// `heph tool resolve-plugins --json`: what the configured plugins
/// contributed, once every one of them loaded.
#[derive(Debug, serde::Serialize, output::Record)]
pub struct Resolved {
    /// Every registered provider, by full name, sorted.
    providers: Vec<String>,
    /// Every registered driver, by full name, sorted.
    drivers: Vec<String>,
}

pub fn execute(
    args: &Args,
    _sink: LogSink,
    session: &Arc<SessionSlot>,
    global: &GlobalOptions,
) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), session, global.clone()))?
}

async fn execute_async(
    args: Args,
    session: &Arc<SessionSlot>,
    global: GlobalOptions,
) -> anyhow::Result<()> {
    if args.force {
        crate::engine::clear_plugin_cache()?;
    }

    // Building the engine resolves every `plugins:` entry end to end: built-ins
    // are instantiated, and each `path:`/`url:` plugin's manifest is read (a
    // `url:` is downloaded to ~/.heph/plugins), the host dylib resolved, and the
    // cdylib load-checked over the stable ABI (`create` runs). Reaching `Ok`
    // here therefore means every configured plugin is present and loadable.
    let (engine, _shutdown) = bootstrap::new_engine(&global, session)?;

    let mut providers: Vec<String> = engine.providers_by_name.keys().cloned().collect();
    let mut drivers: Vec<String> = engine.drivers_by_name.keys().cloned().collect();
    providers.sort();
    drivers.sort();

    if args.out.enabled() {
        let resolved = Resolved { providers, drivers };
        return args.out.record(&resolved, &output::never_cancelled);
    }
    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "plugins resolved \u{2713} ({} providers, {} drivers)\n  providers: {}\n  drivers:   {}",
        providers.len(),
        drivers.len(),
        providers.join(", "),
        drivers.join(", "),
    )
    .context("writing the resolved plugins")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_keys_subset_of_fields() -> anyhow::Result<()> {
        output::check_keys(&Resolved {
            providers: vec!["fs".to_string()],
            drivers: vec!["exec".to_string()],
        })
    }
}
