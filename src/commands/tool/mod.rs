mod build_fmt;
mod build_lsp;
mod cache;
mod clean;
mod completions;
mod coreutils;
pub mod gc;
mod gen_gitignore;
mod resolve_plugins;
mod scratch;

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
    /// Remove the selected targets' entries from the local cache
    ///
    /// Deletes every locally cached revision of the selected target(s) — all of
    /// them, whatever their `cache.history` budget and whether or not the target
    /// still exists. `gc` is the sweep that reclaims space without being told
    /// what; this is the one you point at something.
    ///
    /// Targets are selected exactly as for `run`: an address, a label followed
    /// by a package matcher, or `-e '<expr>'` (see `--help`). The selection is
    /// required — there is no whole-cache default, since this command deletes.
    /// A selection that needs no resolution — an address, or `all <package
    /// matcher>` — is answered from the cache alone: no BUILD files are read, so
    /// an entry whose target has since been deleted is still cleanable.
    /// `label(...)` resolves the graph.
    ///
    /// Examples:
    ///
    /// `heph tool clean //cmd/server:bin` — one target (that exact variant)
    ///
    /// `heph tool clean all //cmd/...` — every cached target under a subtree
    ///
    /// `heph tool clean test //cmd/...` — every target labelled `test`
    ///
    /// `heph tool clean all //...` — clear the entire local cache
    Clean(clean::CleanArgs),
    /// Manage the heph-generated section of the root .gitignore
    ///
    /// Computes the ignore patterns for codegen-copy outputs and writes them
    /// into a managed block in the workspace root .gitignore, leaving the rest
    /// of the file untouched. Idempotent: a no-op when already up to date.
    ///
    /// Example: `heph tool gen-gitignore`
    #[command(name = "gen-gitignore")]
    GenGitignore(gen_gitignore::Args),
    /// Inspect and reclaim persistent scratch caches
    ///
    /// A scratch cache is a directory a target declares and carries between runs
    /// to go faster — a compiler cache, a dependency cache. Nothing else bounds
    /// their growth, so this is where you look when disk is short, and
    /// `heph tool gc` is what sweeps them on a schedule.
    ///
    /// Removing one is always safe: a target's outputs are identical whether its
    /// scratch is warm, cold or absent, so it costs time and nothing else.
    ///
    /// Example: `heph tool scratch ls`
    Scratch(scratch::ScratchArgs),
    /// Remote cache maintenance
    ///
    /// Subcommands for the shared remote cache(s) configured in `.hephconfig2`.
    ///
    /// Example: `heph tool cache measure-latency`
    Cache(cache::CacheArgs),
    /// Inspect the POSIX utilities compiled into this binary
    ///
    /// The binary carries its own `cp`, `install`, `sha256sum` and friends so a
    /// recipe behaves the same on Linux and macOS, and puts them on every
    /// target's `PATH`. These subcommands say what is in the set and what a
    /// name resolves to.
    ///
    /// Example: `heph tool coreutils list`
    Coreutils(coreutils::CoreutilsArgs),
    /// Print a shell completion-registration script
    ///
    /// Emits the script that enables dynamic tab-completion of subcommands,
    /// flags, and target addresses for the given shell. Source it from your
    /// shell rc, e.g. `source <(heph tool completions zsh)`.
    ///
    /// Example: `heph tool completions bash`
    Completions(completions::Args),
    /// Format BUILD files
    ///
    /// Reformats the Starlark `BUILD` files of every package, or only those
    /// matching a package matcher. Rewrites in place by default; `--check`
    /// reports unformatted files and exits non-zero without writing. Pass `-`
    /// to format stdin to stdout.
    ///
    /// Example: `heph tool build-fmt //pkg/...`
    #[command(name = "build-fmt")]
    BuildFmt(build_fmt::Args),
    /// Run the BUILD-file language server over stdio (used by editors).
    #[command(name = "build-lsp", hide = true)]
    BuildLsp(build_lsp::Args),
    /// Download + verify every configured plugin is loadable
    ///
    /// Resolves each `plugins:` entry: built-ins are instantiated and each
    /// `path:`/`url:` plugin's cdylib is fetched (to `~/.heph/plugins`) and
    /// load-checked over the stable ABI. Fails if any plugin can't be loaded.
    ///
    /// Example: `heph tool resolve-plugins --force`
    #[command(name = "resolve-plugins")]
    ResolvePlugins(resolve_plugins::Args),
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
            ToolCommands::Clean(args) => clean::execute(args, sink, global),
            ToolCommands::GenGitignore(args) => gen_gitignore::execute(args, sink, global),
            ToolCommands::Scratch(args) => args.execute(sink),
            ToolCommands::Cache(args) => args.execute(sink, global),
            ToolCommands::Coreutils(args) => args.execute(),
            ToolCommands::Completions(args) => completions::execute(args),
            ToolCommands::BuildFmt(args) => build_fmt::execute(args, sink, global),
            ToolCommands::BuildLsp(args) => build_lsp::execute(args, sink, global),
            ToolCommands::ResolvePlugins(args) => resolve_plugins::execute(args, sink, global),
        }
    }
}
