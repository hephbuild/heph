//! BUILD-file language server.
//!
//! `heph tool build-lsp` runs this. heph owns the loop ([`server`]): it answers
//! `initialize`, keeps the open documents, and serves each request in-process.
//! The standard Starlark surface (diagnostics, symbol completion/hover,
//! goto-def, load resolution) comes from the vendored `starlark_lsp` analysis
//! modules through [`HephLspContext`]; heph specifics (provenance, driver
//! config schemas, plugin functions) are layered on by [`proxy`]. See the
//! submodule docs for detail.

mod context;
pub(crate) mod diagnostics;
mod index;
mod position;
mod proxy;
mod server;

use hplugin::lsp::LspEngine;
use lsp_server::Connection;
use std::sync::Arc;

/// Serve the BUILD-file LSP over stdio until the client disconnects.
///
/// Must be called with a tokio runtime context entered: the LSP context
/// captures `Handle::current()` at construction (its listing provider's
/// memoizer spawns on it). `build_lsp.rs` enters the command runtime before
/// calling in.
pub fn serve_stdio(engine: Arc<dyn LspEngine>) -> anyhow::Result<()> {
    // `_io_threads` is intentionally never joined — see the `process::exit` note
    // below. Kept bound (not dropped) so the stdout writer thread stays alive to
    // flush responses until the process exits.
    let (stdio, _io_threads) = Connection::stdio();

    // Returns once the client sends `exit` (or stdin closes).
    run(stdio, engine)?;

    // Per the LSP spec, `exit` means terminate the process immediately. We must
    // NOT fall through to `io_threads.join()`: its stdin-reader thread blocks
    // until the client closes stdin, which editors (e.g. VS Code) only do
    // *after* the server has exited — so joining here would deadlock, the
    // client would SIGKILL us, and "restart server" would fail. The shutdown
    // response was already written and flushed (the client sequences `shutdown`
    // before `exit`), so there is nothing left to drain.
    std::process::exit(0);
}

/// Serve the BUILD-file LSP on `connection` until the client sends `exit` or
/// disconnects, then return.
///
/// The transport-free half of [`serve_stdio`]: the editor-shaped harness in
/// `tests/lsp.rs` drives it over an in-memory connection. Same runtime
/// contract as [`serve_stdio`].
pub fn run(connection: Connection, engine: Arc<dyn LspEngine>) -> anyhow::Result<()> {
    server::run(connection, engine)
}
