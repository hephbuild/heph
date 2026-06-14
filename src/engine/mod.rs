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
pub use config::DEFAULT_SPILL_THRESHOLD_BYTES;
pub use config::MemCacheOptions;
pub mod config_yaml;
pub use config_yaml::{FuseConfig, FuseMode};
mod cwd;
mod result;
pub use cwd::get_cwd;
pub use cwd::get_cwp;
mod root;
pub use root::get_root;
// The plugin contract (driver/provider/error + targetdef/eresult/htspec) now
// lives in the `heph-plugin` crate; re-export at the original engine paths so
// `crate::engine::driver::…` etc. resolve unchanged across engine + plugins.
pub use heph_plugin::driver;
pub use heph_plugin::error;
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
pub use heph_plugin::provider;
pub use remote_cache::{RemoteCacheDef, RemoteCacheSet};
mod query;
pub mod request_state;
pub mod spec;
pub use result::ArtifactMeta;
pub use result::BatchResult;
pub use result::EResult;
pub use result::OutputMatcher;
pub use result::ResultOptions;
pub use result::{InteractiveInner, InteractiveWrapper};
pub use spec::EngineTargetSpec;
pub mod driver_managed;
pub mod driver_managed_fuse;
pub mod driver_managed_os;
mod execute;
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
