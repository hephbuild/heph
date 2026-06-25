//! FUSE-backed sandbox overlay.
//!
//! On systems with FUSE support the sandbox bridge mounts an in-memory
//! union filesystem at each unpack root instead of copying input artifacts
//! to disk. Read layers are served straight from the seekable tar bytes via
//! a pre-built `TarIndex`; writes land in an on-disk upper directory
//! (copy-on-write).
//!
//! The module is callable from all platforms; on non-Linux or with the
//! `fuse-sandbox` feature disabled, `support_check` returns `Unavailable`
//! and `Mount::mount` returns an error, so the bridge falls back to the
//! existing copy path.
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

mod support;

pub use support::{FuseSupport, MountBackend, support_check};

#[cfg(all(unix, feature = "fuse-sandbox"))]
mod imp;

#[cfg(all(unix, feature = "fuse-sandbox"))]
pub use imp::{Layer, LayerOpener, LayeredFs, Mount, SlotGuard};

#[cfg(not(all(unix, feature = "fuse-sandbox")))]
mod stub;

#[cfg(not(all(unix, feature = "fuse-sandbox")))]
pub use stub::{Layer, LayerOpener, LayeredFs, Mount, SlotGuard};
