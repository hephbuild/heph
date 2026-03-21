mod commands;

use clap::Parser;

#[derive(Parser)]
#[command(name = "rheph")]
#[command(about = "A distributed CLI project using clap", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: commands::Commands,
}

fn main() {
    let cli = Cli::parse();
    cli.command.execute();
}
