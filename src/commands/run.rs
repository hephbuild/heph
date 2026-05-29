use std::io;
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use clap::Args;

use crate::commands::bootstrap;
use crate::commands::utils::matcher_from_args;
use crate::engine::{Engine, InteractiveWrapper, OutputMatcher, ResultOptions, get_cwp};
use crate::htmatcher::Matcher;
use crate::tui::{self, App, AppContext, LogSink};

#[derive(Args, Clone)]
#[command(override_usage = "run <TARGET_ADDRESS>\n       run <LABEL> <PACKAGE_MATCHER>")]
pub struct RunArgs {
    /// Target address (e.g., //pkg:name) OR Label
    #[arg(value_name = "TARGET_ADDRESS/LABEL")]
    pub arg1: String,
    /// Package matcher (only if first argument is a Label)
    #[arg(value_name = "PACKAGE_MATCHER")]
    pub arg2: Option<String>,
    /// Force execution
    #[arg(long = "force")]
    pub force: bool,
    /// Interactive shell
    #[arg(long = "shell", num_args = 0..=1, require_equals = true, default_missing_value = "", value_name = "TARGET",)]
    pub shell: Option<String>,
    /// Print output artifacts to stdout
    #[arg(long = "cat-out")]
    pub cat_out: bool,
    /// Print output file list to stdout
    #[arg(long = "list-out")]
    pub list_out: bool,
    /// Exclude target address (repeatable, e.g. -e //pkg:target)
    #[arg(short = 'e', long = "exclude", value_name = "TARGET_ADDRESS")]
    pub exclude: Vec<String>,
    /// Disable fail-fast: run every matched target and report all failures at the end
    #[arg(long = "no-fail-fast", action = clap::ArgAction::SetTrue)]
    pub no_fail_fast: bool,
}

struct RunApp {
    args: RunArgs,
    engine: Arc<Engine>,
    matcher: Matcher,
}

impl RunApp {
    fn progress_label(&self) -> String {
        match &self.matcher {
            Matcher::Addr(a) => format!("Running {}", a.format()),
            other => format!("Running {other:?}"),
        }
    }
}

#[async_trait]
impl App for RunApp {
    type Output = ();
    type TuiView = tui::TuiProgressView;
    type CiView = tui::CiProgressView;

    fn tui_view(&self) -> Self::TuiView {
        tui::TuiProgressView::new(self.progress_label())
    }

    fn ci_view(&self) -> Self::CiView {
        tui::CiProgressView::new(self.progress_label())
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        let interactive: Option<InteractiveWrapper> = if ctx.interactive() {
            let pauser = ctx.pauser();
            Some(Arc::new(move |inner| {
                let pauser = pauser.clone();
                Box::pin(async move {
                    let _guard = pauser.pause().await;
                    // Source stdin from the client's /dev/tty via a TtyReader
                    // rather than tokio::io::stdin(): tokio's stdin spawns a
                    // global blocking thread parked on read(0, …) that cannot
                    // be cancelled, keeping the runtime alive past target exit
                    // until the user produces another keystroke. TtyReader
                    // also works on macOS PTY-slave fds where mio's AsyncFd
                    // rejects the registration with EINVAL.
                    let mut stdin = tui::tty::TtyReader::from_stdin().ok();
                    let mut stdout = tokio::io::stdout();
                    let mut stderr = tokio::io::stderr();
                    inner(
                        stdin
                            .as_mut()
                            .map(|s| s as &mut (dyn tokio::io::AsyncRead + Send + Sync + Unpin)),
                        Some(&mut stdout),
                        Some(&mut stderr),
                    )
                    .await
                })
            }))
        } else {
            None
        };

        let opts = ResultOptions {
            force: self.args.force,
            shell: self.args.shell.is_some(),
            interactive,
        };
        let rs = self
            .engine
            .new_state_with_events(!self.args.no_fail_fast, ctx.event_sender());

        let (result, failures) = match self.matcher {
            Matcher::Addr(addr) => {
                // Single top-level target: the matched set of one is known
                // immediately, so emit it as already-complete (no `~`).
                rs.emit(crate::engine::event::BuildEventKind::Matched {
                    addrs: vec![addr.format()],
                    complete: true,
                });
                let r = self
                    .engine
                    .clone()
                    .result_addr(rs, &addr, OutputMatcher::All, &opts)
                    .await?;
                (vec![r], Vec::new())
            }
            m => {
                let batch = self.engine.clone().result(rs, &m, &opts).await?;
                (batch.ok, batch.errors)
            }
        };

        tui::paused!(ctx, {
            if self.args.cat_out {
                for r in &result {
                    for a in &r.artifacts {
                        for e in a.walk()? {
                            let e = e?;
                            if let crate::hartifactcontent::WalkEntryKind::File {
                                mut data, ..
                            } = e.kind
                            {
                                io::copy(&mut data, &mut io::stdout())?;
                            }
                        }
                    }
                }
            } else if self.args.list_out {
                for r in &result {
                    for a in &r.artifacts {
                        for e in a.walk()? {
                            println!("{}", e?.path.display());
                        }
                    }
                }
            }
        });

        // Cancellation (Ctrl-C) is not a target failure — separate it from the
        // tally so a cancelled run doesn't report every in-flight target as
        // "failed", but still exits with an error (the build was aborted).
        let (cancelled, real): (Vec<_>, Vec<_>) = failures.iter().partition(|(_, e)| {
            crate::hmemoizer::downcast_chain_ref::<crate::engine::error::CancelledError>(e)
                .is_some()
        });
        if !real.is_empty() {
            anyhow::bail!("{} target(s) failed", real.len());
        }
        if !cancelled.is_empty() {
            anyhow::bail!("cancelled");
        }
        Ok(())
    }
}

pub fn execute(args: &RunArgs, sink: LogSink, no_tui: bool) -> anyhow::Result<()> {
    let rt = bootstrap::build_runtime().context("build tokio runtime")?;
    rt.block_on(execute_async(args.clone(), sink, no_tui))
}

async fn execute_async(args: RunArgs, sink: LogSink, no_tui: bool) -> anyhow::Result<()> {
    let base_pkg = get_cwp()?;
    let m = matcher_from_args(&args.arg1, &args.arg2, &args.exclude, &base_pkg, false)?;
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = RunApp {
        args,
        engine,
        matcher: m,
    };
    let interactive = tui::should_use_tui(no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}
