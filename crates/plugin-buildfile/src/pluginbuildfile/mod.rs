mod lsp;
mod provider;
mod run_file;
#[cfg(test)]
mod scope_tests;
#[cfg(test)]
mod starlark_patch;

/// The transport-free LSP loop, for the editor-shaped harness in `tests/lsp.rs`.
#[cfg(feature = "test-support")]
pub use lsp::{
    RunOptions as LspRunOptions,
    list_with_fresh_buildfile_provider as lsp_list_with_fresh_buildfile_provider, run as lsp_run,
    run_with as lsp_run_with,
};
pub use lsp::{explain as lsp_explain, serve_stdio};
pub use provider::{
    DEFAULT_INDENT, FormatSettings, Provider, build_file_indent_from_options,
    build_file_patterns_from_options, build_files_in_dir, default_build_file_patterns,
};
