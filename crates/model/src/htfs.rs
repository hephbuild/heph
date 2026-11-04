//! The fs provider's addr vocabulary (`//@heph/fs:file`, `//@heph/fs:glob`).
//!
//! The provider itself lives in `builtins::pluginfs`; the sandbox staging in
//! `driver-support` must recognize its addrs too, and cannot depend on
//! `builtins` (that edge would cycle). Both read the one definition here, so the
//! staging-collision exemption and the provider's own overlap rule cannot drift.

use crate::htaddr::Addr;

/// The fs provider's package.
pub const PKG: &str = "@heph/fs";

/// Glob addr arg selecting the hashing/staging mode; absent is content mode.
pub const GLOB_MODE_ARG: &str = "m";

/// Returns `true` if `addr` belongs to the fs provider.
pub fn is_fs_addr(addr: &Addr) -> bool {
    addr.package.as_str() == PKG
}

/// Whether two fs-provider addrs that claim the same sandbox path are certain to
/// provide the same bytes there, so the overlap is benign.
///
/// Two content-mode fs targets (a `file` and a `glob`, or two globs) both expose
/// the workspace file itself. A names-only glob stages an empty placeholder, so
/// its overlap with a content target would put a placeholder over real bytes (or
/// the reverse) depending on staging order: that is a collision, not an overlap.
/// Two names globs both stage the empty placeholder, so they agree.
pub fn fs_overlap_is_benign(a: &Addr, b: &Addr) -> bool {
    is_fs_addr(a) && is_fs_addr(b) && a.args.get(GLOB_MODE_ARG) == b.args.get(GLOB_MODE_ARG)
}
