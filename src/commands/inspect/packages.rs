use crate::tui::session::SessionSlot;
use std::sync::Arc;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::progress_app::run_engine_command_out;
use crate::commands::utils::package_matcher_or_all;
use crate::tui::{BufferedStdout, LogSink};

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Package matcher (defaults to all packages)
    pub matcher: Option<String>,
    #[command(flatten)]
    pub dashboard: crate::commands::progress_app::DashboardArgs,
    #[command(flatten)]
    pub out: output::JsonArgs<PackageRow<'static>>,
}

/// `heph inspect packages --json`: one element per package.
#[derive(Debug, serde::Serialize, output::Record)]
pub struct PackageRow<'a> {
    /// The package, as the text output prints it.
    package: &'a str,
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
    let json = args.out.clone();
    run_engine_command_out(
        format!("Packages {matcher:?}"),
        sink,
        session,
        &global,
        &args.out,
        args.dashboard.dashboard,
        move |engine, rs, ctx| async move {
            let res = engine.packages(&matcher, &rs).await;
            if json.enabled() {
                let cancelled = || rs.ctoken().is_cancelled();
                return crate::commands::errors::finalize(&ctx, &rs, res, None, |pkgs| {
                    let rows = pkgs.iter().map(|p| PackageRow { package: p });
                    json.list_iter(rows, &cancelled)
                })
                .await;
            }
            // Listing may run provider targets, recording rich failures in
            // `rs`, which `finalize` prefers over the returned error.
            let out = BufferedStdout::new(&ctx);
            let res = res.map(|pkgs| {
                for p in pkgs {
                    out.println(p);
                }
            });
            out.close().await;
            crate::commands::errors::finalize(&ctx, &rs, res, None, |_| Ok(())).await
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_keys_subset_of_fields() {
        output::check_keys(&PackageRow { package: "a/b" }).unwrap();
    }
}
