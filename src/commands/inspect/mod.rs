mod def;
mod deps;
mod hashin;
mod hashout;
mod packages;
mod spec;

use clap::{Args, Subcommand};

use crate::commands::GlobalOptions;
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
    /// Prints target def
    Def(def::Args),
    /// Prints target deps
    Deps(deps::Args),
}

impl InspectArgs {
    pub fn execute(&self, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
        if let Some(cmd) = &self.command {
            return cmd.execute(sink, global);
        }

        Ok(())
    }
}

impl InspectCommands {
    pub fn execute(&self, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
        match self {
            InspectCommands::Packages(args) => packages::execute(args, sink, global),
            InspectCommands::Hashin(args) => hashin::execute(args, sink, global),
            InspectCommands::Hashout(args) => hashout::execute(args, sink, global),
            InspectCommands::Spec(args) => spec::execute(args, sink, global),
            InspectCommands::Def(args) => def::execute(args, sink, global),
            InspectCommands::Deps(args) => deps::execute(args, sink, global),
        }
    }
}
