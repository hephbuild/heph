pub mod bootstrap;
pub mod errors;
pub mod gc;
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
    /// Run a command
    #[command(visible_alias = "r")]
    Run(run::RunArgs),
    /// Inspect
    #[command(arg_required_else_help = true, visible_alias = "i")]
    Inspect(inspect::InspectArgs),
    /// Query targets
    #[command(visible_alias = "q")]
    Query(query::Args),
    /// Validate all targets (link graph + check codegen outputs)
    #[command(visible_alias = "v")]
    Validate(validate::ValidateArgs),
    /// Prints version
    Version(version::Args),
    /// Garbage collect the local cache
    Gc(gc::GcArgs),
    /// Developer tools
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
            Commands::Gc(args) => gc::execute(args, sink, global),
            Commands::Tool(args) => args.execute(sink, global),
            Commands::GenDocs(args) => gendocs::execute(args),
        }
    }
}
