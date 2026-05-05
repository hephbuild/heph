pub mod run;
pub mod inspect;
pub mod query;
mod bootstrap;
mod utils;
mod version;

use clap::Subcommand;

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
    pub fn execute(&self) -> anyhow::Result<()>  {
        match self {
            Commands::Run(args) => run::execute(args),
            Commands::Inspect(args) => args.execute(),
            Commands::Query(args) => query::execute(args),
            Commands::Version(args) => version::execute(args),
        }
    }
}
