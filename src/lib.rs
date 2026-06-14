// Restriction lints in `[workspace.lints.clippy]` are production guards. Test
// code legitimately uses panicking helpers, direct map access, and fixture
// asserts, so exempt the test cfg from them here rather than rewriting every
// test. `expect` (not `allow`) keeps these honest: if a listed lint stops
// firing in tests, the unfulfilled expectation surfaces it.
#![cfg_attr(
    test,
    expect(
        clippy::panic_in_result_fn,
        clippy::assertions_on_result_states,
        clippy::err_expect,
        unused_imports,
        reason = "restriction/style lints scoped to production code; tests are exempt"
    )
)]

extern crate core;

// Foundational leaves now live in `heph-core`; re-export them at their original
// module paths so `crate::hasync::…` etc. keep resolving unchanged across the
// monolith.
pub use hbuiltins::{pluginfs, plugingroup, pluginhostbin, pluginstatictarget, plugintextfile};
pub use hcore::{
    debug_hash, defer, hartifactcontent, hasync, hmemoizer, htplatform, htvalue, version,
};
pub use hengine::engine;
pub use hlock::hlock;
pub use hmodel::{htaddr, htmatcher, htpkg};
pub use hplugin::htspec;
pub use hplugin_buildfile::pluginbuildfile;
pub use hplugin_exec::pluginexec;
pub use hplugin_go::plugingo;
pub use hplugin_nix::pluginnix;
pub use hplugin_query::pluginquery;
#[cfg(target_os = "macos")]
pub use hproc::process_watcher;
pub use hproc::{proc_exec, process_supervisor};
pub use hsandboxfuse as sandboxfuse;
pub use htelemetry::telemetry;
pub use htui::tui;
pub use hwalk as htwalk;

pub mod commands;
pub mod fdlimit;
pub mod log;
#[cfg(test)]
mod pluginquery_test;
