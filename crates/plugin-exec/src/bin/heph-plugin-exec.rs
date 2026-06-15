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

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let opts = hplugin::config::Options::new();
    let mut managed: HashMap<String, Arc<dyn ManagedDriver>> = HashMap::new();
    managed.insert("exec".to_string(), Arc::new(Driver::from_options_exec(&opts)?));
    managed.insert("bash".to_string(), Arc::new(Driver::from_options_bash(&opts)?));
    managed.insert("sh".to_string(), Arc::new(Driver::from_options_sh(&opts)?));
    plugin_sdk::serve_components_inherited(None, managed).await
}
