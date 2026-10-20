//! BUILD-file language server.
//!
//! `heph tool build-lsp` runs this. heph owns the loop ([`server`]): it answers
//! `initialize`, keeps the open documents, and serves each request in-process.
//!
//! Every heph-specific answer derives from one classification of the cursor's
//! position, the [`site`] resolver's, computed from the document's tokens
//! ([`lex`]) and never from whether the file evaluated. The [`handlers`] table
//! maps each site kind to its completion items, hover and definition under one
//! merge policy. The standard Starlark surface (locals, symbol hover, goto-def
//! for local and loaded names, parse diagnostics) comes from the vendored
//! `starlark_lsp` analysis modules through [`context::HephLspContext`], which
//! passes through wherever heph cannot know the answer. `docs/LSP.md` describes
//! the site kinds, the merge policy and how to add one.

mod context;
pub(crate) mod diagnostics;
mod handlers;
mod index;
mod lex;
mod position;
mod server;
mod site;

#[cfg(feature = "test-support")]
pub use server::{RunOptions, run_with};

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

/// What the resolver makes of `<file>:<line>:<col>` (1-based line and column,
/// the column counted in characters): the site, why a heph answer would be
/// empty, and what a present one could not do in full. The same
/// classification every editor request goes through, for "why did it do
/// that?". `json` renders it as one JSON object.
pub fn explain(target: &str, json: bool) -> anyhow::Result<String> {
    use anyhow::Context as _;
    let mut parts = target.rsplitn(3, ':');
    let (col, line, file) = match (parts.next(), parts.next(), parts.next()) {
        (Some(c), Some(l), Some(f)) => (c, l, f),
        _ => anyhow::bail!("expected <file>:<line>:<col>, got {target:?}"),
    };
    let line: u32 = line
        .parse()
        .with_context(|| format!("line {line:?} in {target:?}"))?;
    let col: u32 = col
        .parse()
        .with_context(|| format!("column {col:?} in {target:?}"))?;
    let text = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    let tokens = lex::lex(&text);
    let parsed = starlark::syntax::AstModule::parse(
        file,
        text.clone(),
        &starlark::syntax::Dialect::Extended,
    )
    .ok();
    let overlay = parsed.as_ref().map(site::overlay_of);
    let pos = lsp_types::Position::new(line.saturating_sub(1), col.saturating_sub(1));
    let resolution = match position::to_offset(&text, pos, position::Encoding::Utf32) {
        Some(offset) => site::resolve(
            &site::Doc {
                text: &text,
                tokens: &tokens,
                overlay: overlay.as_ref(),
                ast: if overlay.is_some() {
                    site::AstState::Current
                } else {
                    site::AstState::Absent
                },
            },
            offset,
        ),
        None => site::Resolution::none(site::Reason::NotASite),
    };
    let value = resolution.to_json();
    if json {
        let mut out = serde_json::json!({ "file": file, "line": line, "col": col });
        if let (Some(map), serde_json::Value::Object(r)) = (out.as_object_mut(), value) {
            map.extend(r);
        }
        return serde_json::to_string_pretty(&out).context("rendering the resolution");
    }
    let none = serde_json::Value::Null;
    let site = value.get("site").unwrap_or(&none);
    let kind = site.get("kind").and_then(|k| k.as_str()).unwrap_or("none");
    let mut out = format!("{file}:{line}:{col}\n  site: {kind}");
    for key in ["call", "keyword", "prefix", "quote", "plugin", "selected"] {
        if let Some(v) = site.get(key).filter(|v| !v.is_null()) {
            out.push_str(&format!("\n  {key}: {v}"));
        }
    }
    if let Some(keys) = site
        .get("present_keys")
        .and_then(|k| k.as_array())
        .filter(|k| !k.is_empty())
    {
        let keys: Vec<&str> = keys.iter().filter_map(|k| k.as_str()).collect();
        out.push_str(&format!("\n  present keys: {}", keys.join(", ")));
    }
    match value.get("reason").filter(|r| !r.is_null()) {
        Some(r) => out.push_str(&format!("\n  reason: {r}")),
        None => out.push_str("\n  reason: none (heph answers here)"),
    }
    for d in value
        .get("degraded")
        .and_then(|d| d.as_array())
        .into_iter()
        .flatten()
    {
        out.push_str(&format!("\n  degraded: {d}"));
    }
    Ok(out)
}
