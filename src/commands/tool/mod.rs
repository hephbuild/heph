mod gen_gitignore;

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
    /// Manage the heph-generated section of the root .gitignore
    #[command(name = "gen-gitignore")]
    GenGitignore(gen_gitignore::Args),
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
            ToolCommands::GenGitignore(args) => gen_gitignore::execute(args, sink, global),
        }
    }
}
