use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::tui::LogSink;
use crate::tui::session::SessionSlot;
use anyhow::Context as _;
use std::io::Write as _;
use std::sync::Arc;

#[derive(clap::Args, Clone)]
pub struct Args {
    #[command(flatten)]
    pub out: output::JsonArgs<FunctionRow<'static>>,
}

/// `heph inspect functions --json`: one element per plugin function, sorted
/// by plugin, then function.
#[derive(Debug, serde::Serialize, output::Record)]
pub struct FunctionRow<'a> {
    /// The plugin that registers it: the function is `heph.<plugin>.<name>`.
    plugin: &'a str,
    /// The function's name.
    name: &'a str,
    /// Its signature, as the text output prints it after `<plugin>.`.
    signature: &'a str,
}

pub fn execute(
    args: &Args,
    sink: LogSink,
    session: &Arc<SessionSlot>,
    global: &GlobalOptions,
) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, session, global.clone()))?
}

async fn execute_async(
    args: Args,
    _sink: LogSink,
    session: &Arc<SessionSlot>,
    global: GlobalOptions,
) -> anyhow::Result<()> {
    let (engine, _shutdown) = bootstrap::new_engine(&global, session)?;
    // `<plugin>.<signature>`, sorted by plugin, then function.
    let functions = engine.functions();
    let rows = functions
        .iter()
        .map(|(plugin, name, rendered)| FunctionRow {
            plugin,
            name,
            signature: rendered,
        });
    if args.out.enabled() {
        // No request: SIGINT ends the process as is.
        return args.out.list_iter(rows, &output::never_cancelled);
    }
    let mut stdout = std::io::stdout().lock();
    for row in rows {
        writeln!(stdout, "{}.{}", row.plugin, row.signature).context("printing the functions")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_keys_subset_of_fields() {
        output::check_keys(&FunctionRow {
            plugin: "go",
            name: "lib",
            signature: "lib(name: str)",
        })
        .unwrap();
    }
}
