use crate::tui::session::SessionSlot;
use std::sync::Arc;

use async_trait::async_trait;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::utils::package_matcher_or_all;
use crate::engine::Engine;
use crate::htmatcher::Matcher;
use crate::tui::{self, App, AppContext, BufferedStdout, LogSink};

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

struct LabelsApp {
    engine: Arc<Engine>,
    matcher: Matcher,
    fail_fast: bool,
    source: crate::engine::LabelSource,
}

#[async_trait]
impl App for LabelsApp {
    type Output = ();
    type TuiView = crate::tui::TuiProgressView;
    type CiView = crate::tui::CiProgressView;

    fn tui_view(&self) -> Self::TuiView {
        crate::tui::TuiProgressView::new(format!("Labels {:?}", self.matcher))
    }

    fn ci_view(&self) -> Self::CiView {
        crate::tui::CiProgressView::new(format!("Labels {:?}", self.matcher))
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        let rs = self
            .engine
            .new_state_with_events(self.fail_fast, ctx.event_sender());

        // Labels are collected across all matched targets, so there is nothing to
        // print until the full set is known; resolving targets records rich
        // failures in `rs`, which `finalize` prefers over the returned error.
        let out = BufferedStdout::new(&ctx);
        let gaps = crate::engine::Gaps::new(crate::htquery::format(&self.matcher));
        let res: anyhow::Result<()> = async {
            for label in self
                .engine
                .clone()
                .labels(
                    rs.clone(),
                    &self.matcher,
                    crate::engine::Discovery::keep_going_unless(self.fail_fast, &gaps),
                    self.source,
                )
                .await?
            {
                out.println(label);
            }
            // To the log (stderr), never stdout: the label set stays the whole
            // of the output. A matcher an addr cannot decide was resolved.
            if self.source == crate::engine::LabelSource::Listed && self.matcher.is_addr_only() {
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
    let (engine, shutdown) = bootstrap::new_engine(&global, session)?;
    let app = LabelsApp {
        engine,
        matcher,
        fail_fast: global.fail_fast,
        source: args.source(),
    };
    let interactive = tui::should_use_tui(global.no_tui);
    tui::run_app_with(
        app,
        sink,
        session,
        interactive,
        args.dashboard.dashboard,
        shutdown,
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
