pub mod create;
pub mod delete;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum UserCommands {
    /// Create a new user
    Create(create::CreateArgs),
    /// Delete an existing user
    Delete(delete::DeleteArgs),
}

impl UserCommands {
    pub fn execute(&self) {
        match self {
            UserCommands::Create(args) => create::execute(args),
            UserCommands::Delete(args) => delete::execute(args),
        }
    }
}
