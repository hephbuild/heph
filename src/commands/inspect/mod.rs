pub mod packages;

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
    Packages(packages::PackagesArgs),
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
        }
    }
}
