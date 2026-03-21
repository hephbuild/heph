pub mod create;
pub mod delete;

use clap::{Args, Subcommand};

#[derive(Args)]
pub struct UserArgs {
    /// Username
    pub username: Option<String>,

    /// Subcommand to execute
    #[command(subcommand)]
    pub command: Option<UserCommands>,
}

#[derive(Subcommand)]
pub enum UserCommands {
    /// Create a new user
    Create(create::CreateArgs),
    /// Delete an existing user
    Delete(delete::DeleteArgs),
}

impl UserArgs {
    pub fn execute(&self) {
        if let Some(user) = &self.username {
            println!("Dedicated handler for user: {}", user);
        }
        if let Some(cmd) = &self.command {
            cmd.execute();
        }
    }
}

impl UserCommands {
    pub fn execute(&self) {
        match self {
            UserCommands::Create(args) => create::execute(args),
            UserCommands::Delete(args) => delete::execute(args),
        }
    }
}
