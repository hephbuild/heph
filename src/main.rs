use clap::{Args, Parser};
use humantime::format_duration;
use rheph::commands;
use rheph::defer;
use rheph::log;
use rheph::version::VERSION;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;
use tracing::{error, info, warn};

#[derive(Args)]
struct GlobalArgs {
    /// Write CPU pprof on exit
    #[arg(long = "pprof-cpu", value_name = "PATH", global = true)]
    pprof_cpu: Option<PathBuf>,
    /// Disable the interactive TUI (force CI/log-only output)
    #[arg(long = "no-tui", global = true)]
    no_tui: bool,
}

#[derive(Parser)]
#[command(name = "rheph")]
#[command(about = "An efficient build system", long_about = None)]
struct Cli {
    #[command(flatten)]
    global: GlobalArgs,
    #[command(subcommand)]
    command: commands::Commands,
}

fn main() -> ExitCode {
    // Hidden re-exec for the process supervisor sidecar. Must run BEFORE any
    // logging, tokio runtime, or clap parsing so the supervisor stays small
    // and predictable. Format: `rheph __supervisor --ipc-fd <N>`.
    if let Some(fd) = parse_supervisor_args() {
        rheph::process_supervisor::run_supervisor_main(fd);
    }

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

    let start = Instant::now();
    let sink = log::init();
    rheph::tui::panic::install(sink.clone());
    info!(version = VERSION, "Application starting");
    defer! {
        info!(duration = %format_duration(start.elapsed()), "Application finished");
    }

    // Fork the supervisor sidecar that will SIGKILL every tracked child
    // process group when this binary exits — including hard-kill scenarios.
    if let Err(e) = rheph::process_supervisor::init() {
        error!(error = %format!("{e:#}"), "Failed to start process supervisor");
        return ExitCode::FAILURE;
    }

    let cli = Cli::parse();

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

    let result = match cli.command.execute(sink, cli.global.no_tui) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            error!(error = %format!("{:#}", e), "Failed");
            ExitCode::FAILURE
        }
    };

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
