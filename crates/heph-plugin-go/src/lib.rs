//! The `go` provider + driver family: discovers Go packages, builds libs/bins,
//! generates test mains, embeds, etc. The largest plugin; depends on the
//! contract, `heph-driver-support`, and the exec/query plugins it composes.
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

pub mod plugingo;

// The `htspec` derive macros expand to `crate::htvalue` / `crate::htspec`.
pub(crate) use heph_core::htvalue;
pub(crate) use heph_plugin::htspec;
