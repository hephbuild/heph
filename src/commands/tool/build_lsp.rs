use anyhow::Context;
use clap::Args as ClapArgs;

use crate::commands::{GlobalOptions, bootstrap};
use crate::tui::LogSink;

/// Hidden developer command: run the BUILD-file language server over stdio.
///
/// Editors launch this directly (`heph tool build-lsp`). It
/// speaks LSP JSON-RPC on stdin/stdout and runs until the client disconnects.
#[derive(ClapArgs, Clone)]
pub struct Args {
    /// Instead of serving, print how the server classifies one position of a
    /// BUILD file: the site, why heph's answer there would be empty, and what
    /// a present answer could not do in full. Line and column are 1-based,
    /// the column counted in characters.
    #[arg(long, value_name = "FILE:LINE:COL")]
    explain: Option<String>,

    /// With `--explain`: print the classification as one JSON object.
    #[arg(long, requires = "explain")]
    json: bool,
}

pub fn execute(args: &Args, _sink: LogSink, _global: &GlobalOptions) -> anyhow::Result<()> {
    if let Some(target) = &args.explain {
        let out = crate::pluginbuildfile::lsp_explain(target, args.json)?;
        println!("{out}");
        return Ok(());
    }
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
