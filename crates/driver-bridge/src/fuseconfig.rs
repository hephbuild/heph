//! The resolved FUSE-overlay decision the managed-driver bridge acts on. The
//! type and the YAML parsing (`fuse: { enabled: true | false | auto }`) both
//! live in the foundational `config` crate (`hconfig`); re-exported here so the
//! bridge keeps importing `fuseconfig::FuseMode`.

pub use hconfig::FuseMode;
