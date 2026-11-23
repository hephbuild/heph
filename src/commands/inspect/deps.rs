use crate::tui::session::SessionSlot;
use std::sync::Arc;

use clap_complete::engine::ArgValueCompleter;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::commands::progress_app::run_engine_command;
use crate::commands::utils::parse_addr_arg;
use crate::tui::{self, LogSink};

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Target address (e.g. //pkg:name)
    #[arg(add = ArgValueCompleter::new(complete_target_addr))]
    pub addr: String,
    /// Explore the dependency tree in an interactive TUI (requires a terminal)
    #[arg(short = 'i', long = "interactive")]
    pub interactive: bool,
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
    let addr = parse_addr_arg(&args.addr)?;

    if args.interactive {
        let (engine, shutdown) = bootstrap::new_engine(&global, session)?;
        if !tui::should_use_tui(global.no_tui) {
            anyhow::bail!("--interactive requires a terminal (stderr is not a TTY)");
        }
        return super::deps_explorer::run(engine, addr, sink, shutdown).await;
    }

    run_engine_command(
        format!("Deps {}", addr.format()),
        sink,
        session,
        &global,
        args.dashboard.dashboard,
        move |engine, rs, ctx| async move {
            // `get_def` may run provider targets, recording rich failures in `rs`;
            // `finalize` prefers those over the returned error and prints on success.
            let res = Arc::clone(&engine).get_def(rs.clone(), &addr).await;
            crate::commands::errors::finalize!(ctx, rs, res, def => {
                for input in &def.target_def.inputs {
                    println!("{}", input.r#ref);
                }
                Ok(())
            })
        },
    )
    .await
}
