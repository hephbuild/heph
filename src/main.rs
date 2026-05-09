use clap::{Args, Parser};
use humantime::format_duration;
use rheph::commands;
use rheph::defer;
use rheph::log;
use slog::{error, info, warn};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

#[derive(Args)]
struct GlobalArgs {
    /// Write CPU pprof on exit
    #[arg(long = "pprof-cpu", value_name = "PATH", global = true)]
    pprof_cpu: Option<PathBuf>,
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
    let _logger = log::init();
    info!(_logger, "Application starting"; "version" => env!("CARGO_PKG_VERSION"), "mode" => "cli");
    defer! {
        info!(_logger, "Application finished"; "duration" => %format_duration(start.elapsed()));
    }

    let cli = Cli::parse();

    let pprof_guard = if cli.global.pprof_cpu.is_some() {
        match pprof::ProfilerGuardBuilder::default()
            .frequency(1000)
            .build()
        {
            Ok(guard) => Some(guard),
            Err(e) => {
                error!(_logger, "Failed to start CPU profiler"; "error" => %e);
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };

    let result = match cli.command.execute() {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            error!(_logger, "Failed"; "error" => %format!("{:#}", e));
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
            Err(e) => warn!(_logger, "Failed to build CPU profile report"; "error" => %e),
            Ok(report) => {
                use pprof::protos::Message;
                match report.pprof() {
                    Err(e) => warn!(_logger, "Failed to build pprof profile"; "error" => %e),
                    Ok(profile) => {
                        let mut content = Vec::new();
                        match profile.encode(&mut content) {
                            Err(e) => {
                                warn!(_logger, "Failed to encode pprof profile"; "error" => %e)
                            }
                            Ok(()) => match std::fs::write(&path, &content) {
                                Err(e) => {
                                    warn!(_logger, "Failed to write pprof file"; "path" => %path.display(), "error" => %e)
                                }
                                Ok(()) => {
                                    info!(_logger, "CPU profile written"; "path" => %path.display())
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
