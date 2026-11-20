//! The slice of engine behaviour the BUILD-file LSP needs, abstracted behind a
//! trait so the LSP can live in the buildfile plugin crate without depending on
//! the concrete `Engine`. The engine implements this; the LSP takes
//! `Arc<dyn LspEngine>`.

use crate::config::Options;
use crate::driver::DriverSchema;
use crate::function::FunctionRegistry;
use crate::provider::StateSchema;
use crate::scope::ScopeId;
use futures::future::BoxFuture;
use hcore::events::SelectionGap;
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
    pub scope: ScopeId,
    /// Package paths, or target names, sorted, each once.
    pub entries: Vec<ListedEntry>,
    /// What the listing had to leave out, one group per stage and provider,
    /// each with its exact count and up to five examples (the shape `heph
    /// query` reports under `--keep-going`); empty when nothing was.
    pub skipped: Vec<SelectionGap>,
    /// Resolves once the scope ended and every provider answered
    /// `scope_ended`, to what each one answered, in registration order; empty
    /// if no provider was told (no provider request was built in the scope,
    /// or the engine went first). Dropping it costs nothing.
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
    /// The provider that listed it first, in registration order.
    pub provider: String,
}

/// How one provider answered the end of a cache scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeEndAck {
    pub provider: String,
    pub outcome: ScopeEndOutcome,
}

/// What one provider's `scope_ended` came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ScopeEndOutcome {
    /// It dropped what it cached for the scope (possibly nothing).
    Released,
    /// It keeps no scoped caches, or predates scopes.
    Unsupported,
    /// It returned an error; it may keep that scope's caches.
    Failed,
    /// It panicked; it may keep that scope's caches.
    Panicked,
    /// It did not answer in time; it may keep that scope's caches.
    TimedOut,
}

impl ScopeEndOutcome {
    /// The outcome's stable name: `released`, `unsupported`, `failed`,
    /// `panicked` or `timed_out`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Released => "released",
            Self::Unsupported => "unsupported",
            Self::Failed => "failed",
            Self::Panicked => "panicked",
            Self::TimedOut => "timed_out",
        }
    }
}

impl std::fmt::Display for ScopeEndOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
