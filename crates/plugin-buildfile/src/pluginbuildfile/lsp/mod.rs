//! BUILD-file language server.
//!
//! `heph tool build-lsp` runs this. It reuses the official `starlark_lsp` server
//! for the standard surface (diagnostics, symbol completion/hover, goto-def, load
//! resolution) via [`HephLspContext`], and layers heph specifics on top through a
//! small [`proxy`] that enriches hover and completion responses (provenance,
//! driver config schemas). See the submodule docs for detail.

mod context;
pub(crate) mod diagnostics;
mod index;
mod proxy;

use context::HephLspContext;
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
    let ctx = HephLspContext::new(engine);
    let shared = Arc::clone(&ctx.shared);

    // `proxy_end` is the client-facing side of the in-memory pair; `server_end`
    // is what the starlark_lsp server reads/writes.
    let (proxy_end, server_end) = Connection::memory();

    std::thread::scope(|scope| {
        let server = scope.spawn(move || {
            // The stock server owns the initialize handshake and main loop on its
            // connection; the proxy just pipes bytes through.
            starlark_lsp::server::server_with_connection(server_end, ctx)
        });

        // Returns once the client sends `exit` (or the connection closes) and
        // the inner server finishes.
        proxy::run(&connection, &proxy_end, shared);
        drop(proxy_end);

        match server.join() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(anyhow::anyhow!("BUILD-file language server: {e}")),
            Err(_) => Err(anyhow::anyhow!("BUILD-file language server panicked")),
        }
    })
}
