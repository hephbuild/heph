//! The slice of engine behaviour the BUILD-file LSP needs, abstracted behind a
//! trait so the LSP can live in the buildfile plugin crate without depending on
//! the concrete `Engine`. The engine implements this; the LSP takes
//! `Arc<dyn LspEngine>`.

use crate::config::Options;
use crate::driver::DriverSchema;
use crate::provider::{ProviderFunctionRegistry, StateSchema};
use std::path::Path;
use std::sync::Arc;

pub trait LspEngine: Send + Sync {
    /// Workspace root, for resolving BUILD-file paths.
    fn root(&self) -> &Path;
    /// The merged provider-function registry (for completion/hover on
    /// provider-defined functions).
    fn provider_function_registry(&self) -> Arc<ProviderFunctionRegistry>;
    /// Declared schema for a driver by registry name (for target-kind hovers).
    fn driver_schema(&self, name: &str) -> Option<DriverSchema>;
    /// All registered driver names, sorted (for target-kind completion).
    fn driver_names(&self) -> Vec<String>;
    /// A provider's state schema by registry name (for state-field completion).
    fn provider_state_schema(&self, name: &str) -> Option<StateSchema>;
    /// The raw `options:` map a provider was configured with (for the LSP to
    /// read e.g. the buildfile provider's filename patterns). Empty if absent.
    fn provider_options(&self, name: &str) -> Options;
}
