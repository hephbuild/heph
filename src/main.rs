use rheph::commands;
use rheph::log;

use clap::Parser;
use slog::info;

#[derive(Parser)]
#[command(name = "rheph")]
#[command(about = "A distributed CLI project using clap", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: commands::Commands,
}

fn main() {
    let _logger = log::init();
    info!(_logger, "Application starting"; "version" => env!("CARGO_PKG_VERSION"), "mode" => "cli");

    let cli = Cli::parse();
    match cli.command.execute() {
        Ok(_) => (),
        Err(e) => eprintln!("Error: {}", e),
    }
}
