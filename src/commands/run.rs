use std::ffi::OsString;
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
use crate::engine::{
    Discovery, Engine, Gaps, InteractiveWrapper, InvokeOutcome, InvokeRequest, OutputMatcher,
    ResultOptions, get_cwp,
};
use crate::htmatcher::Matcher;
use crate::tui::{self, App, AppContext, LogSink};

/// Long help shared by `run` and `query`, documenting the `-e` query language.
pub const QUERY_LANG_HELP: &str = "\
Selecting targets:
  heph run //pkg:name                 a single target address
  heph run <label> //pkg/...          all targets with <label> under //pkg
  heph run -e '<expr>'                a query expression (see below)

  A label is [A-Za-z0-9_-]+ — the positional form takes one bare label, never
  an expression. For &&, ||, ! or grouping, use -e.

Query language (-e / --expr):
  Patterns:
    //pkg                package //pkg
    //pkg/...            every package under //pkg
    //pkg:name           one target address
    ./sub, ../x, .       relative to the current package
  Functions:
    label(x)             targets carrying label x   (e.g. label(go-lint))
    tree_output(pkg)     targets whose codegen tree writes into pkg
    driver(name)         targets run by that driver, whole name, case-sensitive
                         (e.g. driver(auth.credential))
    addr(//pkg:name)     an explicit target address
    package(//pkg)       an explicit package
    package_prefix(//pkg) every package under //pkg
  Any argument may be bare or quoted, with the same meaning: label(ci) is
  label(\"ci\"). A quoted argument takes escapes: \\\\ \\\" \\n \\r \\t \\u{..}
  Operators (precedence ! > && > ||, group with parentheses):
    a && b               both          a || b   either          !a   negate
  Evaluation follows grouping then left-to-right, bailing as early as possible.

  label(), driver() and tree_output() are decided from what providers list
  where they can, without resolving each target they rule out. `query` then
  resolves each match and prints only targets that exist; `query --candidates`
  skips that, which is faster but may print a listed target that does not. Set
  HEPH_NO_LISTED_FACTS to 1 to resolve every candidate instead; `heph validate`
  checks the listings.

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
    /// Supports &&, ||, !, parentheses, and the label()/driver()/tree_output() functions.
    /// Mutually exclusive with the positional TARGET arguments.
    #[arg(
        short = 'e',
        long = "expr",
        value_name = "EXPR",
        conflicts_with = "arg1"
    )]
    pub expr: Option<String>,
    /// Force execution, ignoring any cached result
    ///
    /// Applies to every target the selector matches, wherever the run first
    /// reaches it, including targets of a matched package that a listing does
    /// not show. Other dependencies still come from cache.
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
    /// Print only what the target prints: no TUI, no progress, stderr only for failures
    ///
    /// On an entrypoint, the program runs on your terminal's own fds rather than
    /// on heph's PTY, with no byte passing through heph. That is what gives it
    /// real Ctrl-Z and exact terminal behaviour, at the cost of a signal limit:
    /// see docs/ENTRYPOINTS.md. On a build target it changes only the display:
    /// the action's output is printed plainly, redacted as in `log.txt`, the
    /// action reads no stdin, and capture, runners and the cache are unchanged,
    /// so a cache hit prints nothing. Refused with `--shell`.
    #[arg(long = "raw")]
    pub raw: bool,
    /// Arguments for an entrypoint (`entrypoint = True`) that declares `args = True`,
    /// after `--`: appended to its command, verbatim. Everything after `--`
    /// is the program's, flags included (`heph run //t:go -- --version`).
    ///
    /// An entrypoint runs in your directory, with your environment, and the run
    /// ends as the program does. When stdin, stdout and stderr are all your
    /// terminal, it runs on a PTY of heph's: the keyboard and the screen are
    /// pumped through heph, and Ctrl-Z does nothing on a plain tool, since the
    /// kernel discards it. A tool that stops itself stops heph too, which gives
    /// your shell its prompt back; `bg` then leaves the tool stopped until `fg`.
    /// Otherwise, or with `--raw`, it inherits heph's fds. Exit status:
    /// the program's own code, unchanged (0 included); killed by a signal,
    /// and the run dies of the same signal; 125 when the run failed before
    /// the program started (a dependency, a refused argument, the closure,
    /// credentials, the spawn), with the error shown; 130 when Ctrl-C
    /// cancelled it before the program started; 1 when the target could not
    /// be resolved at all. Nothing but the program writes to stdout.
    ///
    /// Until the program starts, the run is like any other: Ctrl-C cancels
    /// it, TERM and HUP end it. While the program runs, every signal is the
    /// program's: Ctrl-C, Ctrl-\ and signals sent to the run's own pid reach
    /// it once (a `kill -INT` of the pid while the run owns the terminal is
    /// taken for the terminal's own; see docs/ENTRYPOINTS.md), and the run
    /// acts on none of them. After it exits, Ctrl-C, TERM or HUP cancel
    /// what the run is still finishing (cache uploads); the exit status stays
    /// the program's. A program that traps INT and QUIT is
    /// stopped with `kill -9` on the run's pid; its supervisor then reaps
    /// the program. `--shell` opens a shell with the same command ready, always
    /// on a PTY of heph's: `show` prints what `run` would execute.
    #[arg(last = true, value_name = "ARGS")]
    pub args: Vec<OsString>,
}

/// How an entrypoint run reaches the terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalMode {
    /// On a PTY of heph's, which pumps it to and from the caller's terminal.
    Pty,
    /// On heph's own fds.
    Inherit,
}

impl TerminalMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pty => "pty",
            Self::Inherit => "inherit",
        }
    }
}

/// Why an entrypoint run took its [`TerminalMode`]: the `reason` on the
/// `entrypoint_mode` event, which is how "which mode am I in?" gets answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModeReason {
    Default,
    Flag,
    Fd0NotTty,
    Fd1NotTty,
    Fd2NotTty,
    NotForeground,
    PluginNoPty,
    Shell,
}

impl ModeReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Flag => "flag",
            Self::Fd0NotTty => "fd0_not_tty",
            Self::Fd1NotTty => "fd1_not_tty",
            Self::Fd2NotTty => "fd2_not_tty",
            Self::NotForeground => "not_foreground",
            Self::PluginNoPty => "plugin_no_pty",
            Self::Shell => "shell",
        }
    }
}

/// What the mode of an entrypoint run depends on.
#[derive(Clone, Copy, Debug)]
struct ModeFacts {
    raw: bool,
    shell: bool,
    /// Whether fds 0, 1 and 2 are ttys.
    ttys: [bool; 3],
    /// Whether heph is the foreground job of the terminal on fd 0.
    foreground: bool,
    /// Whether the driver that spawns offered the PTY.
    offered: bool,
}

impl ModeFacts {
    /// Read off the process at the handover.
    fn probe(raw: bool, shell: bool, offered: bool) -> Self {
        use std::io::IsTerminal as _;
        Self {
            raw,
            shell,
            ttys: [
                io::stdin().is_terminal(),
                io::stdout().is_terminal(),
                io::stderr().is_terminal(),
            ],
            foreground: hproc::pty::is_foreground(libc::STDIN_FILENO),
            offered,
        }
    }
}

const RAW_WITH_SHELL: &str = "`--raw` and `--shell` cannot be combined: `--raw` keeps heph off the \
     terminal, and a shell needs heph's PTY; drop one of them";

/// `--raw` with `--shell`, refused before anything builds.
pub fn refuse_raw_with_shell(raw: bool, shell: bool) -> anyhow::Result<()> {
    anyhow::ensure!(!(raw && shell), RAW_WITH_SHELL);
    Ok(())
}

/// The mode of an entrypoint run, in the order the docs state it. `Err` is a
/// combination with no mode at all.
fn choose_mode(facts: &ModeFacts) -> anyhow::Result<(TerminalMode, ModeReason)> {
    refuse_raw_with_shell(facts.raw, facts.shell)?;
    if facts.raw {
        return Ok((TerminalMode::Inherit, ModeReason::Flag));
    }
    if facts.shell {
        // `Engine::invoke` refuses this first, with the driver's name; a slave
        // must still never reach a driver that did not offer to take it.
        anyhow::ensure!(
            facts.offered,
            "`--shell` runs on a PTY, and the driver that spawns does not offer one"
        );
        return Ok((TerminalMode::Pty, ModeReason::Shell));
    }
    let [fd0, fd1, fd2] = facts.ttys;
    let inherit = |reason| Ok((TerminalMode::Inherit, reason));
    if !fd0 {
        return inherit(ModeReason::Fd0NotTty);
    }
    if !fd1 {
        return inherit(ModeReason::Fd1NotTty);
    }
    if !fd2 {
        return inherit(ModeReason::Fd2NotTty);
    }
    if !facts.foreground {
        return inherit(ModeReason::NotForeground);
    }
    if !facts.offered {
        return inherit(ModeReason::PluginNoPty);
    }
    Ok((TerminalMode::Pty, ModeReason::Default))
}

/// Report the mode chosen: a `debug!` under its own target, so the default
/// filter renders nothing and `2>&1 | jq` stays clean, and a `warn!` only for a
/// run that wanted the PTY and could not have it.
fn announce_mode(mode: TerminalMode, reason: ModeReason) {
    tracing::debug!(
        target: "entrypoint_mode",
        mode = mode.as_str(),
        reason = reason.as_str(),
        "entrypoint_mode"
    );
    if reason == ModeReason::PluginNoPty {
        tracing::warn!(
            "the driver of this entrypoint offers no PTY; it runs on heph's own fds instead"
        );
    }
}

/// The caller's window size as it changes, for a build target's `--shell`, whose
/// PTY is the driver's. Stops with the wrapper that holds it.
struct WinsizeWatch {
    sizes: crate::engine::driver::WinSizeReceiver,
    task: tokio::task::JoinHandle<()>,
}

impl WinsizeWatch {
    /// `None` with no terminal to follow, or no way to hear from it: the shell
    /// then keeps the size it started with.
    fn start() -> Option<Self> {
        let initial = hproc::pty::caller_winsize()?;
        let mut winch =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()) {
                Ok(winch) => winch,
                Err(error) => {
                    tracing::warn!(%error, "not following window size changes");
                    return None;
                }
            };
        let (tx, sizes) = tokio::sync::watch::channel(initial);
        let task = tokio::spawn(async move {
            while winch.recv().await.is_some() {
                if let Some(size) = hproc::pty::caller_winsize() {
                    tx.send_replace(size);
                }
            }
        });
        Some(Self { sizes, task })
    }
}

impl Drop for WinsizeWatch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The program an entrypoint ran ended without success: heph exits as it did,
/// with its code, or by re-raising the signal that killed it. Carried out to
/// `main` as an error so the exit goes through its usual tail.
#[derive(Debug)]
pub struct ToolExit(pub hcore::process::Exit);

impl std::fmt::Display for ToolExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            hcore::process::Exit::Code(c) => write!(f, "exited with {c}"),
            hcore::process::Exit::Signal(s) => write!(f, "killed by signal {s}"),
        }
    }
}

impl std::error::Error for ToolExit {}

/// heph failed after the target resolved to an entrypoint (R4): a dep build,
/// a refused arg, materializing the closure, credentials, the spawn itself —
/// exit 125, with `error`'s box — or the run was cancelled before the tool
/// ran (Ctrl-C while its deps built) — exit 130. Either way the tool's own
/// status is not in it, so it can never be mistaken for one.
#[derive(Debug)]
pub struct InvokeFailed {
    pub code: u8,
    pub error: anyhow::Error,
}

/// Exit status for a heph-side failure after resolution (R4): outside the
/// range a tool usually uses for itself, as `env`/`timeout`/`git` use it.
pub const EXIT_HEPH_FAILED: u8 = 125;
/// Exit status for a run cancelled (Ctrl-C) before its tool started.
pub const EXIT_CANCELLED: u8 = 130;

impl std::fmt::Display for InvokeFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.error)
    }
}

impl std::error::Error for InvokeFailed {}

/// The header label, shared with the TUI view so it can change once the run
/// learns it is invoking an entrypoint rather than building (R20).
#[derive(Clone)]
struct SharedLabel(Arc<std::sync::Mutex<String>>);

impl SharedLabel {
    fn new(label: String) -> Self {
        Self(Arc::new(std::sync::Mutex::new(label)))
    }

    fn set(&self, label: String) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = label;
    }

    fn get(&self) -> String {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// [`tui::BuildHeader`] with a label that can change mid-run.
struct RunHeader {
    counts: tui::BuildHeader,
    label: SharedLabel,
}

impl tui::ProgressHeader for RunHeader {
    fn header(&self, core: &tui::BuildState, scope: tui::CountScope) -> Vec<tui::HeaderItem> {
        self.counts.header(core, scope)
    }

    fn label(&self) -> String {
        self.label.get()
    }

    fn last_render(&self, core: &tui::BuildState, scope: tui::CountScope) -> String {
        self.counts.last_render(core, scope)
    }
}

/// The caller's raw cwd and environment, captured before anything else ran:
/// what an entrypoint target runs with.
struct Caller {
    cwd: std::path::PathBuf,
    env: Vec<(OsString, OsString)>,
}

struct RunApp {
    args: RunArgs,
    engine: Arc<Engine>,
    matcher: Matcher,
    caller: Caller,
    fail_fast: bool,
    auto_approve: bool,
    /// `--no-scratch`: run with every declared scratch cache absent. Implies a
    /// forced rebuild — see where `force` is set.
    scratch_off: bool,
    /// Shared approval queue: attached to the TUI view (so prompts render) and to
    /// the TUI approval handler (so keypresses resolve them). Unused in non-TUI
    /// mode, which prompts on the terminal instead.
    approval: tui::ApprovalCenter,
    /// The TUI header's label: "Running …", or "Invoking …" once the target
    /// turns out to be an entrypoint.
    label: SharedLabel,
}

impl RunApp {
    fn progress_label(&self) -> String {
        format!("Running {}", self.selection())
    }

    fn selection(&self) -> String {
        match &self.matcher {
            Matcher::Addr(a) => a.format(),
            other => crate::htquery::format(other),
        }
    }
}

impl RunApp {
    /// An entrypoint: run it through `Engine::invoke`, the TUI paused for it,
    /// and hand how it ended out of the app so the binary exits as it did
    /// (R4): the tool's status as is, [`InvokeFailed`] for heph's own failure
    /// after resolution, and an ordinary error before it.
    ///
    /// Signals (R9): until the tool is about to be spawned — deps building,
    /// the closure materializing — heph is an ordinary run: Ctrl-C cancels,
    /// TERM and HUP end it, SIGQUIT dumps diagnostics. The handover makes it
    /// transparent while the tool runs (it cancels nothing, prints nothing,
    /// its diagnostic handlers stand down) and installs the relay's
    /// listeners, which forward every signal heph receives into the run; the
    /// driver delivers each to the tool. Once the tool exits (R27), signals
    /// are heph's again: Ctrl-C, TERM and HUP cancel what heph is still
    /// finishing (uploads), silently, and the exit status stays the tool's.
    async fn invoke(
        self,
        ctx: AppContext,
        rs: Arc<crate::engine::request_state::RequestState>,
        addr: crate::htaddr::Addr,
    ) -> anyhow::Result<Option<hcore::process::Exit>> {
        self.label.set(format!("Invoking {}", addr.format()));
        let engine = Arc::downgrade(&self.engine);
        let (relay, signals) = bootstrap::signal_relay(move || {
            if let Some(e) = engine.upgrade() {
                e.cancel_all_requests();
            }
        });
        // This invocation's transparency: taken at the handover, handed to
        // `tool_exited` once the tool is done.
        let held: Arc<std::sync::Mutex<Option<hcore::shutdown::TransparencyGuard>>> =
            Arc::default();
        // The pump, when the handover chose the PTY: started there, finished by
        // the wrapper below once the run returned.
        let session: Arc<std::sync::Mutex<Option<tui::PtySession>>> = Arc::default();
        let raw = self.args.raw;
        let shell = self.args.shell.is_some();
        let mut req = InvokeRequest::new(addr, self.caller.cwd, self.caller.env)
            .with_args(self.args.args)
            .with_shell(self.args.shell.is_some())
            .with_signals(signals)
            .with_dispositions(hcore::process::inherited_dispositions().to_vec())
            .with_nofile(hcore::process::inherited_nofile())
            .with_blocked(hcore::process::inherited_blocked().to_vec())
            .with_handover(crate::engine::driver::Handover::deciding({
                let held = Arc::clone(&held);
                let session = Arc::clone(&session);
                move |offered| {
                    let (mode, reason) = choose_mode(&ModeFacts::probe(raw, shell, offered))?;
                    // First: `Direct` is a synchronous stderr write, so the line
                    // precedes raw mode with no flush.
                    announce_mode(mode, reason);
                    // Transparent first: a Ctrl-C between the two must not
                    // cancel a run whose tool is about to own the terminal.
                    let guard = hcore::shutdown::hold_transparent();
                    *held.lock().unwrap_or_else(|e| e.into_inner()) = Some(guard);
                    if let Err(error) = relay.install() {
                        // The tool still runs; only signals sent to heph's own
                        // pid stop reaching it (the terminal's reach it anyway).
                        tracing::error!(error = ?error, "signal relay not installed");
                    }
                    if mode != TerminalMode::Pty {
                        return Ok(None);
                    }
                    let (pump, ends) =
                        tui::PtySession::start().context("starting the PTY session")?;
                    *session.lock().unwrap_or_else(|e| e.into_inner()) = Some(pump);
                    Ok(Some(crate::engine::driver::PtyHandover {
                        slave: ends.slave,
                        stops: ends.stops,
                        continues: ends.continues,
                    }))
                }
            }));
        // Neither the TUI nor the stall notice may draw over the tool, with or
        // without a TUI: nothing progresses while it runs, which is what the
        // watchdog reads as wedged. `Output`, not `Input`: this wraps the
        // whole driver run, materializing included, and until the handover a
        // Ctrl-C there must still cancel. From the handover the transparency
        // hold owns Ctrl-C, for a `--shell` session as much as for a tool.
        //
        // A tool has heph's own fds or the PTY the handover picked, so no stream
        // is handed in, whichever it was.
        let pauser = ctx.interactive().then(|| ctx.pauser());
        let pause: InteractiveWrapper = Arc::new(move |inner| {
            let pauser = pauser.clone();
            let session = Arc::clone(&session);
            Box::pin(async move {
                let _guard = match &pauser {
                    Some(p) => Some(p.pause_for(tui::PauseFor::Output).await),
                    None => None,
                };
                let _quiet = hengine::engine::diag::global().suppress_stall();
                let result = inner(None, None, None, None).await;
                // Before `_guard` resumes the TUI, and before any error renders:
                // this leaves the terminal cooked again, whatever the run returned.
                let pump = session.lock().unwrap_or_else(|e| e.into_inner()).take();
                if let Some(pump) = pump {
                    pump.finish().await;
                }
                result
            })
        });
        req = req.with_wrapper(pause);
        let outcome = self.engine.clone().invoke(rs.clone(), req).await;
        // Read before the tool is marked exited: a signal after that cancels
        // only what heph is finishing, never the run's own outcome.
        let cancelled = rs.ctoken().is_cancelled();
        let guard = held.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(guard) = guard {
            hcore::shutdown::tool_exited(guard);
        }
        match outcome {
            Ok(InvokeOutcome::Exited(exit)) => Ok(Some(exit)),
            Ok(InvokeOutcome::Failed(e)) => {
                // A cancellation reaches here as the driver's bare
                // "cancelled", not a `CancelledError`: the request's token is
                // what says whether this was Ctrl-C.
                let code = if cancelled {
                    EXIT_CANCELLED
                } else {
                    EXIT_HEPH_FAILED
                };
                let res: anyhow::Result<()> = Err(e);
                // In a block of its own: `finalize!` returns early with `?`,
                // which would skip the wrapping below.
                let finalized: anyhow::Result<()> =
                    async { crate::commands::errors::finalize!(ctx, rs, res) }.await;
                let error = match finalized {
                    Err(error) => error,
                    // `finalize!` swallows only a `CancelledError`.
                    Ok(()) => anyhow::anyhow!("cancelled"),
                };
                Err(InvokeFailed { code, error }.into())
            }
            Err(e) => {
                let res: anyhow::Result<()> = Err(e);
                crate::commands::errors::finalize!(ctx, rs, res)?;
                Ok(None)
            }
            Ok(other) => Err(anyhow::anyhow!("unknown invocation outcome {other:?}")),
        }
    }
}

#[async_trait]
impl App for RunApp {
    type Output = Option<hcore::process::Exit>;
    type TuiView = tui::TuiProgressView;
    type CiView = tui::CiProgressView;

    fn tui_view(&self) -> Self::TuiView {
        self.label.set(self.progress_label());
        tui::TuiProgressView::with_header(Box::new(RunHeader {
            counts: tui::BuildHeader::new(String::new()),
            label: self.label.clone(),
        }))
        .with_approval(self.approval.clone())
    }

    fn ci_view(&self) -> Self::CiView {
        tui::CiProgressView::new(self.progress_label())
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<Option<hcore::process::Exit>> {
        let interactive: Option<InteractiveWrapper> = if ctx.interactive() {
            let pauser = ctx.pauser();
            // Who owns Ctrl-C while the target holds the terminal. `--shell` puts
            // the user at a live shell inside the sandbox, and its keystrokes —
            // Ctrl-C first among them — are for that session, so the trigger stays
            // suppressed for its duration. A plain run is the opposite: nothing is
            // reading the keyboard, the terminal is merely on loan for the target's
            // output, and Ctrl-C must still cancel the run.
            let pause_for = if self.args.shell.is_some() {
                tui::PauseFor::Input
            } else {
                tui::PauseFor::Output
            };
            Some(Arc::new(move |inner| {
                let pauser = pauser.clone();
                Box::pin(async move {
                    let _guard = pauser.pause_for(pause_for).await;
                    // A `--shell` target parks a person at a prompt: nothing
                    // opens, closes or moves bytes for as long as they think,
                    // which is exactly what the stall watchdog reads as wedged.
                    // Without this it prints a stall paragraph over the shell
                    // they are typing into, and again every threshold after.
                    // `PauseFor::Output` is not suppressed: nobody is typing,
                    // the terminal is only on loan for the target's output, and
                    // a target that really is stuck there still deserves the
                    // notice.
                    let _quiet = (pause_for == tui::PauseFor::Input)
                        .then(|| hengine::engine::diag::global().suppress_stall());
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
                    // A `--shell`'s PTY is the driver's, so the size travels to it.
                    let winsize = (pause_for == tui::PauseFor::Input)
                        .then(WinsizeWatch::start)
                        .flatten();
                    inner(
                        stdin
                            .as_mut()
                            .map(|s| s as &mut (dyn tokio::io::AsyncRead + Send + Sync + Unpin)),
                        Some(&mut stdout),
                        Some(&mut stderr),
                        winsize.as_ref().map(|w| w.sizes.clone()),
                    )
                    .await
                })
            }))
        } else {
            None
        };

        // What the selector walk could not resolve. The run builds what it did
        // resolve, then reports the rest and exits non-zero; `--fail-fast` keeps
        // today's all-or-nothing walk instead.
        let gaps = Gaps::new(crate::htquery::format(&self.matcher));
        let opts = ResultOptions {
            // `--no-scratch` implies `--force`; the engine applies that from
            // `no_scratch` below, so it is not repeated here.
            force: self.args.force,
            shell: self.args.shell.is_some(),
            interactive,
            frozen: self.args.frozen,
            no_scratch: self.scratch_off,
            discovery: Discovery::keep_going_unless(self.fail_fast, &gaps),
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

        // An entrypoint is invoked, not built: it takes its own path from here.
        // An unresolvable address falls through, so the build path reports it
        // the way it reports any other. With args nothing will build, and it
        // is not known to be anything but unresolved: report that error here,
        // rather than saying a target nobody could read is not an entrypoint.
        if let Matcher::Addr(addr) = &self.matcher {
            match self.engine.is_entrypoint(&rs, addr).await {
                Ok(true) => {
                    let addr = addr.clone();
                    return self.invoke(ctx, rs, addr).await;
                }
                Err(e) if !self.args.args.is_empty() => {
                    let res: anyhow::Result<()> = Err(e);
                    crate::commands::errors::finalize!(ctx, rs, res)?;
                    return Ok(None);
                }
                Ok(false) | Err(_) => {}
            }
        }
        if !self.args.args.is_empty() {
            anyhow::bail!(
                "arguments after `--` are taken only by a single entrypoint target \
                 (`entrypoint = True`) that declares `args = True`; {} is not one",
                self.selection(),
            );
        }

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
                .and_then(crate::commands::errors::fold_batch)
                // A selector that chose nothing is a failed run, not a
                // successful empty one — see `require_non_empty`. Unless the
                // walk skipped something: then the skips are the answer, not
                // "check your selector".
                .and_then(|results| {
                    crate::commands::errors::require_non_empty_unless_incomplete(
                        results,
                        &gaps,
                        rs.listed_decided(),
                    )
                }),
        };

        // On success print `--cat-out` / `--list-out`; failures/cancellation are
        // rendered and turned into the right exit by the macro.
        let finalized: anyhow::Result<()> = crate::commands::errors::finalize!(ctx, rs, res, gaps = &gaps, result => {
            // Outputs are handed over only for a complete selection, exactly
            // as for one where a target failed: `--copy-out` into a directory
            // holding an earlier complete run would otherwise leave the
            // skipped targets' old outputs next to the new ones. (Not a
            // `return`: this block runs inside `finalize!`, which still has
            // to turn the skips into the exit.)
            let result = if gaps.is_empty() { result } else { Vec::new() };
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
        });
        finalized.map(|()| None)
    }
}

/// The two ways tool args end up where heph cannot use them (R18), refused
/// with what to type instead:
///
/// - `--` with no target before it (`heph run -- a b`): with `last = true`
///   everything after `--` is the tool's, so nothing names a target.
/// - a target address followed by a bare word (`heph run //t:go build`): the
///   positional pair is `<LABEL> <PACKAGE_MATCHER>`, and an address is no
///   label, so the word was meant for the tool. (A bare `--flag` after the
///   address is clap's to refuse, and its error already says `-- --flag`.)
fn refuse_misplaced_tool_args(
    args: &RunArgs,
    base_pkg: &crate::htpkg::PkgBuf,
) -> anyhow::Result<()> {
    if args.expr.is_none() && args.arg1.is_none() && !args.args.is_empty() {
        anyhow::bail!(
            "arguments after `--` need a target before it: `heph run //pkg:tool -- {}`",
            quote_args(&args.args)
        );
    }
    if args.expr.is_none()
        && let (Some(arg1), Some(arg2)) = (&args.arg1, &args.arg2)
        && crate::htaddr::parse_addr_with_base(arg1, base_pkg).is_ok()
    {
        anyhow::bail!(
            "`{arg1}` is a target address, so `{arg2}` is not a package matcher; to pass \
             arguments to an entrypoint, put them after `--`: `heph run {arg1} -- {arg2}`"
        );
    }
    Ok(())
}

/// Args for a hint, lossily and space-joined: only ever shown, never run.
fn quote_args(args: &[OsString]) -> String {
    args.iter()
        .map(|a| a.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn execute(args: &RunArgs, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, global.clone()))?
}

async fn execute_async(args: RunArgs, sink: LogSink, global: GlobalOptions) -> anyhow::Result<()> {
    refuse_raw_with_shell(args.raw, args.shell.is_some())?;
    // Raw, before anything else reads either: what an entrypoint runs with.
    // Not the engine's memoized cwd — the caller's cwd may be outside the
    // workspace, and an entrypoint runs there.
    let caller = Caller {
        cwd: std::env::current_dir().context("read the current directory")?,
        env: std::env::vars_os().collect(),
    };
    let base_pkg = get_cwp()?;
    refuse_misplaced_tool_args(&args, &base_pkg)?;
    let m = resolve_matcher(&args.expr, &args.arg1, &args.arg2, &base_pkg, false)?;
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = RunApp {
        args,
        engine: std::sync::Arc::clone(&engine),
        matcher: m,
        caller,
        fail_fast: global.fail_fast,
        auto_approve: global.auto_approve,
        scratch_off: global.no_scratch,
        approval: tui::ApprovalCenter::new(),
        label: SharedLabel::new(String::new()),
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
    // An entrypoint's tool ended: heph exits as it did, now that the TUI has
    // torn down.
    match result? {
        None | Some(hcore::process::Exit::Code(0)) => Ok(()),
        Some(exit) => Err(ToolExit(exit).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        run: RunArgs,
    }

    fn parse(argv: &[&str]) -> RunArgs {
        Cli::try_parse_from(std::iter::once("run").chain(argv.iter().copied()))
            .expect("parse")
            .run
    }

    /// Everything after `--` is the program's, in order and verbatim — heph's
    /// own flag names included, so `-- --quiet` reaches the tool (D18).
    #[test]
    fn args_after_double_dash_are_the_programs() {
        let args = parse(&["//t:go", "--", "--quiet", "", "-e", "--force", "x y"]);
        assert_eq!(args.arg1.as_deref(), Some("//t:go"));
        assert!(!args.force, "a flag after `--` is not heph's");
        assert_eq!(
            args.args,
            ["--quiet", "", "-e", "--force", "x y"].map(OsString::from)
        );
        assert!(
            parse(&["//t:go", "--"]).args.is_empty(),
            "a bare `--` is fine"
        );
        assert!(parse(&["//t:go"]).args.is_empty());
    }

    fn misplaced(argv: &[&str]) -> Option<String> {
        refuse_misplaced_tool_args(&parse(argv), &crate::htpkg::PkgBuf::from("app"))
            .err()
            .map(|e| format!("{e:#}"))
    }

    /// The selection parser rows (R18): what parses, what is refused with a
    /// hint before anything resolves, and what reaches the run (where the
    /// selection decides).
    #[test]
    fn selection_parser_rows() {
        // `-e` with args: parses; refused at run time unless the expression is
        // a single entrypoint address.
        let args = parse(&["-e", "//app:x", "--", "a"]);
        assert_eq!(args.expr.as_deref(), Some("//app:x"));
        assert_eq!(args.args, [OsString::from("a")]);
        assert_eq!(misplaced(&["-e", "//app:x", "--", "a"]), None);

        // A label and a package with args: parses as the label form; the run
        // refuses the args, since a selection is never an invocation.
        let args = parse(&["lint", "//app/...", "--", "a"]);
        assert_eq!(
            (args.arg1.as_deref(), args.arg2.as_deref()),
            (Some("lint"), Some("//app/..."))
        );
        assert_eq!(misplaced(&["lint", "//app/...", "--", "a"]), None);

        // `--` with no target before it: everything after is the tool's, so
        // nothing names a target.
        let args = parse(&["--", "//app:x", "a"]);
        assert_eq!(args.arg1, None, "`last = true`: `-- //x` names no target");
        let msg = misplaced(&["--", "//app:x", "a"]).expect("refused");
        assert!(msg.contains("need a target before it"), "{msg}");
        assert!(msg.contains("heph run //pkg:tool -- //app:x a"), "{msg}");

        // A bare word after an address was meant for the tool.
        let msg = misplaced(&["//app:x", "build"]).expect("refused");
        assert!(msg.contains("heph run //app:x -- build"), "{msg}");
        let msg = misplaced(&[":x", "build"]).expect("relative address too");
        assert!(msg.contains("put them after `--`"), "{msg}");

        // A bare flag after an address is clap's to refuse, and it says how.
        let err = Cli::try_parse_from(["run", "//app:x", "--verbose-tool"])
            .err()
            .expect("an unknown flag");
        let rendered = err.render().to_string();
        assert!(rendered.contains("-- --verbose-tool"), "{rendered}");
    }

    fn facts(
        raw: bool,
        shell: bool,
        ttys: [bool; 3],
        foreground: bool,
        offered: bool,
    ) -> ModeFacts {
        ModeFacts {
            raw,
            shell,
            ttys,
            foreground,
            offered,
        }
    }

    /// The rule of the mode, written out independently of `choose_mode`'s order
    /// of early returns: `None` is a refusal.
    fn expected(f: &ModeFacts) -> Option<(TerminalMode, ModeReason)> {
        use ModeReason as R;
        use TerminalMode::{Inherit, Pty};
        Some(match f {
            ModeFacts {
                raw: true,
                shell: true,
                ..
            } => return None,
            ModeFacts { raw: true, .. } => (Inherit, R::Flag),
            ModeFacts {
                shell: true,
                offered: false,
                ..
            } => return None,
            ModeFacts { shell: true, .. } => (Pty, R::Shell),
            ModeFacts {
                ttys: [false, _, _],
                ..
            } => (Inherit, R::Fd0NotTty),
            ModeFacts {
                ttys: [_, false, _],
                ..
            } => (Inherit, R::Fd1NotTty),
            ModeFacts {
                ttys: [_, _, false],
                ..
            } => (Inherit, R::Fd2NotTty),
            ModeFacts {
                foreground: false, ..
            } => (Inherit, R::NotForeground),
            ModeFacts { offered: false, .. } => (Inherit, R::PluginNoPty),
            _ => (Pty, R::Default),
        })
    }

    const BOOLS: [bool; 2] = [false, true];

    /// {`--raw`, `--shell`, neither} (and both, refused) × tty or not on each of
    /// fds 0, 1, 2 × foreground or not × offered or not, for an entrypoint and
    /// for a build target.
    #[test]
    fn mode_choice_table() {
        let mut rows = 0;
        for raw in BOOLS {
            for shell in BOOLS {
                for fd0 in BOOLS {
                    for fd1 in BOOLS {
                        for fd2 in BOOLS {
                            for foreground in BOOLS {
                                for offered in BOOLS {
                                    let f = facts(raw, shell, [fd0, fd1, fd2], foreground, offered);
                                    assert_eq!(choose_mode(&f).ok(), expected(&f), "{f:?}");
                                    rows += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(rows, 4 * 8 * 2 * 2);

        // The named rows: each inherits, on any partial redirect.
        for (name, ttys, reason) in [
            (
                "stdin tty, stdout a file",
                [true, false, true],
                ModeReason::Fd1NotTty,
            ),
            (
                "stdout tty, stdin a file",
                [false, true, true],
                ModeReason::Fd0NotTty,
            ),
            (
                "only stderr redirected",
                [true, true, false],
                ModeReason::Fd2NotTty,
            ),
        ] {
            assert_eq!(
                choose_mode(&facts(false, false, ttys, true, true)).ok(),
                Some((TerminalMode::Inherit, reason)),
                "{name}",
            );
        }
    }

    /// Everything [`announce_mode`] logs at debug level and above, targets shown.
    fn captured_log(f: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Buf(Arc<std::sync::Mutex<Vec<u8>>>);
        impl io::Write for Buf {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf::default();
        let writer = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8(buf.0.lock().expect("log buffer").clone()).expect("utf-8 log")
    }

    #[test]
    fn entrypoint_mode_event_reasons() {
        let on_a_terminal = [true, true, true];
        for (f, mode, reason) in [
            (
                facts(false, false, on_a_terminal, true, true),
                "pty",
                "default",
            ),
            (
                facts(true, false, on_a_terminal, true, true),
                "inherit",
                "flag",
            ),
            (
                facts(false, false, [false, true, true], true, true),
                "inherit",
                "fd0_not_tty",
            ),
            (
                facts(false, false, [true, false, true], true, true),
                "inherit",
                "fd1_not_tty",
            ),
            (
                facts(false, false, [true, true, false], true, true),
                "inherit",
                "fd2_not_tty",
            ),
            (
                facts(false, false, on_a_terminal, false, true),
                "inherit",
                "not_foreground",
            ),
            (
                facts(false, false, on_a_terminal, true, false),
                "inherit",
                "plugin_no_pty",
            ),
            (
                facts(false, true, [false, false, false], false, true),
                "pty",
                "shell",
            ),
        ] {
            let (m, r) = choose_mode(&f).expect("a mode");
            let log = captured_log(|| announce_mode(m, r));
            assert!(
                log.contains("DEBUG entrypoint_mode:")
                    && log.contains(&format!("mode=\"{mode}\""))
                    && log.contains(&format!("reason=\"{reason}\"")),
                "{f:?}: {log}"
            );
        }
    }

    /// The host's half; the engine's, that `offered` reflects the driver that
    /// spawns, is the test of this name in `crates/e2e/tests/entrypoint.rs`.
    #[test]
    fn pty_fallback_warns_when_plugin_offers_none() {
        let on_a_terminal = [true, true, true];
        let (mode, reason) =
            choose_mode(&facts(false, false, on_a_terminal, true, false)).expect("falls back");
        assert_eq!(mode, TerminalMode::Inherit);
        let log = captured_log(|| announce_mode(mode, reason));
        assert!(
            log.contains("WARN") && log.contains("offers no PTY"),
            "{log}"
        );

        // Nothing to warn of when the PTY was never in question.
        let (mode, reason) = choose_mode(&facts(false, false, [false, true, true], true, false))
            .expect("a redirected run inherits");
        let log = captured_log(|| announce_mode(mode, reason));
        assert!(!log.contains("WARN"), "{log}");

        // `--shell` has no inherited form.
        let err = choose_mode(&facts(false, true, on_a_terminal, true, false))
            .expect_err("nothing to fall back to");
        assert!(format!("{err:#}").contains("does not offer"), "{err:#}");
    }

    /// Non-UTF-8 args reach the tool byte for byte.
    #[test]
    fn non_utf8_args_are_kept() {
        use std::os::unix::ffi::OsStringExt as _;
        let raw = OsString::from_vec(vec![b'a', 0xff, b'b']);
        let args = Cli::try_parse_from([
            OsString::from("run"),
            OsString::from("//app:x"),
            OsString::from("--"),
            raw.clone(),
        ])
        .expect("parse")
        .run;
        assert_eq!(args.args, [raw]);
    }
}
