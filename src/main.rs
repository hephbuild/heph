use clap::parser::ValueSource;
use clap::{ArgMatches, CommandFactory, FromArgMatches, Parser};
use heph::commands;
use heph::commands::GlobalOptions;
use heph::log;
use std::process::ExitCode;
use tracing::{error, info, warn};

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

    // Fork the supervisor sidecar that will SIGKILL every tracked child
    // process group when this binary exits — including hard-kill scenarios.
    if let Err(e) = heph::process_supervisor::init() {
        error!(error = %format!("{e:#}"), "Failed to start process supervisor");
        return ExitCode::FAILURE;
    }

    // Parse once into `ArgMatches`, then into the typed `Cli`. Keeping the
    // matches lets the telemetry reporter enumerate the exact args/flags that
    // were set — generically, for every command, with no per-command list.
    let matches = Cli::command().get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };

    let pprof_guard = if cli.global.pprof_cpu.is_some() {
        match pprof::ProfilerGuardBuilder::default()
            .frequency(1000)
            .build()
        {
            Ok(guard) => Some(guard),
            Err(e) => {
                error!(error = %e, "Failed to start CPU profiler");
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };

    let exec_result = cli.command.execute(sink, &cli.global);
    let success = exec_result.is_ok();
    let result = match exec_result {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            // Render a graphical diagnostic if the error chain carries one
            // (Starlark/Go diagnostics from single-error commands). The TUI is
            // already torn down here, so plain stderr output is safe. Fall back
            // to the one-line log when nothing renderable is found.
            if !heph::commands::errors::render_anyhow(&e) {
                error!(error = %format!("{:#}", e), "Failed");
            }
            ExitCode::FAILURE
        }
    };

    // Best-effort, opt-out usage telemetry. Runs once for whichever command
    // executed, after it (and its TUI) has finished. Never affects the exit.
    report_telemetry(&matches, success);

    if let (Some(guard), Some(path)) = (pprof_guard, cli.global.pprof_cpu) {
        match guard
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
        {
            Err(e) => warn!(error = %e, "Failed to build CPU profile report"),
            Ok(report) => {
                use pprof::protos::Message;
                match report.pprof() {
                    Err(e) => warn!(error = %e, "Failed to build pprof profile"),
                    Ok(profile) => {
                        let mut content = Vec::new();
                        match profile.encode(&mut content) {
                            Err(e) => {
                                warn!(error = %e, "Failed to encode pprof profile")
                            }
                            Ok(()) => match std::fs::write(&path, &content) {
                                Err(e) => {
                                    warn!(path = %path.display(), error = %e, "Failed to write pprof file")
                                }
                                Ok(()) => {
                                    info!(path = %path.display(), "CPU profile written")
                                }
                            },
                        }
                    }
                }
            }
        }
    }

    result
}

/// Send one best-effort, personless telemetry event for whichever command ran.
/// Command name and the set of flags come straight from the clap matches, so
/// coverage is exhaustive and automatic for every subcommand. Runs on a tiny
/// throwaway runtime (the command's own runtime is already gone) with a short
/// timeout, and swallows every failure.
fn report_telemetry(matches: &ArgMatches, success: bool) {
    if !heph::telemetry::is_enabled(commands::bootstrap::telemetry_enabled_from_config()) {
        return;
    }

    let command = matches.subcommand_name().unwrap_or("none").to_string();
    let mut flags = set_args(matches);
    if let Some((_, sub)) = matches.subcommand() {
        flags.extend(set_args(sub));
    }
    flags.sort();
    flags.dedup();

    let snapshot = heph::telemetry::snapshot();
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    rt.block_on(heph::telemetry::report(
        heph::telemetry::ReportContext {
            command: &command,
            flags: &flags,
            success,
        },
        snapshot,
    ));
}

/// Names of the args/flags explicitly set on the command line for one match
/// level (top-level globals, or a subcommand's args). Names only — never the
/// values, which can carry addresses/labels.
fn set_args(m: &ArgMatches) -> Vec<String> {
    m.ids()
        .filter(|id| m.value_source(id.as_str()) == Some(ValueSource::CommandLine))
        .map(|id| id.as_str().to_string())
        .collect()
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
