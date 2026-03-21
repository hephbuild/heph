pub mod run;
pub mod inspect;
mod bootstrap;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum Commands {
    /// Run a command
    Run(run::RunArgs),
    /// Inspect
    #[command(arg_required_else_help = true)]
    Inspect(inspect::InspectArgs),
}

impl Commands {
    pub fn execute(&self) -> anyhow::Result<()>  {
        match self {
            Commands::Run(args) => run::execute(args),
            Commands::Inspect(args) => args.execute(),
        }
    }
}
