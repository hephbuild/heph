use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::tui::LogSink;
use crate::tui::session::SessionSlot;
use std::sync::Arc;

#[derive(clap::Args, Clone)]
pub struct Args {}

pub fn execute(
    args: &Args,
    sink: LogSink,
    session: &Arc<SessionSlot>,
    global: &GlobalOptions,
) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, session, global.clone()))?
}

async fn execute_async(
    _args: Args,
    _sink: LogSink,
    session: &Arc<SessionSlot>,
    global: GlobalOptions,
) -> anyhow::Result<()> {
    let (engine, _shutdown) = bootstrap::new_engine(&global, session)?;
    // `<plugin>.<signature>`, sorted by plugin, then function.
    for (plugin, _func, rendered) in engine.functions() {
        println!("{plugin}.{rendered}");
    }
    Ok(())
}
