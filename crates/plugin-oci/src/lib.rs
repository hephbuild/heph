//! The container rules: build an image, and move one between a registry, the
//! local daemon and the cache. Depends on the contract + `heph-driver-support`.
//!
//! The plugin is named for the artifact — an OCI image — and each *driver* is
//! named for what it needs from the host, because that is the part a user has to
//! plan around. `docker_build` says it shells out to `docker buildx`; `oci_push`
//! and `oci_pull` speak the distribution protocol in-process and need no host
//! binary at all. See [`pluginoci`] for the split.
//!
//! Not compiled into the CLI: like the go plugin, this ships as a loadable
//! cdylib (`crates/plugin-oci-cdylib`) published per os/arch with a
//! `heph-oci-plugin.json` manifest, selected by a workspace with
//!
//! ```yaml
//! plugins:
//!   - url: https://…/heph-oci-plugin.json
//! ```
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
