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
    let start = Instant::now();
    let sink = log::init();
    rheph::tui::panic::install(sink.clone());
    info!(version = VERSION, "Application starting");
    defer! {
        info!(duration = %format_duration(start.elapsed()), "Application finished");
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
