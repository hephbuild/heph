use crate::engine::provider::TargetSpec;
use serde::{Serialize, Serializer};
use std::ops::Deref;
use std::sync::Arc;

/// Engine-owned wrapper around `provider::TargetSpec`. The provider trait yields
/// a `TargetSpec` it built itself; the engine stamps additional bookkeeping it
/// alone controls (currently the producing provider's name, used to skip that
/// provider when resolving the spec's query inputs).
///
/// `Deref` to `TargetSpec` so existing callsites that read `spec.driver`,
/// `spec.config`, `spec.addr`, etc. keep working unchanged.
pub struct EngineTargetSpec {
    pub spec: Arc<TargetSpec>,
    /// Name of the provider that returned `spec` from `Provider::get`. Empty if
    /// the spec was constructed without going through a registered provider
    /// (e.g. some test scaffolding).
    pub provider: String,
}

impl Deref for EngineTargetSpec {
    type Target = TargetSpec;

    fn deref(&self) -> &Self::Target {
        &self.spec
    }
}

// Hand-roll Serialize: `Arc<TargetSpec>` doesn't serialize without serde's `rc`
// feature, so flatten the inner spec's fields and append `provider`.
impl Serialize for EngineTargetSpec {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("EngineTargetSpec", 2)?;
        st.serialize_field("spec", &*self.spec)?;
        st.serialize_field("provider", &self.provider)?;
        st.end()
    }
}
