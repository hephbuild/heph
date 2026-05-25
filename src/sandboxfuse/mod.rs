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

mod support;

pub use support::{FuseSupport, support_check};

#[cfg(all(unix, feature = "fuse-sandbox"))]
mod imp;

#[cfg(all(unix, feature = "fuse-sandbox"))]
pub use imp::{Layer, LayerOpener, LayeredFs, Mount, SlotGuard};

#[cfg(not(all(unix, feature = "fuse-sandbox")))]
mod stub;

#[cfg(not(all(unix, feature = "fuse-sandbox")))]
pub use stub::{Layer, LayerOpener, LayeredFs, Mount, SlotGuard};
