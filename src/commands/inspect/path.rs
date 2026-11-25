use crate::tui::session::SessionSlot;
use std::sync::Arc;

use anyhow::Context as _;
use clap_complete::engine::ArgValueCompleter;
use std::io::Write as _;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::commands::progress_app::run_engine_command_out;
use crate::commands::utils::resolve_addr;
use crate::htaddr::Addr;
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
    #[command(flatten)]
    pub out: output::JsonArgs<PathRow<'static>>,
}

/// `heph inspect path --json`: one element per hop, from the dependent to the
/// dependency; `[]` when the two are unconnected.
#[derive(Debug, serde::Serialize, output::Record)]
pub struct PathRow<'a> {
    /// A target on the chain.
    addr: &'a Addr,
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
    let json = args.out.clone();
    run_engine_command_out(
        format!("Path {a_label} ↔ {b_label}"),
        sink,
        session,
        &global,
        &args.out,
        args.dashboard.dashboard,
        move |engine, rs, ctx| async move {
            // Resolving defs may run provider targets, recording rich failures in
            // `rs`; `finalize` prefers those over the returned error.
            let res = Arc::clone(&engine)
                .dep_path_between(rs.clone(), a, b, no_transitive)
                .await;
            let cancelled = || rs.ctoken().is_cancelled();
            crate::commands::errors::finalize(&ctx, &rs, res, None, |chain| {
                // Unconnected targets leave stdout empty (`[]` under --json), so
                // callers can test the output for emptiness; the reason goes to
                // the log (stderr) instead.
                if chain.is_none() {
                    tracing::info!("no path between {a_label} and {b_label}");
                }
                // The chain reads dependent → dependency whichever way the
                // arguments were given, so the direction is visible in the output.
                let rows = chain.iter().flatten().map(|addr| PathRow { addr });
                if json.enabled() {
                    return json.list_iter(rows, &cancelled);
                }
                let mut stdout = std::io::stdout().lock();
                for row in rows {
                    writeln!(stdout, "{}", row.addr.format()).context("printing the path")?;
                }
                Ok(())
            })
            .await
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_keys_subset_of_fields() {
        let addr = crate::htaddr::parse_addr("//a:b").unwrap();
        output::check_keys(&PathRow { addr: &addr }).unwrap();
    }
}
