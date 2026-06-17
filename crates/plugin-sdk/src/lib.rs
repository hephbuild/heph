//! Rust SDK for writing heph plugins.
//!
//! This is the ergonomic, transport-abstracting surface plugin authors use. An
//! author implements the **same** [`hplugin::provider::Provider`] /
//! [`hplugin::driver::Driver`] traits as an in-process plugin; the SDK wraps
//! them as ABI-stable handles a host can load. Today the only transport is the
//! in-process stable-ABI cdylib ([`plugin_stabby`]); the seam is structured so
//! further transports slot in behind this same crate.
//!
//! A plugin crate builds as a `cdylib`, constructs its provider + drivers with
//! [`serve::make_dyn_provider`] / [`serve::make_dyn_managed_driver`], and returns
//! them from a `#[stabby::export]` entry as [`abi::PluginComponents`]. The host
//! loads it via `hplugin_stabby::load_stable`.

pub mod guest;
pub mod serve;

/// The shared stable-ABI contract (types crossing the cdylib boundary). Re-exported
/// from `plugin-stabby` so a plugin depends only on the SDK.
pub use hplugin_stabby::abi;

pub use guest::GuestExecutor;
pub use serve::{make_dyn_managed_driver, make_dyn_provider};

/// Re-export of the author-facing contract so a plugin depends only on the SDK.
pub use hplugin::{driver, eresult, provider};

/// The ABI version this SDK builds against.
pub use plugin_abi::ABI_SEMVER;
