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

impl Listing {
    /// Whether the path names a place inside the workspace ([`check_path`],
    /// every segment whole). The path reaches every provider, some of which
    /// join it to the workspace root and walk the result, so the LSP checks
    /// what it types and the engine checks again what it is handed.
    pub fn check(&self) -> Result<(), BadPath> {
        match self {
            Self::Packages { under: path } | Self::Targets { package: path } => {
                check_path(path, path)
            }
        }
    }
}

/// Why a package path is not listed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadPath {
    /// It starts with `/` (`///etc` types `/etc`).
    Absolute,
    /// A whole segment is `.` or `..`.
    DotSegment,
    /// Two slashes in a row (`a//b`), or one at the end of a package.
    EmptySegment,
    /// A NUL or other control character.
    ControlByte,
}

impl BadPath {
    /// The stable name `--explain --json` reports.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Absolute => "absolute",
            Self::DotSegment => "dot_segment",
            Self::EmptySegment => "empty_segment",
            Self::ControlByte => "control_byte",
        }
    }

    /// Why, in a sentence.
    pub const fn message(self) -> &'static str {
        match self {
            Self::Absolute => "an absolute path is outside the workspace",
            Self::DotSegment => "a `.` or `..` segment does not name a package",
            Self::EmptySegment => "an empty segment (`//` inside the path) does not name a package",
            Self::ControlByte => "a control character does not belong in a package path",
        }
    }
}

impl std::fmt::Display for BadPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message(), self.as_str())
    }
}

impl std::error::Error for BadPath {}

/// Checks the package path `text` whose whole segments are `whole` (a
/// prefix of it; the rest is a partial segment that only filters): no
/// control character anywhere, no leading `/`, and each whole segment
/// neither empty nor `.` nor `..`. Anything else stays inside the workspace
/// when a provider joins it to the root.
pub fn check_path(text: &str, whole: &str) -> Result<(), BadPath> {
    if text.chars().any(char::is_control) {
        return Err(BadPath::ControlByte);
    }
    if text.starts_with('/') {
        return Err(BadPath::Absolute);
    }
    if whole.is_empty() {
        return Ok(());
    }
    for seg in whole.split('/') {
        match seg {
            "" => return Err(BadPath::EmptySegment),
            "." | ".." => return Err(BadPath::DotSegment),
            _ => {}
        }
    }
    Ok(())
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
    /// The provider that listed it first, in registration order. Shared, not
    /// owned: every entry from one provider points at one name, so a listing
    /// costs no allocation per entry for a field completion never reads.
    pub provider: Arc<str>,
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
