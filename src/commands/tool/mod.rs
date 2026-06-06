pub mod gc;
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
    /// Garbage collect the local cache
    ///
    /// Sweeps the local cache (.heph3/cache) and removes artifacts no longer
    /// reachable from any current target, reclaiming disk space. Resolves every
    /// cached target's spec, so providers may run.
    ///
    /// Example: `heph tool gc`
    Gc(gc::GcArgs),
    /// Manage the heph-generated section of the root .gitignore
    ///
    /// Computes the ignore patterns for codegen-copy outputs and writes them
    /// into a managed block in the workspace root .gitignore, leaving the rest
    /// of the file untouched. Idempotent: a no-op when already up to date.
    ///
    /// Example: `heph tool gen-gitignore`
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
            ToolCommands::Gc(args) => gc::execute(args, sink, global),
            ToolCommands::GenGitignore(args) => gen_gitignore::execute(args, sink, global),
        }
    }
}
