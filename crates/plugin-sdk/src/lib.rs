//! Rust SDK for writing heph plugins.
//!
//! This crate is the single third-party integration point for plugin authors:
//! an author implements the **same** [`hplugin::provider::Provider`] /
//! [`hplugin::driver::Driver`] traits as an in-process plugin, and the SDK
//! provides the utilities to expose them to a host. It is **transport-agnostic**
//! — the author surface (this module's re-exports) carries no transport deps.
//!
//! A **transport** is opt-in, selected by the consumer via a cargo feature, so a
//! plugin author chooses how their plugin is carried:
//! - `stabby` — in-process stable-ABI cdylib (native speed). See [`stabby`].
//! - (future) proto/shm, wasm — sibling features, same author surface.

/// Re-export of the author-facing contract so a plugin depends only on the SDK.
pub use hplugin::{driver, eresult, provider};

#[cfg(feature = "stabby")]
mod guest;
#[cfg(feature = "stabby")]
mod serve;

/// In-process stable-ABI cdylib transport (opt-in via the `stabby` feature).
///
/// A plugin crate builds as a `cdylib`, constructs its provider + drivers with
/// [`stabby::make_dyn_provider`] / [`stabby::make_dyn_managed_driver`], and
/// returns them from a `#[stabby::export]` entry as [`stabby::abi::PluginComponents`].
/// The host loads it via `hplugin_stabby::load_stable`.
#[cfg(feature = "stabby")]
pub mod stabby {
    /// The shared stable-ABI contract (types crossing the cdylib boundary).
    pub use hplugin_stabby::abi;

    pub use crate::guest::GuestExecutor;
    pub use crate::serve::{make_dyn_managed_driver, make_dyn_provider};

    /// The ABI version this transport builds against.
    pub use plugin_abi::ABI_SEMVER;
}
