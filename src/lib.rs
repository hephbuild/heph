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

pub mod commands;
pub mod debug_hash;
pub mod defer;
pub mod engine;
pub mod fdlimit;
pub mod hartifactcontent;
pub mod hasync;
pub mod hlock;
pub mod hmemoizer;
pub mod htaddr;
pub mod htmatcher;
pub mod htpkg;
pub mod log;
pub mod loosespecparser;
pub mod pluginbuildfile;
pub mod pluginexec;
pub mod pluginfs;
pub mod plugingo;
pub mod plugingroup;
pub mod pluginhostbin;
pub mod pluginnix;
pub mod pluginquery;
pub mod pluginstatictarget;
pub mod plugintextfile;
pub mod proc_exec;
pub mod process_supervisor;
#[cfg(target_os = "macos")]
pub mod process_watcher;
pub mod sandboxfuse;
pub mod tui;
pub mod version;
