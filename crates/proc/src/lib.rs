//! Process execution + lifecycle: spawn/reap subprocess groups (`proc_exec`),
//! the out-of-process supervisor sidecar that reaps orphaned groups
//! (`process_supervisor`), and the macOS kqueue-based exit watcher
//! (`process_watcher`). Depends only on `heph-core` (cancellation).
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

pub mod proc_exec;
pub mod process_supervisor;
#[cfg(target_os = "macos")]
pub mod process_watcher;
