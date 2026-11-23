use crate::tui::session::SessionSlot;
use std::sync::Arc;

use clap_complete::engine::ArgValueCompleter;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::commands::progress_app::run_engine_command;
use crate::commands::utils::parse_addr_arg;
use crate::engine::{OutputMatcher, ResultOptions};
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
    // `--no-scratch`: run against empty scratch caches. Implies a rebuild.
    let no_scratch = global.no_scratch;
    run_engine_command(
        format!("Hashout {}", addr.format()),
        sink,
        session,
        &global,
        args.dashboard.dashboard,
        move |engine, rs, ctx| async move {
            // `result_addr` runs the target; rich failures land in `rs`. `finalize`
            // prefers those over the returned marker and prints the hashes on success.
            let res = Arc::clone(&engine)
                .result_addr(
                    rs.clone(),
                    &addr,
                    OutputMatcher::None,
                    &ResultOptions {
                        no_scratch,
                        ..Default::default()
                    },
                )
                .await;
            crate::commands::errors::finalize!(ctx, rs, res, result => {
                for art in &result.artifacts_meta {
                    println!("{}", art.hashout);
                }
                Ok(())
            })
        },
    )
    .await
}
