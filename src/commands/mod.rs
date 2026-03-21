pub mod add;
pub mod greet;
pub mod user;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum Commands {
    /// Adds two numbers
    Add(add::AddArgs),
    /// Greets a person
    Greet(greet::GreetArgs),
    /// Manage users
    #[command(arg_required_else_help = true)]
    User(user::UserArgs),
}

impl Commands {
    pub fn execute(&self) {
        match self {
            Commands::Add(args) => add::execute(args),
            Commands::Greet(args) => greet::execute(args),
            Commands::User(args) => args.execute(),
        }
    }
}
