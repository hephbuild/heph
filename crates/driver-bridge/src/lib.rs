//! Engine-side managed-driver orchestration: the `ManagedDriverBridge` that
//! materializes a sandbox (FUSE overlay or OS copy) and dispatches to the inner
//! `ManagedDriver`, plus the FUSE backend (`ManagedDriverFuse`). This is the
//! crate that links `fuser`/libfuse; the contract-level `heph-driver-support` —
//! and therefore every plugin that merely *implements* `ManagedDriver` — stays
//! fuse-free. The engine wires the bridge via `Engine::new_managed_driver`.
#![cfg_attr(
    test,
    expect(
        clippy::panic_in_result_fn,
        clippy::unimplemented,
        reason = "restriction lints scoped to production code; tests are exempt"
    )
)]

pub mod bridge;
pub mod driver_managed_fuse;

pub use bridge::{FuseSlot, ManagedDriverBridge};
