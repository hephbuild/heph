//! Wire ABI types for heph external plugins.
//!
//! This crate is the raw wire layer the plugin transport shares. Plugin authors
//! do NOT use it directly — they use the SDK (`plugin-sdk`), which sits on top.
//!
//! - [`pb`] re-exports the prost-generated message types (source of truth,
//!   generated from `proto/plugin/v1/*.proto` via `buf`).
//! - [`convert`] holds the conversions between the [`pb`] wire types and the
//!   in-process `hplugin`/`hmodel`/`hcore` types.
//!
//! The cold, low-volume Provider/Driver methods cross the cdylib boundary as
//! prost-encoded [`pb`] bytes; the hot `ProviderExecutor` callbacks cross as
//! native stabby vtable calls (see `plugin-stabby`).

/// The prost-generated wire message types (`heph.plugin.v1`).
pub use hproto_gen::heph::plugin::v1 as pb;

/// ABI semantic version. Major must match exactly between host and plugin;
/// minor is negotiated to `min(host, plugin)` at handshake.
pub const ABI_SEMVER: &str = "0.1.0";

#[cfg(feature = "convert")]
pub mod convert;
