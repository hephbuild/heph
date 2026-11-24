use crate::tui::session::SessionSlot;
use std::sync::Arc;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::progress_app::run_engine_command;
use crate::commands::utils::package_matcher_or_all;
use crate::tui::{BufferedStdout, LogSink};

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Package matcher (defaults to all packages)
    pub matcher: Option<String>,
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
    let matcher = package_matcher_or_all(args.matcher.as_deref())?;
    run_engine_command(
        format!("Packages {matcher:?}"),
        sink,
        session,
        &global,
        args.dashboard.dashboard,
        move |engine, rs, ctx| async move {
            // Output is incremental; listing may run provider targets, recording rich
            // failures in `rs`, which `finalize` prefers over the returned error.
            let out = BufferedStdout::new(&ctx);
            let res: anyhow::Result<()> = async {
                for p in engine.packages(&matcher, &rs).await? {
                    out.println(p);
                }
                Ok(())
            }
            .await;
            out.close().await;
            crate::commands::errors::finalize(&ctx, &rs, res, None, |_| Ok(())).await
        },
    )
    .await
}
