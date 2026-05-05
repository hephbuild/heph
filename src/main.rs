use rheph::commands;
use rheph::defer;
use rheph::log;
use std::process::ExitCode;
use std::time::Instant;

use clap::Parser;
use humantime::format_duration;
use slog::{error, info};

#[derive(Parser)]
#[command(name = "rheph")]
#[command(about = "An efficient build system", long_about = None)]
struct Cli {
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
    match cli.command.execute() {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            error!(_logger, "Failed"; "error" => %format!("{:#}", e));

            ExitCode::FAILURE
        }
    }
}
