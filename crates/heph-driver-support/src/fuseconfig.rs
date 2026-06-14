//! The resolved FUSE-overlay decision the managed-driver bridge acts on. The
//! YAML parsing (`fuse: { enabled: true | false | auto }`) lives in the engine's
//! central config module (`engine::config_yaml`), which resolves it to one of
//! these.

/// Resolved decision used by the engine + bridge. Independent of YAML shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuseMode {
    /// Forced on. Probe must succeed; mount errors propagate.
    On,
    /// Forced off.
    Off,
    /// Engine decides per-target by walking inputs.
    Auto,
}
