use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::tui::LogSink;

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Re-download every plugin even if already cached (clears
    /// `~/.heph/plugins/<os>-<arch>/` first).
    #[arg(long)]
    pub force: bool,
}

pub fn execute(args: &Args, _sink: LogSink, _global: &GlobalOptions) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone()))?
}

async fn execute_async(args: Args) -> anyhow::Result<()> {
    if args.force {
        crate::engine::clear_plugin_cache()?;
    }

    // Building the engine resolves every `plugins:` entry end to end: built-ins
    // are instantiated, and each `path:`/`url:` plugin's manifest is read (a
    // `url:` is downloaded to ~/.heph/plugins), the host dylib resolved, and the
    // cdylib load-checked over the stable ABI (`create` runs). Reaching `Ok`
    // here therefore means every configured plugin is present and loadable.
    let (engine, _shutdown) = bootstrap::new_engine()?;

    let mut providers: Vec<String> = engine.providers_by_name.keys().cloned().collect();
    let mut drivers: Vec<String> = engine.drivers_by_name.keys().cloned().collect();
    providers.sort();
    drivers.sort();

    println!(
        "plugins resolved \u{2713} ({} providers, {} drivers)",
        providers.len(),
        drivers.len()
    );
    println!("  providers: {}", providers.join(", "));
    println!("  drivers:   {}", drivers.join(", "));
    Ok(())
}
