pub mod bootstrap;
pub mod errors;
pub mod gc;
mod global;
pub mod inspect;
pub mod query;
pub mod run;
mod utils;
pub mod version;

pub use global::GlobalOptions;

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
    /// Garbage collect the local cache
    Gc(gc::GcArgs),
}

impl Commands {
    pub fn execute(&self, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
        match self {
            Commands::Run(args) => run::execute(args, sink, global),
            Commands::Inspect(args) => args.execute(sink, global),
            Commands::Query(args) => query::execute(args, sink, global),
            Commands::Version(args) => version::execute(args),
            Commands::Gc(args) => gc::execute(args, sink, global),
        }
    }
}
