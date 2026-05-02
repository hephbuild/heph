mod packages;
mod hashin;
mod hashout;

use clap::{Args, Subcommand};

#[derive(Args)]
pub struct InspectArgs {
    /// Username
    pub username: Option<String>,

    /// Subcommand to execute
    #[command(subcommand)]
    pub command: Option<InspectCommands>,
}

#[derive(Subcommand)]
pub enum InspectCommands {
    /// List packages
    Packages(packages::Args),
    /// Prints targets hashin
    Hashin(hashin::Args),
    /// Prints targets hashout
    Hashout(hashout::Args),
}

impl InspectArgs {
    pub fn execute(&self) -> anyhow::Result<()> {
        if let Some(user) = &self.username {
            println!("Dedicated handler for user: {}", user);
        }
        if let Some(cmd) = &self.command {
            return cmd.execute()
        }

        Ok(())
    }
}

impl InspectCommands {
    pub fn execute(&self) -> anyhow::Result<()>  {
        match self {
            InspectCommands::Packages(args) => packages::execute(args),
            InspectCommands::Hashin(args) => hashin::execute(args),
            InspectCommands::Hashout(args) => hashout::execute(args),
        }
    }
}
