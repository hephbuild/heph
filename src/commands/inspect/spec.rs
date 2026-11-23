use crate::tui::session::SessionSlot;
use std::sync::Arc;

use anyhow::Context;
use clap_complete::engine::ArgValueCompleter;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::commands::progress_app::run_engine_command;
use crate::commands::utils::parse_addr_arg;
use crate::tui::LogSink;

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Target address (e.g. //pkg:name)
    #[arg(add = ArgValueCompleter::new(complete_target_addr))]
    pub addr: String,
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
    run_engine_command(
        format!("Spec {}", addr.format()),
        sink,
        session,
        &global,
        args.dashboard.dashboard,
        move |engine, rs, ctx| async move {
            // `get_spec` may run provider targets, recording rich failures in `rs`;
            // `finalize` prefers those over the returned error and prints on success.
            let res = Arc::clone(&engine).get_spec(rs.clone(), &addr).await;
            crate::commands::errors::finalize!(ctx, rs, res, spec => {
                let json = serde_json::to_string_pretty(&*spec).context("serialize spec")?;
                println!("{json}");
                Ok(())
            })
        },
    )
    .await
}
