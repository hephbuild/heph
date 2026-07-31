//! The OCI plugin as a loadable cdylib behind the stable ABI.
//!
//! Exports a single stabby `create` entry that constructs the four `oci_*`
//! managed drivers and hands them back as ABI-stable handles. The host loads
//! this with `hplugin_stabby::load_stable::load`, which verifies ABI
//! compatibility via stabby's type reports before use.
//!
//! A driver-only plugin: images are declared by whatever provider the workspace
//! already uses (`BUILD` files), so there is no `oci` provider and no hooks.
//!
//! Nothing crosses in [`CreateConfig`] that the drivers need — they take the
//! `docker` / `skopeo` binaries from `PATH`, the same host capability the
//! in-process build used.

use hdriver_support::driver_managed::ManagedDriver;
use hplugin_oci::pluginoci;
use plugin_sdk::stabby::abi::{DynLogSink, DynSupervisor, NamedDriver, PluginComponents};
use plugin_sdk::stabby::{install_log_sink, install_supervisor, make_dyn_managed_driver};
use std::sync::Arc;

/// Stable ABI create entry. `#[stabby::export]` emits the type-report symbols the
/// host's `get_stabbied` checks for ABI compatibility. `cfg` is prost-encoded
/// `pb::CreateConfig` bytes; this plugin reads nothing out of it, but the
/// parameter stays so config fields can be added without an ABI change.
#[stabby::export]
pub extern "C" fn heph_plugin_create(_cfg: stabby::vec::Vec<u8>) -> PluginComponents {
    build()
}

/// Stable ABI log-sink entry: the host calls this right after `create` to hand
/// the plugin a sink for its `tracing` events. Without it, this cdylib's
/// statically-linked `tracing` has no subscriber and every `oci_image built` /
/// `oci_push: pushed` line vanishes.
#[stabby::export]
pub extern "C" fn heph_plugin_set_log_sink(sink: DynLogSink) {
    install_log_sink(sink);
}

/// Stable ABI supervisor entry: the host hands the plugin its process-supervisor
/// client. This cdylib links its own `proc`, whose tracker the host's startup
/// `init` never reached — without this, every `docker buildx` and `skopeo` this
/// plugin spawns goes unregistered with the sidecar and is orphaned on a hard
/// kill of the host. A detached `buildx` is exactly the child that must not
/// survive: it keeps writing into a sandbox the cleaner is deleting.
#[stabby::export]
pub extern "C" fn heph_plugin_set_supervisor(sup: DynSupervisor) {
    install_supervisor(sup);
}

fn build() -> PluginComponents {
    let mut drivers = stabby::vec::Vec::new();

    // Builds a Dockerfile + context into a cacheable image archive.
    let image: Arc<dyn ManagedDriver> = Arc::new(pluginoci::Driver::new());
    drivers.push(NamedDriver {
        name: pluginoci::DRIVER_NAME.into(),
        driver: make_dyn_managed_driver(image),
    });
    // Pulls a base image into a cacheable archive (or OCI layout).
    let pull: Arc<dyn ManagedDriver> = Arc::new(pluginoci::pull::Driver::new());
    drivers.push(NamedDriver {
        name: pluginoci::pull::DRIVER_NAME.into(),
        driver: make_dyn_managed_driver(pull),
    });
    // Actions — uncached, they mutate a registry or the local daemon.
    let push: Arc<dyn ManagedDriver> = Arc::new(pluginoci::push::Driver::new());
    drivers.push(NamedDriver {
        name: pluginoci::push::DRIVER_NAME.into(),
        driver: make_dyn_managed_driver(push),
    });
    let load: Arc<dyn ManagedDriver> = Arc::new(pluginoci::load::Driver::new());
    drivers.push(NamedDriver {
        name: pluginoci::load::DRIVER_NAME.into(),
        driver: make_dyn_managed_driver(load),
    });

    PluginComponents {
        // Driver-only: images are declared through the workspace's own provider.
        provider_name: String::new().into(),
        provider: stabby::option::Option::None(),
        drivers,
        hooks: stabby::vec::Vec::new(),
        meta: stabby::vec::Vec::new(),
    }
}
