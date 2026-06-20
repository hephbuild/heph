pub mod bootstrap;
pub mod completion;
pub mod errors;
pub mod gendocs;
mod global;
pub mod inspect;
pub mod query;
pub mod run;
pub mod tool;
mod utils;
pub mod validate;
pub mod version;

pub use global::GlobalOptions;

use clap::Subcommand;

use crate::tui::LogSink;

#[derive(Subcommand)]
pub enum Commands {
    /// Build and run targets
    ///
    /// Resolves the matched target(s), builds their dependency graph, and runs
    /// them — reusing cached results when inputs are unchanged. A single
    /// argument is a target address; two arguments are a label followed by a
    /// package matcher, selecting every target carrying that label. For richer
    /// selection use `-e '<expr>'` with the query language (see `--help`).
    ///
    /// Examples:
    ///
    /// `heph run //cmd/server:bin` — run a single target
    ///
    /// `heph run test //...` — run every target labelled `test`
    ///
    /// `heph run //cmd/server:bin --shell` — open a shell in the target sandbox
    ///
    /// `heph run -e '//cmd/... && label(test)'` — run via a query expression
    ///
    /// `heph run -e '//... && !//vendor/...'` — run, excluding a subtree
    #[command(visible_alias = "r")]
    Run(run::RunArgs),
    /// Inspect targets, packages, hashes, and deps
    ///
    /// Read-only introspection of the build graph. Subcommands print a target's
    /// spec, resolved def, input/output hashes, or dependencies, and list
    /// packages or provider functions. Nothing is executed unless a provider
    /// must run a target to answer the query.
    ///
    /// Examples:
    ///
    /// `heph inspect packages //...`
    ///
    /// `heph inspect deps //cmd/server:bin`
    ///
    /// `heph inspect hashin //cmd/server:bin`
    #[command(arg_required_else_help = true, visible_alias = "i")]
    Inspect(inspect::InspectArgs),
    /// Query targets
    ///
    /// Prints the address of every target matched by the argument(s), one per
    /// line. Accepts the same address / label+matcher forms as `run`, plus
    /// `-e '<expr>'` for the query language (see `--help`). Useful for scripting
    /// and for previewing what a selection matches before running it.
    ///
    /// Examples:
    ///
    /// `heph query //...`
    ///
    /// `heph query test //cmd/...`
    ///
    /// `heph query -e '//... && !//vendor/...'` — select with exclusion
    ///
    /// `heph query -e '//cmd/... && !label(slow)'` — select via a query
    #[command(visible_alias = "q")]
    Query(query::Args),
    /// Validate all targets (link graph + check codegen outputs)
    #[command(visible_alias = "v")]
    Validate(validate::ValidateArgs),
    /// Prints version
    ///
    /// Prints the heph version string and exits.
    Version(version::Args),
    /// Developer tools
    ///
    /// Maintenance and housekeeping subcommands that operate on the workspace
    /// or the local cache rather than on individual targets.
    Tool(tool::ToolArgs),
    /// Generate markdown CLI reference
    #[command(name = "gen-docs", hide = true)]
    GenDocs(gendocs::GenDocsArgs),
}

impl Commands {
    pub fn execute(&self, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
        match self {
            Commands::Run(args) => run::execute(args, sink, global),
            Commands::Inspect(args) => args.execute(sink, global),
            Commands::Query(args) => query::execute(args, sink, global),
            Commands::Validate(args) => validate::execute(args, sink, global),
            Commands::Version(args) => version::execute(args),
            Commands::Tool(args) => args.execute(sink, global),
            Commands::GenDocs(args) => gendocs::execute(args),
        }
    }
}
