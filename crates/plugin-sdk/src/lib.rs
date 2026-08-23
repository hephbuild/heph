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
pub use hplugin::{driver, eresult, hook, provider};

/// The exec-runner author surface: implement [`runner::ExecRunner`] to serve the
/// environment a runner target describes, returning an [`runner::ExecSession`].
/// A runner is its own component kind — it needs no driver, no schema and no
/// config.
///
/// This is the *same* trait a runner compiled into the host implements. There is
/// no plugin-specific variant, because the session crosses the seam as a live
/// object: what it holds open — a shell, a socket, a pid, a mux over them — is
/// the runner's business and the host never names it.
#[cfg(feature = "stabby")]
pub mod runner {
    pub use hexec_runner::{
        ExecRunner, ExecSession, Identity, OpenRequest, RunnerArtifact, SessionCaps,
        SessionDescription, SpawnError, TeardownJob,
    };
}

#[cfg(feature = "stabby")]
mod guest;
#[cfg(feature = "stabby")]
mod logsink;
#[cfg(feature = "stabby")]
mod serve;
#[cfg(feature = "stabby")]
mod supervisor;

/// In-process stable-ABI cdylib transport (opt-in via the `stabby` feature).
///
/// A plugin crate builds as a `cdylib`, constructs its components with
/// [`stabby::make_dyn_provider`] / [`stabby::make_dyn_managed_driver`] /
/// [`stabby::make_dyn_exec_runner`], and returns them from a
/// `#[stabby::export]` entry as [`stabby::abi::PluginComponents`].
/// The host loads it via `hplugin_stabby::load_stable`.
#[cfg(feature = "stabby")]
pub mod stabby {
    /// The shared stable-ABI contract (types crossing the cdylib boundary).
    pub use hplugin_stabby::abi;

    pub use crate::guest::GuestExecutor;
    pub use crate::logsink::install_log_sink;
    pub use crate::serve::{
        cdylib_runtime_handle, make_dyn_exec_runner, make_dyn_hook, make_dyn_managed_driver,
        make_dyn_provider,
    };
    pub use crate::supervisor::{install_supervisor, supervisor_sink};

    /// Decode the cdylib create-entry config (`pb::CreateConfig`) from its prost
    /// bytes, and convert its structured `options` map into a plugin `options:`
    /// map. Authors then read typed values via [`hplugin::config::decode_opt`],
    /// exactly as in-process plugins do.
    pub use plugin_abi::convert::{create_config_from_bytes, options_from_pb_map};

    /// The ABI version this transport builds against.
    pub use plugin_abi::ABI_SEMVER;
}
