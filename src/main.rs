use std::process::ExitCode;
use std::time::Instant;
use rheph::commands;
use rheph::log;
use rheph::defer;

use clap::Parser;
use slog::{error, info};
use humantime::format_duration;

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
        },
    }
}
