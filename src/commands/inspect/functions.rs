use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::tui::LogSink;

#[derive(clap::Args, Clone)]
pub struct Args {}

pub fn execute(args: &Args, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, global.clone()))?
}

async fn execute_async(_args: Args, _sink: LogSink, _global: GlobalOptions) -> anyhow::Result<()> {
    let (engine, _shutdown) = bootstrap::new_engine()?;
    for (provider, func) in engine.provider_functions() {
        println!("heph.{provider}.{func}");
    }
    Ok(())
}
