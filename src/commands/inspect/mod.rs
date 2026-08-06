mod def;
mod deps;
mod deps_explorer;
mod functions;
mod hashin;
mod hashout;
mod labels;
mod outputs;
mod packages;
mod path;
mod revdeps;
mod spec;
mod states;

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
    /// List the unique labels declared across matching targets
    ///
    /// Enumerates every target in the packages matching the given matcher,
    /// collects the labels each declares, and prints the sorted, deduplicated
    /// set one per line. With no argument, scans the whole workspace.
    ///
    /// Examples:
    ///
    /// `heph inspect labels`
    ///
    /// `heph inspect labels //cmd/...`
    Labels(labels::Args),
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
    /// Print the paths a target actually produces
    ///
    /// Runs the target (or reads its cached result) and prints every path in
    /// its output artifacts, one per line — the paths as a consumer sees them
    /// in its sandbox, after any filtering or relocation.
    ///
    /// This is the answer to "why did that file land there?" for a `group`
    /// target using `include`/`exclude`/`strip_prefix`/`prefix`/`rename`:
    /// compare the group's paths with its deps' to see exactly what the
    /// transform did.
    ///
    /// Examples:
    ///
    /// `heph inspect outputs //cmd/server:bin`
    ///
    /// `heph inspect outputs //cmd:dist --json` — also lists support files
    Outputs(outputs::Args),
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
    /// Print the chain of targets linking two targets
    ///
    /// Prints the shortest chain of hops linking A and B, one target per line.
    /// Argument order does not matter: both directions are searched, and the
    /// chain is printed from the dependent to the dependency. Hops follow each
    /// target's resolved deps, so a dep pulled in by another dep's transitives
    /// counts; pass --no-transitive to follow only the directly declared ones.
    /// When the two are unconnected, nothing is printed on stdout — the reason
    /// is logged instead.
    ///
    /// Examples:
    ///
    /// `heph inspect path //cmd/server:bin //lib:core`
    ///
    /// `heph inspect path //lib:core //cmd/server:bin` — same chain
    ///
    /// `heph inspect path //cmd/server:bin main.go`
    ///
    /// `heph inspect path //cmd/server:bin //lib:core --no-transitive`
    Path(path::Args),
    /// Show the `provider_state(...)` declared across the package tree
    ///
    /// A `provider_state(provider="X", …)` call in a BUILD file configures
    /// provider `X` for that package and — depending on the provider — its
    /// descendants. This prints where those declarations live and what they
    /// carry: a `//pkg` header per package, then one line per state as
    /// `<provider>  <field>=<json> …`. Packages declaring nothing are omitted,
    /// so empty output means "no state here".
    ///
    /// Pass --inherited to see the whole chain a provider is handed for a
    /// package — its own declarations plus every ancestor's — each line
    /// prefixed with the package that declared it, root first. Which of two
    /// declarations wins is the provider's own policy; this only reports what
    /// applies.
    ///
    /// Examples:
    ///
    /// `heph inspect states` — every declaration in the workspace
    ///
    /// `heph inspect states //cmd/...`
    ///
    /// `heph inspect states //cmd/server --inherited` — what applies there
    ///
    /// `heph inspect states -p go --json`
    States(states::Args),
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
            InspectCommands::Labels(args) => labels::execute(args, sink, global),
            InspectCommands::Hashin(args) => hashin::execute(args, sink, global),
            InspectCommands::Hashout(args) => hashout::execute(args, sink, global),
            InspectCommands::Outputs(args) => outputs::execute(args, sink, global),
            InspectCommands::Spec(args) => spec::execute(args, sink, global),
            InspectCommands::Def(args) => def::execute(args, sink, global),
            InspectCommands::Deps(args) => deps::execute(args, sink, global),
            InspectCommands::Revdeps(args) => revdeps::execute(args, sink, global),
            InspectCommands::Path(args) => path::execute(args, sink, global),
            InspectCommands::States(args) => states::execute(args, sink, global),
            InspectCommands::Functions(args) => functions::execute(args, sink, global),
        }
    }
}
