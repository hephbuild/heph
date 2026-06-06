use std::io;
use std::sync::Arc;

use async_trait::async_trait;
use clap::Args;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::utils::matcher_from_args;
use crate::engine::{Engine, InteractiveWrapper, OutputMatcher, ResultOptions, get_cwp};
use crate::htmatcher::Matcher;
use crate::tui::{self, App, AppContext, LogSink};

#[derive(Args, Clone)]
#[command(override_usage = "heph run <TARGET_ADDRESS>\n       heph run <LABEL> <PACKAGE_MATCHER>")]
pub struct RunArgs {
    /// Target address (e.g., //pkg:name) OR Label
    #[arg(value_name = "TARGET_ADDRESS/LABEL")]
    pub arg1: String,
    /// Package matcher (only if first argument is a Label)
    #[arg(value_name = "PACKAGE_MATCHER")]
    pub arg2: Option<String>,
    /// Force execution, ignoring any cached result
    #[arg(long = "force")]
    pub force: bool,
    /// Drop into an interactive shell in the target's sandbox instead of running it
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
    /// Fail if generated output differs from the tree (CI check)
    #[arg(long = "frozen")]
    pub frozen: bool,
}

struct RunApp {
    args: RunArgs,
    engine: Arc<Engine>,
    matcher: Matcher,
    fail_fast: bool,
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
            frozen: self.args.frozen,
        };
        let rs = self
            .engine
            .new_state_full(self.fail_fast, ctx.event_sender(), ctx.bg_pending());

        // Fold both matcher paths into a single `res: Result<Vec<_>>` so the
        // `finalize!` paved road handles rendering and exit uniformly. The engine
        // already returns `Err` for cancellation and genuine top-level failures;
        // per-addr failures (default, fail-fast off) live in the request's failure registry.
        let res = match self.matcher {
            Matcher::Addr(addr) => self
                .engine
                .clone()
                .result_addr(rs.clone(), &addr, OutputMatcher::All, &opts)
                .await
                .map(|r| vec![r]),
            m => self
                .engine
                .clone()
                .result(rs.clone(), &m, &opts)
                .await
                .map(|batch| batch.ok),
        };

        // On success print `--cat-out` / `--list-out`; failures/cancellation are
        // rendered and turned into the right exit by the macro.
        crate::commands::errors::finalize!(ctx, rs, res, result => {
            if self.args.cat_out {
                for r in &result {
                    for a in &r.artifacts {
                        for e in a.walk()? {
                            let e = e?;
                            if let crate::hartifactcontent::WalkEntryKind::File { mut data, .. } =
                                e.kind
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
            Ok(())
        })
    }
}

pub fn execute(args: &RunArgs, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, global.clone()))?
}

async fn execute_async(args: RunArgs, sink: LogSink, global: GlobalOptions) -> anyhow::Result<()> {
    let base_pkg = get_cwp()?;
    let m = matcher_from_args(&args.arg1, &args.arg2, &args.exclude, &base_pkg, false)?;
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = RunApp {
        args,
        engine,
        matcher: m,
        fail_fast: global.fail_fast,
    };
    let interactive = tui::should_use_tui(global.no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}
