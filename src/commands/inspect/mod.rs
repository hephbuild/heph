mod deps;
mod hashin;
mod hashout;
mod packages;
mod spec;

use clap::{Args, Subcommand};

use crate::tui::LogSink;

#[derive(Args)]
pub struct InspectArgs {
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
    /// Prints target spec
    Spec(spec::Args),
    /// Prints target spec
    Deps(deps::Args),
}

impl InspectArgs {
    pub fn execute(&self, sink: LogSink, no_tui: bool) -> anyhow::Result<()> {
        if let Some(cmd) = &self.command {
            return cmd.execute(sink, no_tui);
        }

        Ok(())
    }
}

impl InspectCommands {
    pub fn execute(&self, sink: LogSink, no_tui: bool) -> anyhow::Result<()> {
        match self {
            InspectCommands::Packages(args) => packages::execute(args),
            InspectCommands::Hashin(args) => hashin::execute(args, sink, no_tui),
            InspectCommands::Hashout(args) => hashout::execute(args, sink, no_tui),
            InspectCommands::Spec(args) => spec::execute(args, sink, no_tui),
            InspectCommands::Deps(args) => deps::execute(args, sink, no_tui),
        }
    }
}
