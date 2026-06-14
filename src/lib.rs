// Restriction lints in `[workspace.lints.clippy]` are production guards. Test
// code legitimately uses panicking helpers, direct map access, and fixture
// asserts, so exempt the test cfg from them here rather than rewriting every
// test. `expect` (not `allow`) keeps these honest: if a listed lint stops
// firing in tests, the unfulfilled expectation surfaces it.
#![cfg_attr(
    test,
    expect(
        clippy::get_unwrap,
        clippy::panic_in_result_fn,
        clippy::assertions_on_result_states,
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::unimplemented,
        clippy::undocumented_unsafe_blocks,
        clippy::unreachable,
        clippy::let_underscore_must_use,
        clippy::float_cmp,
        clippy::assertions_on_constants,
        clippy::cloned_ref_to_slice_refs,
        clippy::err_expect,
        unused_imports,
        reason = "restriction/style lints scoped to production code; tests are exempt"
    )
)]

extern crate core;

// Foundational leaves now live in `heph-core`; re-export them at their original
// module paths so `crate::hasync::…` etc. keep resolving unchanged across the
// monolith.
pub use heph_core::{
    debug_hash, defer, hartifactcontent, hasync, hmemoizer, htplatform, htvalue, version,
};
pub use heph_model::{htaddr, htmatcher, htpkg};
pub use heph_plugin::htspec;
pub use heph_builtins::{
    pluginfs, plugingroup, pluginhostbin, pluginstatictarget, plugintextfile,
};
pub use heph_plugin_buildfile::pluginbuildfile;
pub use heph_plugin_exec::pluginexec;
pub use heph_plugin_nix::pluginnix;
pub use heph_plugin_query::pluginquery;
pub use heph_plugin_go::plugingo;
pub use heph_telemetry::telemetry;
pub use heph_tui::tui;
#[cfg(target_os = "macos")]
pub use heph_proc::process_watcher;
pub use heph_proc::{proc_exec, process_supervisor};
pub use heph_sandboxfuse as sandboxfuse;
pub use heph_walk as htwalk;

pub mod commands;
pub mod engine;
pub mod fdlimit;
pub mod hlock;
pub mod log;
#[cfg(test)]
mod plugingo_e2e_test;
#[cfg(test)]
mod pluginquery_test;
