use anyhow::Context;
use clap::Args as ClapArgs;

use crate::commands::{GlobalOptions, bootstrap};
use crate::tui::LogSink;

/// Hidden developer command: run the BUILD-file language server over stdio.
///
/// Editors launch this directly (`heph tool build-lsp`). It
/// speaks LSP JSON-RPC on stdin/stdout and runs until the client disconnects.
#[derive(ClapArgs, Clone)]
pub struct Args {}

pub fn execute(_args: &Args, _sink: LogSink, _global: &GlobalOptions) -> anyhow::Result<()> {
    // Engine construction spawns background tasks (signal/shutdown handlers), so
    // it must run inside a runtime context. The LSP itself is a blocking stdio
    // loop that owns its own lifecycle via the LSP shutdown/exit handshake, so we
    // run it on this thread rather than through the TUI/engine shutdown path.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime for build-lsp")?;
    let _guard = runtime.enter();
    let (engine, _shutdown) = bootstrap::new_engine()?;
    crate::pluginbuildfile::serve_stdio(engine)
}
