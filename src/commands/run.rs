use std::io;
use std::sync::Arc;

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
}

struct RunApp {
    args: RunArgs,
    engine: Arc<Engine>,
    matcher: Matcher,
}

#[async_trait(?Send)]
impl App for RunApp {
    type Output = ();

    fn label(&self) -> String {
        match &self.matcher {
            Matcher::Addr(a) => format!("Running {}", a.format()),
            other => format!("Running {other:?}"),
        }
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
        let rs = self.engine.new_state();

        let result = match self.matcher {
            Matcher::Addr(addr) => {
                vec![
                    self.engine
                        .clone()
                        .result_addr(rs, &addr, OutputMatcher::All, &opts)
                        .await?,
                ]
            }
            m => self.engine.clone().result(rs, &m, &opts).await?,
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
            } else {
                println!("{} matched", result.len());
            }
        });

        Ok(())
    }
}

pub fn execute(args: &RunArgs, sink: LogSink, no_tui: bool) -> anyhow::Result<()> {
    execute_async(args.clone(), sink, no_tui)
}

#[tokio::main]
async fn execute_async(args: RunArgs, sink: LogSink, no_tui: bool) -> anyhow::Result<()> {
    let base_pkg = get_cwp()?;
    let m = matcher_from_args(&args.arg1, &args.arg2, &base_pkg, false)?;
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = RunApp {
        args,
        engine,
        matcher: m,
    };
    let interactive = tui::should_use_tui(no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}
