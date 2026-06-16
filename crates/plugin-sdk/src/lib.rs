//! Rust SDK for writing heph plugins that run out-of-process.
//!
//! This is the ergonomic surface plugin authors use; it sits on top of the raw
//! wire layer ([`plugin_abi`]) and hides framing, encoding, streaming, leases
//! and cancellation. A plugin author implements the **same** [`hplugin::provider::Provider`]
//! / [`hplugin::driver::Driver`] traits as an in-process plugin, so moving an
//! existing plugin out-of-process is near-zero-change.
//!
//! One SDK exists per guest language; this is the Rust one. The proto/WIT
//! contract is designed so a future Go/Python SDK is mechanical.
//!
//! Status: M0 skeleton — public surface (author traits, [`Ctx`], [`HostClient`])
//! is defined; transport-backed implementations and the `serve` entry point
//! land in M1 (`plugin-remote`).

pub mod ctx;
pub mod host;
pub mod serve;

pub use ctx::Ctx;
pub use host::HostClient;
#[cfg(all(unix, feature = "shm"))]
pub use serve::serve_components_shm;
pub use serve::{serve, serve_components, serve_driver, serve_managed_driver, serve_plugin};
#[cfg(unix)]
pub use serve::{serve_components_inherited, serve_inherited};

/// Re-export of the author-facing contract so a plugin depends only on the SDK.
pub use hplugin::{driver, eresult, provider};

/// The ABI version this SDK builds against (negotiated at handshake).
pub use plugin_abi::ABI_SEMVER;
