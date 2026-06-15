//! Transport-agnostic wire ABI for heph external plugins.
//!
//! This crate is the raw wire layer shared by all three transports (proto/UDS,
//! shm/iceoryx2, wasm). Plugin authors do NOT use it directly — they use the
//! per-language SDK (`plugin-sdk` for Rust), which sits on top.
//!
//! - [`pb`] re-exports the prost-generated message types (source of truth,
//!   generated from `proto/plugin/v1/*.proto` via `buf`).
//! - [`shm_types`] holds rkyv zero-copy mirrors of the ≤5 hot-path messages,
//!   used by the shm fast path, with `From`/`Into` conversions to [`pb`].
//!
//! The handshake negotiates the payload encoding per transport: `Rkyv` (native
//! zero-copy) for Rust plugins on shm, `Capnp` for polyglot shm, `Prost`
//! everywhere else.

/// The prost-generated wire message types (`heph.plugin.v1`).
pub use hproto_gen::heph::plugin::v1 as pb;

/// ABI semantic version. Major must match exactly between host and plugin;
/// minor is negotiated to `min(host, plugin)` at handshake.
pub const ABI_SEMVER: &str = "0.1.0";

pub mod shm_types;

#[cfg(feature = "convert")]
pub mod convert;

#[cfg(feature = "transport")]
pub mod frame;
#[cfg(feature = "transport")]
pub mod mux;

#[cfg(feature = "shm")]
pub mod shm;
