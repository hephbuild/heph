//! The host callback surface, from the guest plugin's perspective.

use anyhow::Result;
use async_trait::async_trait;
use hmodel::htaddr::Addr;
use hmodel::htmatcher::Matcher;
use hplugin::eresult::EResult;
use std::sync::Arc;

/// The callbacks a plugin makes back into the host while serving `get`/`parse`.
///
/// Mirrors [`hplugin::provider::ProviderExecutor`] plus the [`HostClient::note_dep`]
/// cache-hit fast path. The SDK implements this over the negotiated transport
/// (M1, in `plugin-remote`); plugin code uses it exactly like the in-process
/// `executor`, so the dep-resolution call sites don't change when a plugin goes
/// out-of-process.
///
/// Like `ProviderExecutor`, **do not memoize these calls on the addr inside a
/// provider** — the host registers the `parent -> addr` dep edge on each call
/// (the synchronous cycle check). Cache the derived output, never the call.
#[async_trait]
pub trait HostClient: Send + Sync {
    /// Resolve a target's result (artifacts + lease). Registers the dep edge
    /// host-side; bytes are pulled lazily through the returned `EResult`'s
    /// SDK-backed `Content` handles.
    async fn result(&self, addr: &Addr) -> Result<Arc<EResult>>;

    /// Resolve all targets matching `m` (`extra_skip` unioned with the request's
    /// skip set for this call only).
    async fn query(&self, m: &Matcher, extra_skip: &[String]) -> Result<Vec<Addr>>;

    /// Register a `parent -> addr` dependency edge only, without fetching the
    /// result — the cache-hit fast path. Returns an error whose context marks a
    /// cycle when the edge closes one. Batched by the SDK on the shm transport.
    async fn note_dep(&self, parent: &Addr, addr: &Addr) -> Result<()>;
}
