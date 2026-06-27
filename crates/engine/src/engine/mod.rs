#[expect(
    clippy::module_inception,
    reason = "module and struct share name by design"
)]
pub mod engine;
pub use engine::Engine;
pub use engine::EngineFuse;
pub use engine::PluginInit;
pub mod config;
pub use config::Config;
pub use config::ConfigYamlExt;
pub use config::DEFAULT_SPILL_THRESHOLD_BYTES;
pub use config::MemCacheOptions;
// The YAML config shape + loader + workspace-root discovery now live in the
// engine-free `config` crate (so layers that run before the engine — e.g. the
// self-upgrade check — can read it). Re-export under the original paths so
// `crate::engine::config_yaml::…`, `engine::get_root`, `engine::get_cwd` resolve
// unchanged across the monolith.
pub use hconfig as config_yaml;
pub use hconfig::{FuseConfig, FuseMode, get_cwd, get_root};
mod plugin_load;
pub use plugin_load::clear_plugin_cache;
mod cwd;
mod result;
pub use cwd::get_cwp;
// The plugin contract (driver/provider/error + targetdef/eresult/htspec) now
// lives in the `heph-plugin` crate; re-export at the original engine paths so
// `crate::engine::driver::…` etc. resolve unchanged across engine + plugins.
pub use hplugin::driver;
pub use hplugin::error;
pub use hplugin::hook;
pub mod event;
mod local_cache;
mod local_cache_fs;
mod local_cache_mem;
mod local_cache_spill;
mod local_cache_sqlite;
mod local_cache_tmp;
mod packages;
mod remote_cache;
mod remote_cache_latency;
mod remote_cache_objstore;
pub use hplugin::provider;
pub use remote_cache::{RemoteCacheDef, RemoteCacheSet};
mod query;
pub mod request_state;
mod revdeps;
pub mod spec;
pub use result::ArtifactMeta;
pub use result::BatchResult;
pub use result::EResult;
pub use result::OutputMatcher;
pub use result::ResultOptions;
pub use result::{InteractiveInner, InteractiveWrapper};
pub use spec::EngineTargetSpec;
// The managed-driver contract lives in fuse-free `heph-driver-support`; the
// FUSE backend + routing bridge live in `heph-driver-bridge`. Re-export both at
// the old engine paths. `managed_register` adds `Engine::new_managed_driver`
// (engine state + the pluginexec shell fallback).
pub use hdriver_bridge::driver_managed_fuse;
pub use hdriver_support::driver_managed;
pub use hdriver_support::driver_managed_os;
mod execute;
mod managed_register;
mod result_lock;
pub use result_lock::{LockBackend, ResultLock};
mod expand;
pub mod fanout;
mod gc;
pub use gc::GcStats;
pub mod gitignore;
mod grow_stack;
mod link;
mod matcher_spec;
pub mod matcher_target;
mod meta;
pub mod sandbox_cleaner;
pub mod validate;
