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
pub use hplugin::{driver, eresult, function, hook, provider};

/// Running a program in the caller's terminal, for a driver that honours
/// `RunStdio::Inherit` (an entrypoint's run). The same spawn exec uses:
/// inherited fds 0–2, the caller's dispositions, signal mask and open-file
/// limit restored (they travel on the run's `Foreground`: pass them on with
/// [`with_foreground`](foreground::ForegroundCommand::with_foreground), not
/// field by field), the pid tracked with the host's supervisor, and the run's
/// signals relayed by [`ForegroundChild::wait`](foreground::ForegroundChild::wait)
/// without ever signalling a reaped pid.
///
/// ```ignore
/// let RunStdio::Inherit(fg) = std::mem::take(&mut req.options.stdio) else { … };
/// let cmd = ForegroundCommand::new(program, args, env, cwd).with_foreground(&fg);
/// let (child, _) = spawn_foreground(RunnerRef::local(), cmd, &PathPolicy::default(), ctoken).await?;
/// let exit = child?.wait(fg.signals, ctoken).await?;
/// ```
///
/// A driver that declares [`Entrypoint::with_pty`](driver::targetdef::Entrypoint::with_pty)
/// also gets `RunStdio::Pty` on a default run: the same spawn with
/// [`with_slave`](foreground::ForegroundCommand::with_slave), and
/// [`wait_pty`](foreground::ForegroundChild::wait_pty) in place of `wait`. Without
/// that bit the run stays `Inherit`.
pub mod foreground {
    pub use hexecrunner::foreground::{
        ForegroundChild, ForegroundCommand, RelayContext, should_relay, spawn_foreground,
    };
    pub use hexecrunner::{PathPolicy, RunnerRef, SpawnedAs};
}

#[cfg(feature = "stabby")]
mod capabilities;
#[cfg(feature = "stabby")]
mod guest;
#[cfg(feature = "stabby")]
mod logsink;
#[cfg(feature = "stabby")]
mod runnerhost;
#[cfg(feature = "stabby")]
mod serve;
#[cfg(feature = "stabby")]
mod supervisor;

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
    pub use crate::logsink::{install_log_filter, install_log_sink};
    pub use crate::runnerhost::{install_runner_host, make_dyn_runner};
    pub use crate::serve::{
        cdylib_runtime_handle, guest_functions, make_dyn_hook, make_dyn_managed_driver,
        make_dyn_provider, make_plugin_functions, named_boxed_managed_driver, named_managed_driver,
    };
    pub use crate::supervisor::{install_supervisor, supervisor_sink};

    /// The cdylib create-entry config, and its decoder from the bytes the host
    /// hands `heph_plugin_create` — which also records the host's capabilities
    /// for this plugin ([`host_capabilities`]).
    pub use crate::capabilities::{
        create_config_from_bytes, host_capabilities, require_entrypoint_support,
    };
    pub use plugin_abi::convert::{CreateConfig, HostCapabilities};

    /// The ABI version this transport builds against.
    pub use plugin_abi::ABI_SEMVER;

    /// The body of a cdylib's required `heph_plugin_abi_semver` entry: a
    /// pointer to [`ABI_SEMVER`], NUL-terminated and `'static`. The host reads
    /// it before calling anything else and refuses a plugin from an ABI it does
    /// not load — or one that does not export the entry at all.
    /// [`export_host_entries!`](crate::export_host_entries) exports it.
    pub fn abi_semver() -> *const std::ffi::c_char {
        plugin_abi::ABI_SEMVER_C.as_ptr()
    }
}

/// Export every host entry a cdylib plugin needs besides its own
/// `heph_plugin_create`: the ABI version, the log sink and filter, the process
/// supervisor and the exec-runner host. Invoke it once at the root of the
/// cdylib crate, which must depend on `stabby` itself:
///
/// ```ignore
/// plugin_sdk::export_host_entries!();
/// ```
///
/// Each entry is optional to the host except the ABI version, so one a plugin
/// forgot to export would only degrade: its logs vanish, its children go
/// unsupervised, its targets cannot name a runner. Exporting them all from
/// here means a new entry reaches every plugin built against this SDK.
///
/// - `heph_plugin_abi_semver`: the [`ABI_SEMVER`](crate::stabby::ABI_SEMVER) the
///   plugin was built for. The host reads it before anything else and refuses
///   a plugin from an ABI it does not load. A plain C symbol, not a stabby
///   export, so it reads the same whatever stabby layout either side has.
/// - `heph_plugin_set_log_sink`: the host's sink for this cdylib's `tracing`
///   events. The cdylib links its own `tracing`, whose global subscriber is
///   never set, so without it every log the plugin emits vanishes.
/// - `heph_plugin_set_log_filter`: the host's filter as protobuf `LogFilter`
///   bytes, so events it would discard are dropped here instead of forwarded.
/// - `heph_plugin_set_supervisor`: the host's process-supervisor client. The
///   cdylib links its own `proc`, whose tracker the host's startup never
///   reached, so without it every child the plugin spawns goes unregistered
///   with the sidecar and is orphaned on a hard kill of the host.
/// - `heph_plugin_set_runner_host`: the host's runner registry. The cdylib
///   links its own `execrunner`, whose registry the engine's
///   `install_exec_runner_host` never reached, so without it a target that
///   names a `runner` fails with "no runner host is installed in this
///   component" rather than running in the environment it asked for.
#[cfg(feature = "stabby")]
#[macro_export]
macro_rules! export_host_entries {
    () => {
        #[unsafe(no_mangle)]
        pub extern "C" fn heph_plugin_abi_semver() -> *const ::std::ffi::c_char {
            $crate::stabby::abi_semver()
        }

        #[stabby::export]
        pub extern "C" fn heph_plugin_set_log_sink(sink: $crate::stabby::abi::DynLogSink) {
            $crate::stabby::install_log_sink(sink);
        }

        #[stabby::export]
        pub extern "C" fn heph_plugin_set_log_filter(filter: stabby::vec::Vec<u8>) {
            $crate::stabby::install_log_filter(&filter);
        }

        #[stabby::export]
        pub extern "C" fn heph_plugin_set_supervisor(sup: $crate::stabby::abi::DynSupervisor) {
            $crate::stabby::install_supervisor(sup);
        }

        #[stabby::export]
        pub extern "C" fn heph_plugin_set_runner_host(host: $crate::stabby::abi::DynRunnerHost) {
            $crate::stabby::install_runner_host(host);
        }
    };
}
