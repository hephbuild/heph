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
    /// Exact: only labels of targets that exist, at the cost of resolving
    /// every target. Without it, labels come from what providers list — fast,
    /// but a label carried only by a listed target that does not resolve (a Go
    /// `test` in a package without tests) is printed too.
    #[arg(long)]
    pub exact: bool,
    #[command(flatten)]
    pub dashboard: crate::commands::progress_app::DashboardArgs,
}

impl Args {
    fn source(&self) -> crate::engine::LabelSource {
        if self.exact {
            crate::engine::LabelSource::Resolved
        } else {
            crate::engine::LabelSource::Listed
        }
    }
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
    let source = args.source();
    run_engine_command(
        format!("Labels {matcher:?}"),
        sink,
        session,
        &global,
        global.no_tui,
        args.dashboard.dashboard,
        move |engine, rs, ctx| async move {
            // Labels are collected across all matched targets, so there is nothing to
            // print until the full set is known; resolving targets records rich
            // failures in `rs`, which `finalize` prefers over the returned error.
            let out = BufferedStdout::new(&ctx);
            let gaps = crate::engine::Gaps::new(crate::htquery::format(&matcher));
            let res: anyhow::Result<()> = async {
                for label in Arc::clone(&engine)
                    .labels(
                        rs.clone(),
                        &matcher,
                        crate::engine::Discovery::keep_going_unless(rs.fail_fast(), &gaps),
                        source,
                    )
                    .await?
                {
                    out.println(label);
                }
                // To the log (stderr), never stdout: the label set stays the whole
                // of the output. A matcher an addr cannot decide was resolved.
                if source == crate::engine::LabelSource::Listed && matcher.is_addr_only() {
                    tracing::info!(
                        "labels read from provider listings, which may include labels of \
                         targets that do not exist; --exact prints the exact set"
                    );
                }
                Ok(())
            }
            .await;
            out.close().await;

            crate::commands::errors::finalize!(ctx, rs, res, gaps = &gaps)
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(clap::Parser)]
    struct Cli {
        #[command(flatten)]
        args: Args,
    }

    /// Listed labels unless `--exact` asks for the exact set.
    #[test]
    fn exact_flag_picks_the_label_source() -> anyhow::Result<()> {
        let plain = Cli::try_parse_from(["labels", "//foo/..."])?.args;
        assert_eq!(plain.source(), crate::engine::LabelSource::Listed);
        let exact = Cli::try_parse_from(["labels", "--exact", "//foo/..."])?.args;
        assert_eq!(exact.source(), crate::engine::LabelSource::Resolved);
        Ok(())
    }
}
