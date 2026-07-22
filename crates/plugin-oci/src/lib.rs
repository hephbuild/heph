//! The `oci_image` managed driver: builds an OCI (or docker-format) image
//! archive from a build context as a cacheable target output. Depends on the
//! contract + `heph-driver-support`.
//!
//! Docker/BuildKit is the builder heph shells out to, but the plugin is named
//! for what it *produces* — an OCI image archive — so the builder can be swapped
//! (buildah, podman, a hermetic buildkit) without changing the plugin's
//! identity. See sibling `oci_push` / `oci_load` action drivers (later
//! milestones) that consume this driver's tar output.
#![cfg_attr(
    test,
    allow(
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
        reason = "restriction/style lints scoped to production code; tests are exempt"
    )
)]

pub mod pluginoci;

// The `htspec` derive macros expand to `crate::htvalue` / `crate::htspec`.
pub(crate) use hcore::htvalue;
pub(crate) use hplugin::htspec;
