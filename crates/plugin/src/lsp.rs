//! The slice of engine behaviour a plugin's language server needs (e.g. the
//! buildfile plugin's BUILD-file LSP), abstracted behind a trait so the server
//! can live in its plugin's crate without depending on the concrete `Engine`.
//! The engine implements this; the server takes `Arc<dyn LspEngine>`.

use crate::config::Options;
use crate::driver::{DriverSchema, DriverSchemas};
use crate::function::FunctionRegistry;
use crate::provider::StateSchema;
use std::path::Path;
use std::sync::Arc;

pub trait LspEngine: Send + Sync {
    /// Workspace root, for resolving a provider's source-file paths (e.g.
    /// BUILD files).
    fn root(&self) -> &Path;
    /// The engine's sealed plugin-function registry (for completion/hover on
    /// `heph.<plugin>.<fn>`).
    fn function_registry(&self) -> Arc<FunctionRegistry>;
    /// Declared schema for a driver by registry name (for target-kind hovers).
    fn driver_schema(&self, name: &str) -> Option<DriverSchema>;
    /// All registered driver names, sorted (for target-kind completion).
    fn driver_names(&self) -> Vec<String>;
    /// Every registered driver's schema, by name (for
    /// `heph.types.<driver>.Kwargs`). Blocks like [`Self::driver_names`]:
    /// the LSP reads it off its loop, with the rest of the registry snapshot.
    ///
    /// The default derives it from the two methods above; the engine returns
    /// the set it sealed for its plugins, so the editor's `Kwargs` are
    /// evaluation's.
    fn driver_schemas(&self) -> DriverSchemas {
        self.driver_names()
            .into_iter()
            .filter_map(|name| {
                let schema = self.driver_schema(&name)?;
                Some((name, schema))
            })
            .collect()
    }
    /// The registered providers that declare a state schema, sorted (for
    /// `provider_state(provider = "…")` completion).
    fn provider_names(&self) -> Vec<String>;
    /// A provider's state schema by registry name (for state-field completion).
    fn provider_state_schema(&self, name: &str) -> Option<StateSchema>;
    /// The raw `options:` map a provider was configured with (for the LSP to
    /// read e.g. the buildfile provider's filename patterns). Empty if absent.
    fn provider_options(&self, name: &str) -> Options;
}
