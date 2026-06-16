//! The exec/bash/sh driver plugin as an out-of-process plugin.
//!
//! pluginexec stays Rust — this binary relocates its three managed drivers
//! (exec, bash, sh) into a process served over the heph plugin transport via
//! the SDK. The host's ManagedDriverBridge still materializes the sandbox; this
//! process executes the command in it and returns output artifacts.
//!
//! Served over the inherited fd 3 (set up by the host's spawn).

use hdriver_support::driver_managed::ManagedDriver;
use plugin_exec::pluginexec::Driver;
use std::collections::HashMap;
use std::sync::Arc;

// Multi-thread runtime: the exec driver parks workers via `block_in_place`
// while reading the target's subprocess stdio, which requires a multi-thread
// runtime (current-thread falls back to a poll loop and serializes).
fn main() -> anyhow::Result<()> {
    let n = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(8);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(n)
        .max_blocking_threads(8 * n + 64)
        .enable_all()
        .build()?;
    rt.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    let opts = hplugin::config::Options::new();
    let mut managed: HashMap<String, Arc<dyn ManagedDriver>> = HashMap::new();
    managed.insert(
        "exec".to_string(),
        Arc::new(Driver::from_options_exec(&opts)?),
    );
    managed.insert(
        "bash".to_string(),
        Arc::new(Driver::from_options_bash(&opts)?),
    );
    managed.insert("sh".to_string(), Arc::new(Driver::from_options_sh(&opts)?));

    #[cfg(feature = "shm")]
    if let Ok(id) = std::env::var("HEPH_PLUGIN_SHM") {
        return plugin_sdk::serve_components_shm(&id, None, managed).await;
    }
    plugin_sdk::serve_components_inherited(None, managed).await
}
