//! The `exec`/`bash`/`sh` managed drivers: run a target's command in the
//! sandbox the managed-driver bridge materializes. Depends on the contract +
//! `heph-driver-support`; isolates the PTY / minijinja / crossterm surface.
#![cfg_attr(
    test,
    expect(
        clippy::panic_in_result_fn,
        clippy::undocumented_unsafe_blocks,
        reason = "restriction/style lints scoped to production code; tests are exempt"
    )
)]

pub mod pluginexec;

// The `htspec` derive macros expand to `crate::htvalue` / `crate::htspec`.
pub(crate) use hcore::htvalue;
pub(crate) use hplugin::htspec;
