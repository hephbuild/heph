use crate::tui::session::SessionSlot;
use std::sync::Arc;

use clap_complete::engine::ArgValueCompleter;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::commands::progress_app::run_engine_command;
use crate::commands::utils::resolve_addr;
use crate::tui::LogSink;

#[derive(clap::Args, Clone)]
pub struct Args {
    /// One end of the chain (e.g. //cmd/server:bin)
    #[arg(add = ArgValueCompleter::new(complete_target_addr))]
    pub a: String,
    /// The other end of the chain (e.g. //lib:core) — order does not matter
    #[arg(add = ArgValueCompleter::new(complete_target_addr))]
    pub b: String,
    /// Follow only directly declared deps, without applying transitive deps
    #[arg(long)]
    pub no_transitive: bool,
    #[command(flatten)]
    pub dashboard: crate::commands::progress_app::DashboardArgs,
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
    sink: LogSink,
    session: &Arc<SessionSlot>,
    global: GlobalOptions,
) -> anyhow::Result<()> {
    let a = resolve_addr(args.a.as_ref())?;
    let b = resolve_addr(args.b.as_ref())?;
    // Kept for the "no path" log line — the addrs themselves move into the walk.
    let (a_label, b_label) = (a.format(), b.format());
    let no_transitive = args.no_transitive;
    run_engine_command(
        format!("Path {a_label} ↔ {b_label}"),
        sink,
        session,
        &global,
        args.dashboard.dashboard,
        move |engine, rs, ctx| async move {
            // Resolving defs may run provider targets, recording rich failures in
            // `rs`; `finalize` prefers those over the returned error.
            let res = Arc::clone(&engine)
                .dep_path_between(rs.clone(), a, b, no_transitive)
                .await;
            crate::commands::errors::finalize!(ctx, rs, res, chain => {
                match chain {
                    // The chain reads dependent → dependency whichever way the
                    // arguments were given, so the direction is visible in the output.
                    Some(chain) => {
                        for addr in chain {
                            println!("{}", addr.format());
                        }
                    }
                    // Unconnected targets leave stdout empty, so callers can test the
                    // output for emptiness; the reason goes to the log (stderr) instead.
                    None => tracing::info!("no path between {a_label} and {b_label}"),
                }
                Ok(())
            })
        },
    )
    .await
}
