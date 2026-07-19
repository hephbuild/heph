use clap::{CommandFactory, FromArgMatches, Parser};
use heph::commands;
use heph::commands::GlobalOptions;
use heph::log;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;
use tracing::{error, info, warn};

/// Set by the `SIGUSR2` handler; polled by the pprof-dump watcher thread. Lets a
/// stuck/hung run be profiled in place: `kill -USR2 <pid>` writes the CPU profile
/// accumulated so far to the `--pprof-cpu` path, without waiting for exit (which
/// a hang never reaches) or needing ptrace/core dumps (both blocked in locked-down
/// CI containers).
static PPROF_DUMP_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Set by `main` after the command finishes so the watcher writes a final report
/// and exits.
static PPROF_SHUTDOWN: AtomicBool = AtomicBool::new(false);

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

    // When `--pprof-cpu` is set, start the sampler and hand the guard to a
    // dedicated watcher thread. The watcher writes the profile on `SIGUSR2`
    // (mid-run, for hangs) and once more at shutdown (filtered).
    let pprof_watcher = match cli.global.pprof_cpu.clone() {
        Some(path) => match pprof::ProfilerGuardBuilder::default()
            .frequency(1000)
            .build()
        {
            Ok(guard) => {
                install_pprof_dump_signal();
                Some(spawn_pprof_watcher(guard, path))
            }
            Err(e) => {
                error!(error = %e, "Failed to start CPU profiler");
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

    // Signal the watcher to write its final (filtered) report and exit.
    if let Some(handle) = pprof_watcher {
        PPROF_SHUTDOWN.store(true, Ordering::Relaxed);
        if let Err(e) = handle.join() {
            warn!("pprof watcher thread panicked: {e:?}");
        }
    }

    result
}

/// `SIGUSR2` handler: request an on-demand CPU-profile dump. Only stores to an
/// atomic, so it is async-signal-safe.
extern "C" fn pprof_sigusr2(_sig: libc::c_int) {
    PPROF_DUMP_REQUESTED.store(true, Ordering::Relaxed);
}

/// Install the `SIGUSR2` → dump-request handler. Called once, before the tokio
/// runtime starts.
fn install_pprof_dump_signal() {
    let handler = pprof_sigusr2 as extern "C" fn(libc::c_int);
    // SAFETY: the handler only stores to an `AtomicBool` (async-signal-safe),
    // and this runs once at startup before other threads matter.
    unsafe {
        libc::signal(libc::SIGUSR2, handler as libc::sighandler_t);
    }
}

/// Own the profiler guard on a dedicated thread. Poll for `SIGUSR2` dump
/// requests (mid-run snapshots for diagnosing hangs) and, on shutdown, write a
/// final report with the tokio/std runtime frames filtered out.
fn spawn_pprof_watcher(guard: pprof::ProfilerGuard<'static>, path: PathBuf) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("pprof-dump".to_string())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(200));
                if PPROF_DUMP_REQUESTED.swap(false, Ordering::Relaxed) {
                    // Unfiltered on purpose: a hang might be *in* the runtime, so
                    // show every frame.
                    dump_pprof(&guard, &path, false);
                }
                if PPROF_SHUTDOWN.load(Ordering::Relaxed) {
                    break;
                }
            }
            dump_pprof(&guard, &path, true);
        })
        .expect("spawn pprof-dump thread")
}

/// Build the current pprof report and write it to `path`. When `filter_runtime`
/// is set, drop pure tokio/std scheduler frames (the exit-time report); the
/// on-demand dump keeps everything.
fn dump_pprof(guard: &pprof::ProfilerGuard<'_>, path: &Path, filter_runtime: bool) {
    use pprof::protos::Message;
    let report = if filter_runtime {
        guard
            .report()
            .frames_post_processor(|frames| {
                frames.frames.retain(|syms| {
                    syms.iter().all(|s| {
                        let name = s.name();
                        !name.starts_with("tokio::runtime")
                            && !name.starts_with("tokio::task")
                            && !name.starts_with("tokio::park")
                            && !name.starts_with("tokio::loom")
                            && !name.starts_with("tokio::time::driver")
                            && !name.starts_with("std::thread")
                            && !name.starts_with("std::panicking")
                            && !name.starts_with("_pthread")
                            && !name.starts_with("__pthread")
                    })
                });
            })
            .build()
    } else {
        guard.report().build()
    };
    let report = match report {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "Failed to build CPU profile report");
            return;
        }
    };
    let profile = match report.pprof() {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "Failed to build pprof profile");
            return;
        }
    };
    let mut content = Vec::new();
    if let Err(e) = profile.encode(&mut content) {
        warn!(error = %e, "Failed to encode pprof profile");
        return;
    }
    match std::fs::write(path, &content) {
        Ok(()) => info!(path = %path.display(), "CPU profile written"),
        Err(e) => warn!(path = %path.display(), error = %e, "Failed to write pprof file"),
    }
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
