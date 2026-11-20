//! The slice of engine behaviour the BUILD-file LSP needs, abstracted behind a
//! trait so the LSP can live in the buildfile plugin crate without depending on
//! the concrete `Engine`. The engine implements this; the LSP takes
//! `Arc<dyn LspEngine>`.

use crate::config::Options;
use crate::driver::DriverSchema;
use crate::function::FunctionRegistry;
use crate::provider::StateSchema;
use futures::future::BoxFuture;
use std::path::Path;
use std::sync::Arc;

pub trait LspEngine: Send + Sync {
    /// Workspace root, for resolving BUILD-file paths.
    fn root(&self) -> &Path;
    /// The engine's sealed plugin-function registry (for completion/hover on
    /// `heph.<plugin>.<fn>`).
    fn function_registry(&self) -> Arc<FunctionRegistry>;
    /// Declared schema for a driver by registry name (for target-kind hovers).
    fn driver_schema(&self, name: &str) -> Option<DriverSchema>;
    /// All registered driver names, sorted (for target-kind completion).
    fn driver_names(&self) -> Vec<String>;
    /// The registered providers that declare a state schema, sorted (for
    /// `provider_state(provider = "…")` completion).
    fn provider_names(&self) -> Vec<String>;
    /// A provider's state schema by registry name (for state-field completion).
    fn provider_state_schema(&self, name: &str) -> Option<StateSchema>;
    /// The raw `options:` map a provider was configured with (for the LSP to
    /// read e.g. the buildfile provider's filename patterns). Empty if absent.
    fn provider_options(&self, name: &str) -> Options;
    /// What the engine lists, asked the way `heph query` asks it: every
    /// registered provider, with its states and probes, keeping going past a
    /// provider that fails. Each call is its own root request, so its own
    /// cache scope: it sees the workspace as it is now, never what an earlier
    /// call cached.
    fn list(self: Arc<Self>, what: Listing) -> BoxFuture<'static, anyhow::Result<Listed>>;
}

/// What [`LspEngine::list`] lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listing {
    /// The packages at or below `under` (`""` for all of them).
    Packages { under: String },
    /// The targets in `package`.
    Targets { package: String },
}

/// One [`LspEngine::list`] answer.
pub struct Listed {
    /// The cache scope the listing ran in.
    pub scope: u64,
    /// Package paths, or target names, sorted, each once.
    pub entries: Vec<ListedEntry>,
    /// What the listing had to leave out, one line each (`list buildfile //a:
    /// <cause>`); empty when nothing was.
    pub skipped: Vec<String>,
    /// Resolves once the scope ended and every provider answered
    /// `scope_ended`, to what each one answered; empty if no provider was
    /// told. Dropping it costs nothing.
    pub scope_ended: BoxFuture<'static, Vec<ScopeEndAck>>,
}

impl std::fmt::Debug for Listed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listed")
            .field("scope", &self.scope)
            .field("entries", &self.entries)
            .field("skipped", &self.skipped)
            .finish_non_exhaustive()
    }
}

/// A package or target name and the provider it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedEntry {
    pub name: String,
    /// The provider that listed the package first, in registration order, or
    /// the one that resolves the target.
    pub provider: String,
}

/// How one provider answered the end of a cache scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeEndAck {
    pub provider: String,
    /// `released` (it dropped the scope's caches), `unsupported` (it keeps no
    /// scoped caches, or predates scopes), `failed`, `panicked` or
    /// `timed_out`.
    pub outcome: &'static str,
}
