//! The `go` provider + driver family: discovers Go packages, builds libs/bins,
//! generates test mains, embeds, etc. The largest plugin; depends on the
//! contract, `heph-driver-support`, and the exec/query plugins it composes.
#![cfg_attr(
    test,
    expect(
        clippy::get_unwrap,
        clippy::assertions_on_result_states,
        clippy::unimplemented,
        clippy::let_underscore_must_use,
        clippy::cloned_ref_to_slice_refs,
        unused_imports,
        reason = "restriction/style lints scoped to production code; tests are exempt"
    )
)]

pub mod plugingo;

// The `htspec` derive macros expand to `crate::htvalue` / `crate::htspec`.
pub(crate) use hcore::htvalue;
pub(crate) use hplugin::htspec;
