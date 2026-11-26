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
    /// Exact: only labels of targets that exist, at the cost of resolving
    /// every target. Without it, labels come from what providers list — fast,
    /// but a label carried only by a listed target that does not resolve (a Go
    /// `test` in a package without tests) is printed too.
    #[arg(long)]
    pub exact: bool,
    #[command(flatten)]
    pub dashboard: crate::commands::progress_app::DashboardArgs,
    #[command(flatten)]
    pub out: output::JsonArgs<LabelRow<'static>>,
}

/// `heph inspect labels --json`: one element per label, sorted.
#[derive(Debug, serde::Serialize, output::Record)]
pub struct LabelRow<'a> {
    /// The label.
    label: &'a str,
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
    let json = args.out.clone();
    run_engine_command_out(
        format!("Labels {matcher:?}"),
        sink,
        session,
        &global,
        &args.out,
        args.dashboard.dashboard,
        move |engine, rs, ctx| async move {
            // Labels are collected across all matched targets, so there is nothing to
            // print until the full set is known; resolving targets records rich
            // failures in `rs`, which `finalize` prefers over the returned error.
            let gaps = crate::engine::Gaps::new(crate::htquery::format(&matcher));
            let res = Arc::clone(&engine)
                .labels(
                    rs.clone(),
                    &matcher,
                    crate::engine::Discovery::keep_going_unless(rs.fail_fast(), &gaps),
                    source,
                )
                .await;
            // To the log (stderr), never stdout: the label set stays the whole
            // of the output. A matcher an addr cannot decide was resolved.
            if res.is_ok() && source == crate::engine::LabelSource::Listed && matcher.is_addr_only()
            {
                tracing::info!(
                    "labels read from provider listings, which may include labels of \
                     targets that do not exist; --exact prints the exact set"
                );
            }
            if json.enabled() {
                let cancelled = || rs.ctoken().is_cancelled();
                return crate::commands::errors::finalize(&ctx, &rs, res, Some(&gaps), |labels| {
                    let rows = labels.iter().map(|label| LabelRow { label });
                    crate::commands::utils::json_list(&json, rows, &gaps, &cancelled)
                })
                .await;
            }
            let out = BufferedStdout::new(&ctx);
            let res = res.map(|labels| {
                for label in labels {
                    out.println(label);
                }
            });
            out.close().await;

            crate::commands::errors::finalize(&ctx, &rs, res, Some(&gaps), |_| Ok(())).await
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

    #[test]
    fn record_keys_subset_of_fields() {
        output::check_keys(&LabelRow { label: "test" }).unwrap();
    }
}
