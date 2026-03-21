pub mod run;
pub mod user;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum Commands {
    /// Run a command
    Run(run::RunArgs),
    /// Manage users
    #[command(arg_required_else_help = true)]
    User(user::UserArgs),
}

impl Commands {
    pub fn execute(&self) {
        match self {
            Commands::Run(args) => run::execute(args),
            Commands::User(args) => args.execute(),
        }
    }
}
