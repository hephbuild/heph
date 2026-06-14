//! The managed-driver base shared by the exec-family plugins (exec/go/nix): the
//! `ManagedDriver` trait and the `ManagedDriverBridge` that materializes a
//! sandbox (FUSE overlay or copy) and dispatches `parse`/`run`/`run_shell` to
//! the inner driver. Depends only on the contract + FUSE crate; the engine wires
//! it (`Engine::new_managed_driver`) and supplies the pluginexec shell fallback.
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

pub mod driver_managed;
pub mod driver_managed_fuse;
pub mod driver_managed_os;
pub mod fuseconfig;
