mod def;
mod deps;
mod deps_explorer;
mod functions;
mod hashin;
mod hashout;
mod packages;
mod revdeps;
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
    /// List packages matching a matcher
    ///
    /// Walks providers to discover packages and prints those matching the given
    /// matcher, one per line. With no argument, lists every package in the
    /// workspace.
    ///
    /// Examples:
    ///
    /// `heph inspect packages`
    ///
    /// `heph inspect packages //cmd/...`
    Packages(packages::Args),
    /// Print a target's input hash
    ///
    /// Computes and prints the content hash of all the target's declared
    /// inputs — the key heph uses to decide a cache hit. Does not run the
    /// target.
    ///
    /// Example: `heph inspect hashin //cmd/server:bin`
    Hashin(hashin::Args),
    /// Print a target's output hashes
    ///
    /// Runs the target (or reads its cached result) and prints the content hash
    /// of each output artifact, one per line.
    ///
    /// Example: `heph inspect hashout //cmd/server:bin`
    Hashout(hashout::Args),
    /// Print a target's spec, as supplied by its provider
    ///
    /// Prints the raw spec — the unresolved definition a provider returns
    /// before a driver parses it — as pretty JSON.
    ///
    /// Example: `heph inspect spec //cmd/server:bin`
    Spec(spec::Args),
    /// Print a target's resolved def (inputs, outputs, sandbox)
    ///
    /// Parses the target's spec into a def and prints it as pretty JSON,
    /// including declared inputs, outputs, and sandbox configuration. By
    /// default transitive deps are applied; pass --no-transitive for the
    /// direct def only.
    ///
    /// Examples:
    ///
    /// `heph inspect def //cmd/server:bin`
    ///
    /// `heph inspect def //cmd/server:bin --no-transitive`
    Def(def::Args),
    /// Print a target's input dependencies
    ///
    /// Resolves the target's def and prints the ref of each declared input,
    /// one per line. Pass -i/--interactive to browse the dependency tree in a
    /// TUI.
    ///
    /// Examples:
    ///
    /// `heph inspect deps //cmd/server:bin`
    ///
    /// `heph inspect deps //cmd/server:bin -i`
    Deps(deps::Args),
    /// Print the targets that depend on a target ("where is this used?")
    ///
    /// The reverse of `deps`: scans the workspace (or `--scope` packages) and
    /// prints every target that declares the given target as a direct input,
    /// one per line.
    ///
    /// Examples:
    ///
    /// `heph inspect revdeps //lib:core`
    ///
    /// `heph inspect revdeps //lib:core --scope //cmd/...`
    Revdeps(revdeps::Args),
    /// List provider-exposed functions (`heph.<provider>.<fn>`)
    ///
    /// Prints every function registered by a provider for use in BUILD files,
    /// in `heph.<provider>.<function>` form, one per line.
    ///
    /// Example: `heph inspect functions`
    Functions(functions::Args),
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
            InspectCommands::Revdeps(args) => revdeps::execute(args, sink, global),
            InspectCommands::Functions(args) => functions::execute(args, sink, global),
        }
    }
}
