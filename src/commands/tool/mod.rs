pub mod gc;

use clap::{Args, Subcommand};

use crate::commands::GlobalOptions;
use crate::tui::LogSink;

#[derive(Args)]
pub struct ToolArgs {
    /// Subcommand to execute
    #[command(subcommand)]
    pub command: Option<ToolCommands>,
}

#[derive(Subcommand)]
pub enum ToolCommands {
    /// Garbage collect the local cache
    Gc(gc::GcArgs),
}

impl ToolArgs {
    pub fn execute(&self, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
        if let Some(cmd) = &self.command {
            return cmd.execute(sink, global);
        }

        Ok(())
    }
}

impl ToolCommands {
    pub fn execute(&self, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
        match self {
            ToolCommands::Gc(args) => gc::execute(args, sink, global),
        }
    }
}
