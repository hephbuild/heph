pub mod bootstrap;
pub mod inspect;
pub mod query;
pub mod run;
mod utils;
pub mod version;

use clap::Subcommand;

use crate::tui::LogSink;

#[derive(Subcommand)]
pub enum Commands {
    /// Run a command
    #[command(visible_alias = "r")]
    Run(run::RunArgs),
    /// Inspect
    #[command(arg_required_else_help = true, visible_alias = "i")]
    Inspect(inspect::InspectArgs),
    /// Query targets
    #[command(visible_alias = "q")]
    Query(query::Args),
    /// Prints version
    Version(version::Args),
}

impl Commands {
    pub fn execute(&self, sink: LogSink, no_tui: bool) -> anyhow::Result<()> {
        match self {
            Commands::Run(args) => run::execute(args, sink, no_tui),
            Commands::Inspect(args) => args.execute(sink, no_tui),
            Commands::Query(args) => query::execute(args, sink, no_tui),
            Commands::Version(args) => version::execute(args),
        }
    }
}
