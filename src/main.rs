use clap::{CommandFactory, FromArgMatches, Parser};
use heph::commands;
use heph::commands::GlobalOptions;
use heph::log;
use std::process::ExitCode;
use tracing::error;

use heph::diag;
mod pprof_dump;

/// Resolution is allocator-bound: a warm 85k-target `validate` spends ~15% of
/// its on-CPU samples inside the platform allocator, spread across every worker
/// thread. mimalloc's per-thread free lists take that traffic off the shared
/// arena locks that libmalloc/glibc-malloc serialize on.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(name = "heph")]
#[command(about = "An efficient build system", long_about = None)]
struct Cli {
    #[command(flatten)]
    global: GlobalOptions,
    #[command(subcommand)]
    command: commands::Commands,
}

fn main() -> ExitCode {
    // Hidden re-exec for the process supervisor sidecar. Must run BEFORE any
    // logging, tokio runtime, or clap parsing so the supervisor stays small
    // and predictable. Format: `heph __supervisor --ipc-fd <N>`.
    if let Some(fd) = parse_supervisor_args() {
        heph::process_supervisor::run_supervisor_main(fd);
    }

    // The two exec-runner subcommands, dispatched here for the same reason:
    // they must not pull in logging, a runtime, clap or the TUI. Unlike
    // `__supervisor` these are a pure prefix strip — `__runner-exec`'s argv IS
    // the target's argv, so parsing it as flags would misread the target's own
    // options — and they read `args_os`, because a non-UTF-8 filename would
    // panic `args()`.
    match parse_runner_args() {
        Some(RunnerInvocation::Client { argv }) => hexecrunner::agent::client_main(argv),
        Some(RunnerInvocation::Agent { socket }) => hexecrunner::agent::agent_main(socket),
        None => {}
    }

    // Dynamic shell completion. A no-op unless the `COMPLETE` env var is set
    // (a tab press or `heph tool completions` registration), in which case it
    // emits candidates / the registration script and exits the process. Runs
    // before the supervisor fork and logging init so a tab press never forks
    // the sidecar or writes log noise; the address completers spin up their
    // own short-lived engine on demand.
    clap_complete::CompleteEnv::with_factory(Cli::command).complete();

    // Ignore SIGTTOU/SIGTTIN so terminal-control syscalls (tcsetattr,
    // tcgetattr, tcsetpgrp …) from a background process group fail with
    // EIO instead of stopping the process. Test subprocesses can move the
    // foreground process group off heph3 (e.g. Go test runners that grab
    // the terminal) and exit without restoring; the next TUI raw-mode
    // toggle in `crossterm::terminal::enable_raw_mode` would otherwise
    // freeze the entire process (observed as a deadlock — every thread
    // stopped, tokio runtime appears hung). With these signals ignored,
    // the call returns an error that the TUI can surface.
    // SAFETY: signal(2) at process startup, before any thread spawns.
    unsafe {
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
    }
    // SAFETY: signal(2) at process startup, before any thread spawns.
    unsafe {
        libc::signal(libc::SIGTTIN, libc::SIG_IGN);
    }

    // Raise the open-file limit before any build work: heph holds an flock fd
    // per in-use cached artifact, and the default macOS soft limit (256) is
    // exhausted almost immediately on a wide build.
    heph::fdlimit::raise_open_file_limit();

    let sink = log::init();
    heph::tui::panic::install(sink.clone());

    // Self-upgrade BEFORE matching the command. A workspace can pin a newer heph
    // that defines subcommands this binary doesn't, and `get_matches()` below
    // hard-exits on an "unrecognized subcommand" — so if the upgrade ran after
    // matching, `heph <newcmd>` would die before the upgrade that adds <newcmd>
    // ever fired. Do a *soft* parse first (tolerates an unknown command), upgrade
    // (a successful upgrade re-execs into the pin and never returns), then do the
    // real parse below in whichever binary ends up running. The upgrade also runs
    // *before* the supervisor fork so a re-exec can't orphan a sidecar.
    //
    // A missing-workspace (`NoConfig`) result is fatal only when this binary
    // positively recognizes a real, non-`version` command: `version` must work
    // outside a workspace, and an unrecognized command (the forward-compat case)
    // falls through so the real parse can emit clap's canonical diagnostic.
    let soft_match = Cli::command().try_get_matches();
    match heph::selfupdate::maybe_self_upgrade() {
        Ok(()) => {}
        Err(heph::selfupdate::SelfUpgradeError::NoConfig) if no_config_tolerable(&soft_match) => {}
        Err(e) => {
            error!(error = %format!("{e:#}"), "Self-upgrade failed");
            return ExitCode::FAILURE;
        }
    }

    // Real parse — by here we've either re-exec'd into the pinned version (never
    // returns) or stayed on this binary. Hard-exits with clap's canonical
    // diagnostic on a genuinely unknown command or bad flags. Keeping the matches
    // also lets the telemetry reporter enumerate the exact args/flags set —
    // generically, for every command, with no per-command list.
    let matches = Cli::command().get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };

    // Fork the supervisor sidecar that will SIGKILL every tracked child
    // process group when this binary exits — including hard-kill scenarios.
    if let Err(e) = heph::process_supervisor::init() {
        error!(error = %format!("{e:#}"), "Failed to start process supervisor");
        return ExitCode::FAILURE;
    }

    // Telemetry decision is taken up front (config read), but the flush happens
    // at exit: this run's event is spooled, then we try to send it (plus any
    // backlog) within a short post-work budget. On CI the runner — and its
    // spool — is ephemeral, so there is no "next run" to defer to.
    let telemetry_on =
        heph::telemetry::is_enabled(commands::bootstrap::telemetry_enabled_from_config());
    // Warm the repo fingerprint off the hot path: the git root walk runs on a
    // detached thread now so the exit-time flush only reads the cached result.
    if telemetry_on {
        heph::telemetry::prewarm();
    }
    let started_at = std::time::Instant::now();

    // Always-on hang diagnostics. An opt-in dumper is useless for the hang you
    // did not anticipate — you would have to have passed a flag on the run that
    // is already stuck. Costs one `signal(2)` at startup; the file is opened
    // lazily inside the handler.
    // Applied before any engine is constructed, since it decides whether the
    // engine resolves scratch caches at all.
    heph::commands::bootstrap::set_scratch_enabled(
        cli.global.scratch == heph::commands::ScratchMode::On,
    );

    diag::install(cli.global.diag_backtrace);

    // When `--pprof-cpu` is set, start the sampler + `SIGUSR2` dump watcher.
    let pprof_watcher = match cli.global.pprof_cpu.clone() {
        Some(path) => match pprof_dump::start(path) {
            Ok(watcher) => Some(watcher),
            Err(e) => {
                error!(error = %format!("{e:#}"), "Failed to start CPU profiler");
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };

    let exec_result = cli.command.execute(sink, &cli.global);
    let result = match &exec_result {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            // Render a graphical diagnostic if the error chain carries one
            // (Starlark/Go diagnostics from single-error commands). The TUI is
            // already torn down here, so plain stderr output is safe. Fall back
            // to the one-line log when nothing renderable is found.
            if !heph::commands::errors::render_anyhow(e) {
                error!(error = %format!("{:#}", e), "Failed");
            }
            ExitCode::FAILURE
        }
    };

    // Best-effort, opt-out usage telemetry: record this invocation's event and
    // try to send it (bounded; CI blocks). Never changes the exit code.
    if telemetry_on {
        heph::telemetry::record_invocation(
            &matches,
            &Cli::command(),
            exec_result.as_ref().err(),
            started_at.elapsed(),
        );
    }

    // Flush the final (filtered) report and stop the watcher.
    if let Some(watcher) = pprof_watcher {
        watcher.shutdown();
    }

    result
}

/// The hidden exec-runner invocations.
enum RunnerInvocation {
    /// `heph __runner-exec -- <program> <args…>` — the per-target client heph
    /// forks in the target's place.
    Client { argv: Vec<std::ffi::OsString> },
    /// `heph __runner-agent --socket <path>` — the long-lived helper that lives
    /// inside a held-open environment.
    Agent { socket: std::path::PathBuf },
}

/// Detect either exec-runner subcommand.
///
/// `args_os`, not `args`: the client's argv is the target's own, and a target
/// argument that is not valid UTF-8 is legal on every supported target — a
/// `String`-based scan would panic the client before it ever reached the agent.
fn parse_runner_args() -> Option<RunnerInvocation> {
    let mut args = std::env::args_os().skip(1);
    let first = args.next()?;
    if first == hexecrunner::agent::CLIENT_SUBCOMMAND {
        // Everything after the `--` separator is the target's, verbatim.
        let sep = args.next()?;
        if sep != "--" {
            return None;
        }
        return Some(RunnerInvocation::Client {
            argv: args.collect(),
        });
    }
    if first == hexecrunner::agent::AGENT_SUBCOMMAND {
        let flag = args.next()?;
        if flag != "--socket" {
            return None;
        }
        return Some(RunnerInvocation::Agent {
            socket: std::path::PathBuf::from(args.next()?),
        });
    }
    None
}

/// Detect the hidden `__supervisor --ipc-fd <N>` invocation without dragging
/// clap into a hot path that runs at every startup.
fn parse_supervisor_args() -> Option<i32> {
    let mut args = std::env::args().skip(1);
    if args.next()? != "__supervisor" {
        return None;
    }
    let flag = args.next()?;
    if flag != "--ipc-fd" {
        return None;
    }
    args.next()?.parse::<i32>().ok()
}

/// Commands that run outside a workspace. They neither read `.hephconfig` nor
/// touch the build graph, so a missing workspace must not block them: `version`
/// prints the build string, `gen-docs` emits the CLI reference from clap.
const NO_CONFIG_COMMANDS: &[&str] = &["version", "gen-docs"];

/// Whether a `NoConfig` (not-in-a-workspace) self-upgrade result may be tolerated
/// for this invocation, decided from a soft pre-parse of the CLI args.
///
/// Tolerated unless the args name a real, workspace-requiring command. The
/// commands in `NO_CONFIG_COMMANDS` run anywhere, and an *unrecognized* command
/// (the `Err` case — a workspace pin may add it) falls through to the real parse,
/// which surfaces clap's canonical error. Any other recognized command
/// legitimately needs a workspace, so its `NoConfig` stays fatal.
fn no_config_tolerable(soft_match: &Result<clap::ArgMatches, clap::Error>) -> bool {
    match soft_match {
        Ok(m) => m
            .subcommand_name()
            .is_some_and(|name| NO_CONFIG_COMMANDS.contains(&name)),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn soft(args: &[&str]) -> Result<clap::ArgMatches, clap::Error> {
        Cli::command().try_get_matches_from(args)
    }

    #[test]
    fn version_tolerates_missing_workspace() {
        // `heph version` must keep working outside a workspace.
        assert!(no_config_tolerable(&soft(&["heph", "version"])));
    }

    #[test]
    fn gen_docs_tolerates_missing_workspace() {
        // `heph gen-docs` only renders clap's CLI reference — no workspace needed.
        assert!(no_config_tolerable(&soft(&["heph", "gen-docs"])));
    }

    #[test]
    fn unknown_command_tolerates_missing_workspace() {
        // Forward-compat: a workspace pin may add this command. Upgrading is the
        // whole point, so a NoConfig here must not be fatal — the real parse
        // afterward surfaces clap's "unrecognized subcommand" diagnostic.
        assert!(no_config_tolerable(&soft(&["heph", "totally-new-cmd"])));
    }

    #[test]
    fn real_command_requires_workspace() {
        // A recognized, non-`version` command run outside a workspace is a genuine
        // error — keep NoConfig fatal so it isn't silently tolerated.
        assert!(!no_config_tolerable(&soft(&["heph", "validate"])));
    }
}
