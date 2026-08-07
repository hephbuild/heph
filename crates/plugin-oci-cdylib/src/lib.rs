//! The OCI plugin as a loadable cdylib behind the stable ABI.
//!
//! Exports a single stabby `create` entry that constructs the plugin's managed
//! drivers and hands them back as ABI-stable handles. The host loads this with
//! `hplugin_stabby::load_stable::load`, which verifies ABI compatibility via
//! stabby's type reports before use.
//!
//! Images themselves are declared by whatever provider the workspace already
//! uses (`BUILD` files). The one thing this plugin's own provider contributes is
//! `//@heph/oci:platform` — the builder-platform probe a `docker_build` without
//! explicit `platforms` depends on, so that platform reaches the cache key as an
//! input hash rather than as a parse-time side effect. No hooks.
//!
//! Nothing crosses in [`CreateConfig`] that the drivers need — `docker_build`
//! and the platform probe take the `docker` binary from `PATH`, the same host
//! capability the in-process build used, and `oci_push` / `oci_pull` need no
//! host binary at all.

use hdriver_support::driver_managed::ManagedDriver;
use hplugin_oci::pluginoci;
use plugin_sdk::stabby::abi::{DynLogSink, DynSupervisor, NamedDriver, PluginComponents};
use plugin_sdk::stabby::{
    install_log_sink, install_supervisor, make_dyn_managed_driver, make_dyn_provider,
};
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
/// statically-linked `tracing` has no subscriber and every `docker_build built` /
/// `oci_push: pushed` line vanishes.
#[stabby::export]
pub extern "C" fn heph_plugin_set_log_sink(sink: DynLogSink) {
    install_log_sink(sink);
}

/// Stable ABI supervisor entry: the host hands the plugin its process-supervisor
/// client. This cdylib links its own `proc`, whose tracker the host's startup
/// `init` never reached — without this, every `docker buildx` this
/// plugin spawns goes unregistered with the sidecar and is orphaned on a hard
/// kill of the host. A detached `buildx` is exactly the child that must not
/// survive: it keeps writing into a sandbox the cleaner is deleting.
#[stabby::export]
pub extern "C" fn heph_plugin_set_supervisor(sup: DynSupervisor) {
    install_supervisor(sup);
}

fn build() -> PluginComponents {
    let mut drivers = stabby::vec::Vec::new();

    // Assembles target outputs into an image. No daemon, no execution.
    let image: Arc<dyn ManagedDriver> = Arc::new(pluginoci::image::Driver::new());
    drivers.push(NamedDriver {
        name: pluginoci::image::DRIVER_NAME.into(),
        driver: make_dyn_managed_driver(image),
    });
    let layer: Arc<dyn ManagedDriver> = Arc::new(pluginoci::layer::Driver::new());
    drivers.push(NamedDriver {
        name: pluginoci::layer::DRIVER_NAME.into(),
        driver: make_dyn_managed_driver(layer),
    });
    // Groups per-platform images into one multi-platform image.
    let index: Arc<dyn ManagedDriver> = Arc::new(pluginoci::index::Driver::new());
    drivers.push(NamedDriver {
        name: pluginoci::index::DRIVER_NAME.into(),
        driver: make_dyn_managed_driver(index),
    });
    // Builds a Dockerfile + context into a cacheable image archive.
    let docker: Arc<dyn ManagedDriver> = Arc::new(pluginoci::docker_build::Driver::new());
    drivers.push(NamedDriver {
        name: pluginoci::docker_build::DRIVER_NAME.into(),
        driver: make_dyn_managed_driver(docker),
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
    // Asks buildx which platform it would build by default. Depended on, never
    // named by a user.
    let platform: Arc<dyn ManagedDriver> = Arc::new(pluginoci::platform::Driver::new());
    drivers.push(NamedDriver {
        name: pluginoci::platform::DRIVER_NAME.into(),
        driver: make_dyn_managed_driver(platform),
    });

    let provider: Arc<dyn hplugin::provider::Provider> = Arc::new(pluginoci::platform::Provider);

    PluginComponents {
        provider_name: "oci".into(),
        provider: stabby::option::Option::Some(make_dyn_provider(provider)),
        drivers,
        hooks: stabby::vec::Vec::new(),
        meta: stabby::vec::Vec::new(),
    }
}
