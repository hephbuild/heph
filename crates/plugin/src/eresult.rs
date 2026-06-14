//! The result of executing a target: the artifacts it produced. This is the
//! data the engine hands back and that `ProviderExecutor` returns; the cache
//! read/compute machinery that builds it stays in the engine.

use hcore::hartifactcontent::Content;
use std::sync::Arc;

#[derive(Clone)]
pub struct ArtifactMeta {
    pub hashout: String,
}

#[derive(Clone, Default)]
pub struct EResult {
    pub artifacts: Vec<Arc<dyn Content>>,
    /// Auxiliary artifacts a dependent target materializes into its sandbox
    /// without surfacing in SRC/list env routing. Sourced from the producing
    /// target's `support_files` declaration; never filtered by output group.
    pub support_artifacts: Vec<Arc<dyn Content>>,
    pub artifacts_meta: Vec<ArtifactMeta>,
}
