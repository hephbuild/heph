use clap::parser::ValueSource;
use clap::{ArgMatches, CommandFactory, FromArgMatches, Parser};
use heph::commands;
use heph::commands::GlobalOptions;
use heph::log;
use std::process::ExitCode;
use std::time::Duration;
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

    // Telemetry decision is taken up front (config read), but the flush happens
    // at exit: this run's event is spooled, then we try to send it (plus any
    // backlog) within a short post-work budget. On CI the runner — and its
    // spool — is ephemeral, so there is no "next run" to defer to.
    let telemetry_on =
        heph::telemetry::is_enabled(commands::bootstrap::telemetry_enabled_from_config());
    let telemetry_ci = heph::telemetry::is_ci();
    let started_at = std::time::Instant::now();

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
    let failure = exec_result.as_ref().err().map(classify_failure);
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

    // Best-effort, opt-out usage telemetry: spool this invocation's event (one
    // sqlite insert), then try to send it now. Non-CI gives the send a 500ms
    // budget past the end of the work and defers the rest to the next run; CI
    // (ephemeral, no next run) blocks until the send completes. Never changes
    // the exit code.
    if telemetry_on {
        spool_telemetry(&matches, failure, started_at.elapsed());
        if telemetry_ci {
            heph::telemetry::flush_blocking();
        } else {
            heph::telemetry::flush_bounded(Duration::from_millis(500));
        }
    }

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
/// Command path and the set of flags come straight from the clap matches, so
/// coverage is exhaustive and automatic for every subcommand. Spool-only: one
/// sqlite insert, no network (a later run's background flush sends it).
fn spool_telemetry(matches: &ArgMatches, failure: Option<&'static str>, took: Duration) {
    // Full subcommand path ("inspect deps", "tool gc"), plus the args set at
    // every nesting level.
    let mut parts = Vec::new();
    let mut flags = set_args(matches);
    let mut level = matches;
    while let Some((name, sub)) = level.subcommand() {
        parts.push(name);
        flags.extend(set_args(sub));
        level = sub;
    }
    let command = if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join(" ")
    };
    flags.sort();
    flags.dedup();

    // Selector shape from the positional args present: two args (`arg1` +
    // `arg2`) is a label + package matcher; one is a single address. Structure
    // only — the actual label/address values are never read.
    let request_shape = if flags.iter().any(|f| f == "arg2") {
        "label_matcher"
    } else if flags.iter().any(|f| f == "arg1") {
        "addr"
    } else {
        "none"
    };

    heph::telemetry::enqueue_invocation(heph::telemetry::ReportContext {
        command: &command,
        request_shape,
        flags: &flags,
        success: failure.is_none(),
        failure,
        duration_ms: took.as_millis() as u64,
    });
}

/// Coarse, non-PII failure class for telemetry: user-cancelled vs a target that
/// genuinely failed vs anything else.
fn classify_failure(e: &anyhow::Error) -> &'static str {
    use heph::engine::error::{CancelledError, TargetFailure, UpstreamFailed};
    use heph::hmemoizer::downcast_chain_ref;
    if downcast_chain_ref::<CancelledError>(e).is_some() {
        "cancelled"
    } else if downcast_chain_ref::<TargetFailure>(e).is_some()
        || downcast_chain_ref::<UpstreamFailed>(e).is_some()
    {
        "target_failure"
    } else {
        "error"
    }
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
