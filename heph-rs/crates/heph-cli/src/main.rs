//! Heph CLI binary entry point

use clap::Parser;
use heph_cli::Cli;
use std::process;

fn main() {
    let cli = Cli::parse();

    if let Err(e) = cli.execute() {
        eprintln!("Error: {}", e);
        process::exit(1);
    }
}
