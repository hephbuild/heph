//! Host-side adapter for running external plugins behind the in-process
//! `hplugin::Provider`/`Driver` traits. The engine registers a `RemoteProvider`
//! (or, later, `RemoteDriver`) through its normal factory hooks and stays
//! unaware the plugin is out-of-process.
//!
//! Transports: proto (UDS, always available) is the default; `shm` (iceoryx2,
//! M3) and `wasm` (wasmtime, M4) are feature-gated.

pub mod lease;

mod host;
mod provider;
#[cfg(unix)]
mod spawn;

pub use provider::RemoteProvider;
#[cfg(unix)]
pub use spawn::{spawn_plugin, PLUGIN_FD};

#[cfg(feature = "shm")]
pub mod shm;
#[cfg(feature = "wasm")]
pub mod wasm;
