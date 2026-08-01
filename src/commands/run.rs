use std::io;
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use clap::Args;
use clap_complete::engine::ArgValueCompleter;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::commands::utils::resolve_matcher;
use crate::engine::{Engine, InteractiveWrapper, OutputMatcher, ResultOptions, get_cwp};
use crate::htmatcher::Matcher;
use crate::tui::{self, App, AppContext, LogSink};

/// Long help shared by `run` and `query`, documenting the `-e` query language.
pub const QUERY_LANG_HELP: &str = "\
Selecting targets:
  heph run //pkg:name                 a single target address
  heph run <label> //pkg/...          all targets with <label> under //pkg
  heph run -e '<expr>'                a query expression (see below)

Query language (-e / --expr):
  Patterns:
    //pkg                package //pkg
    //pkg/...            every package under //pkg
    //pkg:name           one target address
    ./sub, ../x, .       relative to the current package
  Functions:
    label(x)             targets carrying label x   (e.g. label(\"//tag:release\"))
    tree_output(pkg)     targets whose codegen tree writes into pkg
    addr(//pkg:name)     an explicit target address
    package(//pkg)       an explicit package
    package_prefix(//pkg) every package under //pkg
  Operators (precedence ! > && > ||, group with parentheses):
    a && b               both          a || b   either          !a   negate
  Evaluation follows grouping then left-to-right, bailing as early as possible.

  Examples:
    heph run -e '//some/... && label(foo)'
    heph run -e '//app/... && !label(slow)'
    heph run -e '//... && !//vendor/...'
    heph run -e '(//a/... || //b/...) && tree_output(gen)'
";

#[derive(Args, Clone)]
#[command(
    override_usage = "heph run <TARGET_ADDRESS>\n       heph run <LABEL> <PACKAGE_MATCHER>\n       heph run -e <EXPR>",
    after_long_help = QUERY_LANG_HELP
)]
pub struct RunArgs {
    /// Target address (e.g., //pkg:name) OR Label
    #[arg(value_name = "TARGET_ADDRESS/LABEL", add = ArgValueCompleter::new(complete_target_addr))]
    pub arg1: Option<String>,
    /// Package matcher (only if first argument is a Label)
    #[arg(value_name = "PACKAGE_MATCHER")]
    pub arg2: Option<String>,
    /// Select targets with a query expression, e.g. -e '//pkg/... && !//vendor/...'.
    /// Supports &&, ||, !, parentheses, and the label()/tree_output() functions.
    /// Mutually exclusive with the positional TARGET arguments.
    #[arg(
        short = 'e',
        long = "expr",
        value_name = "EXPR",
        conflicts_with = "arg1"
    )]
    pub expr: Option<String>,
    /// Force execution, ignoring any cached result
    #[arg(long = "force")]
    pub force: bool,
    /// Drop into an interactive shell in the target's sandbox instead of running it
    ///
    /// The terminal goes to the single target you name, never to its
    /// dependencies. A group of exactly one member *is* that member, so it can
    /// be shelled into; a group of two or more inlines its members as
    /// dependencies of the run, and dependencies are not interactive. Shell
    /// into the member you want instead. Likewise for a multi-target selection,
    /// which names no single target at all.
    #[arg(long = "shell", num_args = 0..=1, require_equals = true, default_missing_value = "", value_name = "TARGET",)]
    pub shell: Option<String>,
    /// Print output artifacts to stdout
    #[arg(long = "cat-out", conflicts_with = "list_out")]
    pub cat_out: bool,
    /// Print output file list to stdout
    #[arg(long = "list-out")]
    pub list_out: bool,
    /// Copy output artifacts into DIR (created if needed).
    /// Relative paths resolve against the current directory.
    #[arg(long = "copy-out", value_name = "DIR")]
    pub copy_out: Option<String>,
    /// Restrict --cat-out/--list-out/--copy-out to these output names
    /// (repeatable). When omitted, all of the target's outputs are considered.
    #[arg(long = "output", value_name = "NAME")]
    pub output: Vec<String>,
    /// Fail if generated output differs from the tree (CI check)
    #[arg(long = "frozen")]
    pub frozen: bool,
    /// Number of trailing process-log lines to show in a failing target's
    /// diagnostic box. The full log is always saved as the `log.txt` artifact.
    #[arg(long = "log-lines", value_name = "N", default_value_t = 10)]
    pub log_lines: usize,
}

struct RunApp {
    args: RunArgs,
    engine: Arc<Engine>,
    matcher: Matcher,
    fail_fast: bool,
    auto_approve: bool,
    /// Shared approval queue: attached to the TUI view (so prompts render) and to
    /// the TUI approval handler (so keypresses resolve them). Unused in non-TUI
    /// mode, which prompts on the terminal instead.
    approval: tui::ApprovalCenter,
}

impl RunApp {
    fn progress_label(&self) -> String {
        match &self.matcher {
            Matcher::Addr(a) => format!("Running {}", a.format()),
            other => format!("Running {}", crate::htquery::format(other)),
        }
    }
}

#[async_trait]
impl App for RunApp {
    type Output = ();
    type TuiView = tui::TuiProgressView;
    type CiView = tui::CiProgressView;

    fn tui_view(&self) -> Self::TuiView {
        tui::TuiProgressView::new(self.progress_label()).with_approval(self.approval.clone())
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
        // In the interactive TUI the prompt renders on the live view and `y`/`n`
        // resolve it; otherwise the notice prints to stderr and the decision is
        // read from the terminal (or auto-approved).
        let approval: Arc<dyn crate::engine::approval::ApprovalHandler> = if ctx.interactive() {
            Arc::new(crate::commands::approval::TuiApprovalHandler::new(
                self.approval.clone(),
                self.auto_approve,
            ))
        } else {
            Arc::new(crate::commands::approval::CliApprovalHandler::new(
                self.auto_approve,
            ))
        };
        let rs = self.engine.new_state_full(
            self.fail_fast,
            ctx.event_sender(),
            ctx.bg_pending(),
            self.args.log_lines,
            Some(approval),
        );

        // Fold both matcher paths into a single `res: Result<Vec<_>>` so the
        // `finalize!` paved road handles rendering and exit uniformly. The engine
        // already returns `Err` for cancellation and genuine top-level failures;
        // per-addr failures (default, fail-fast off) live in the request's failure registry.
        let outputs = if self.args.output.is_empty() {
            OutputMatcher::All
        } else {
            OutputMatcher::Exact(self.args.output.clone())
        };

        let res = match self.matcher {
            Matcher::Addr(addr) => self
                .engine
                .clone()
                .result_addr(rs.clone(), &addr, outputs, &opts)
                .await
                .map(|r| vec![r]),
            m => self
                .engine
                .clone()
                .result(rs.clone(), &m, outputs, &opts)
                .await
                // NOT `.map(|batch| batch.ok)`: keep-going failures ride
                // inside the Ok and must fold into `res`, or an error the
                // registry didn't record exits 0 in silence. See `fold_batch`.
                .and_then(crate::commands::errors::fold_batch),
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
            if let Some(dir) = &self.args.copy_out {
                let dst = std::path::Path::new(dir);
                let dst = if dst.is_absolute() {
                    dst.to_path_buf()
                } else {
                    std::env::current_dir()
                        .context("resolve current directory for --copy-out")?
                        .join(dst)
                };
                std::fs::create_dir_all(&dst)
                    .with_context(|| format!("create --copy-out dir {:?}", dst))?;
                for r in &result {
                    for a in &r.artifacts {
                        crate::hartifactcontent::unpack::unpack(a.as_ref(), &dst, None, None)
                            .with_context(|| format!("copy output into {:?}", dst))?;
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
    let m = resolve_matcher(&args.expr, &args.arg1, &args.arg2, &base_pkg, false)?;
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = RunApp {
        args,
        engine: std::sync::Arc::clone(&engine),
        matcher: m,
        fail_fast: global.fail_fast,
        auto_approve: global.auto_approve,
        approval: tui::ApprovalCenter::new(),
    };
    let interactive = tui::should_use_tui(global.no_tui);

    // Stall watchdog. Registered before the run so it observes the whole stream.
    //
    // The paragraph goes to `<home>/diag/stall-<pid>.log` and the terminal gets
    // one `warn!` naming it. Inline, the table repeated on every escalation
    // buries the build output it is meant to annotate — and it grows exactly when
    // the terminal is least readable. The `warn!` goes through `tracing`, hence
    // the same `LogSink` as every other log, so while the TUI owns the terminal
    // it lands as a proper line instead of interleaving mid-frame.
    //
    // It runs in both TUI and `--no-tui` mode: the CI backend creates the same
    // event channel, so the fold is already paid for, and a hung CI build is
    // precisely where nobody is watching a progress bar.
    let watchdog = (!global.stall_notice.is_zero()).then(|| {
        let threshold = global.stall_notice;
        let diag_sink = sink.clone();
        let log = hengine::engine::diag::StallLog::new(&engine.home);
        let inflight = hengine::engine::diag::InflightLog::new(&engine.home);
        hengine::engine::diag::Watchdog::spawn(
            std::sync::Arc::clone(hengine::engine::diag::global()),
            threshold,
            move |report, snapshot| {
                // Best-effort and first: the paragraph points at this file, and
                // a hang that ends with the process being killed takes every
                // byte of in-flight state with it unless it was already on disk.
                // A failure here must not cost us the paragraph itself.
                if let Err(e) =
                    inflight.write(&hengine::engine::diag::InflightLog::render(snapshot))
                {
                    tracing::warn!(
                        path = %inflight.path().display(),
                        error = %e,
                        "Cannot write the in-flight report"
                    );
                }
                let text = hengine::engine::diag::render_stall(report);
                if let Err(e) = log.append(&text) {
                    // Read-only fs, full disk, gc'd home. The paragraph is the
                    // whole point of the watchdog, so print it rather than lose
                    // it to the failure of its own delivery mechanism.
                    tracing::warn!(
                        path = %log.path().display(),
                        error = %e,
                        "Cannot write the stall diagnostic; printing it instead"
                    );
                    diag_sink.write_diagnostic(&text);
                    return;
                }
                tracing::warn!(
                    path = %log.path().display(),
                    quiet_for_s = report.quiet_for.as_secs(),
                    open = report.open.iter().map(|(_, n, _)| n).sum::<u64>(),
                    "No progress; wrote a stall diagnostic"
                );
            },
        )
    });

    let result = tui::run_app(app, sink, interactive, shutdown).await;
    // Stop before teardown so no paragraph races the final summary.
    if let Some(w) = &watchdog {
        w.stop();
    }
    // The app's request state has dropped now (firing each hook's `on_close`);
    // await any hook's final out-of-process flush before returning so a process
    // exit never races it.
    engine.await_hooks().await;
    result
}
