//! BUILD-file language server.
//!
//! `heph tool build-lsp` runs this. It reuses the official `starlark_lsp` server
//! for the standard surface (diagnostics, symbol completion/hover, goto-def, load
//! resolution) via [`HephLspContext`], and layers heph specifics on top through a
//! small [`proxy`] that enriches hover and completion responses (provenance,
//! driver config schemas). See the submodule docs for detail.

mod context;
mod index;
mod proxy;

use crate::engine::engine::Engine;
use context::HephLspContext;
use lsp_server::Connection;
use std::sync::Arc;

/// Serve the BUILD-file LSP over stdio until the client disconnects. The
/// `starlark_lsp` server runs on an in-memory connection; a proxy bridges it to
/// the real stdio and enriches hover/completion.
pub fn serve_stdio(engine: Arc<Engine>) -> anyhow::Result<()> {
    let ctx = HephLspContext::new(engine);
    let shared = Arc::clone(&ctx.shared);

    let (stdio, io_threads) = Connection::stdio();
    // `proxy_end` is the client-facing side of the in-memory pair; `server_end`
    // is what the starlark_lsp server reads/writes.
    let (proxy_end, server_end) = Connection::memory();

    std::thread::scope(|scope| -> anyhow::Result<()> {
        let server = scope.spawn(move || {
            // The stock server owns the initialize handshake and main loop on its
            // connection; the proxy just pipes bytes through.
            starlark_lsp::server::server_with_connection(server_end, ctx)
        });

        proxy::run(&stdio, &proxy_end, shared);

        server
            .join()
            .map_err(|_panic| anyhow::anyhow!("lsp server thread panicked"))??;
        Ok(())
    })?;

    io_threads.join()?;
    Ok(())
}
