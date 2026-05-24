#[expect(
    clippy::module_inception,
    reason = "module and struct share name by design"
)]
mod engine;
pub use engine::Config;
pub use engine::Engine;
pub use engine::MemCacheOptions;
pub mod config_file;
mod cwd;
mod result;
pub use cwd::get_cwd;
pub use cwd::get_cwp;
mod root;
pub use root::get_root;
pub mod driver;
pub mod error;
mod local_cache;
#[cfg(test)]
mod local_cache_fs;
mod local_cache_mem;
mod local_cache_sqlite;
mod packages;
pub mod provider;
mod query;
pub mod request_state;
pub mod spec;
pub use result::ArtifactMeta;
pub use result::EResult;
pub use result::OutputMatcher;
pub use result::ResultOptions;
pub use result::{InteractiveInner, InteractiveWrapper};
pub use spec::EngineTargetSpec;
pub mod driver_managed;
mod execute;
mod expand;
mod link;
mod matcher_spec;
mod meta;
mod sandbox_cleaner;
