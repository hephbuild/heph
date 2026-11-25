mod ast;
mod checks;
mod lsp;
mod provider;
mod run_file;
#[cfg(test)]
mod starlark_patch;
mod types;

pub use checks::format::check_verdict as format_check_verdict;
pub use checks::targets::{ChecksOptions, ShadowedCheck, ShadowedChecks};
pub use lsp::{Explanation as LspExplanation, explain as lsp_explain, serve_stdio};
/// The transport-free LSP loop, for the editor-shaped harness in `tests/lsp.rs`.
#[cfg(feature = "test-support")]
pub use lsp::{RunOptions as LspRunOptions, run as lsp_run, run_with as lsp_run_with};
pub use provider::{
    DEFAULT_INDENT, FormatSettings, Provider, build_file_indent_from_options,
    build_file_patterns_from_options, build_files_in_dir, default_build_file_patterns,
};

use hdriver_support::driver_managed::ManagedDriver;
use std::path::PathBuf;
use std::sync::Arc;

/// What [`plugin_parts`] needs from the engine registering the plugin: the
/// fields of its `PluginInit` the provider reads. (`PluginInit` and
/// `PluginParts` are the engine's, which this crate does not depend on.)
#[expect(
    missing_debug_implementations,
    reason = "CachedWalker and FunctionSlot have no Debug"
)]
pub struct PartsInit<'a> {
    pub root: PathBuf,
    pub skip_dirs: &'a [PathBuf],
    pub skip_globs: &'a [String],
    pub runtime: tokio::runtime::Handle,
    pub walker: Arc<hwalk::CachedWalker>,
    /// The engine's function slot, read on the first evaluation.
    pub functions: Arc<hplugin::function::FunctionSlot>,
    /// The engine's driver schema slot, read on the first evaluation:
    /// `heph.types.<driver>.Kwargs` is built from it.
    pub drivers: Arc<hplugin::driver::DriverSchemaSlot>,
    /// Where the provider records every check target a package's own target
    /// shadows (I2); the caller keeps a clone to read (`heph validate`).
    pub shadowed: ShadowedChecks,
}

/// The buildfile plugin: its provider, which lists the check targets, and
/// the three drivers that run them. Register all of it: a provider without
/// the drivers would list targets nothing can run.
pub struct Parts {
    pub provider: Provider,
    pub drivers: Vec<Box<dyn ManagedDriver>>,
}

impl std::fmt::Debug for Parts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parts")
            .field("drivers", &self.drivers.len())
            .finish_non_exhaustive()
    }
}

/// Build the buildfile plugin from its `options:`, decoded once: the
/// provider [`with_checks`](Provider::with_checks), plus the drivers
/// `buildfile.lint_check`, `buildfile.format_check` and `buildfile.format`.
/// [`Provider::new`] and [`Provider::from_options`] alone synthesize no check
/// target.
pub fn plugin_parts(init: PartsInit<'_>, opts: &hplugin::config::Options) -> anyhow::Result<Parts> {
    let checks = ChecksOptions::from_options(opts)?;
    // The lint driver reads the function names from the same slot the
    // provider digests them from.
    let functions = provider::FunctionSource::Engine(Arc::clone(&init.functions));
    // And the driver kwargs from the same source as the provider, built once
    // for both.
    let drivers = provider::DriverSource::engine(init.drivers);
    let provider = Provider::from_options(
        init.root,
        init.skip_dirs,
        init.skip_globs,
        opts,
        init.runtime,
    )?
    .with_walker(init.walker)
    .with_functions(init.functions)
    .with_drivers(drivers.clone())
    .with_checks(checks)
    .with_shadowed_checks(init.shadowed);
    let check_drivers = checks::targets::CheckKind::ALL
        .into_iter()
        .map(|k| {
            Box::new(checks::drivers::CheckDriver::new(
                k,
                functions.clone(),
                drivers.clone(),
            )) as Box<dyn ManagedDriver>
        })
        .collect();
    Ok(Parts {
        provider,
        drivers: check_drivers,
    })
}
