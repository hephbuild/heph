use std::sync::OnceLock;

/// Which userspace FUSE backend a mount should use.
///
/// Only meaningful on macOS, where macFUSE exposes two backends. On Linux the
/// kernel FUSE driver is the only path and this is ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MountBackend {
    /// Linux kernel FUSE, or macOS macFUSE kernel-extension backend. The
    /// fastest path; on macOS it requires the macFUSE system extension to be
    /// approved. This is the default.
    #[default]
    Kernel,
    /// Apple FSKit backend (macOS 15.4+). Unprivileged — no kernel extension —
    /// but mounts only under `/Volumes` and has reduced feature support. The
    /// mountpoint volume is created by macFUSE's privileged mount service.
    Fskit,
}

/// Outcome of the FUSE probe.
#[derive(Debug, Clone)]
pub enum FuseSupport {
    Available,
    Unavailable(String),
}

impl FuseSupport {
    pub fn is_available(&self) -> bool {
        matches!(self, FuseSupport::Available)
    }
}

static CACHE: OnceLock<FuseSupport> = OnceLock::new();

/// Cached runtime probe. Cheap on subsequent calls. The probe opens
/// `/dev/fuse` on Linux; any other host or compile-time configuration
/// reports `Unavailable` with a fixed reason.
pub fn support_check() -> &'static FuseSupport {
    CACHE.get_or_init(probe)
}

#[cfg(all(target_os = "linux", feature = "fuse-sandbox"))]
fn probe() -> FuseSupport {
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
    {
        Ok(_) => FuseSupport::Available,
        Err(e) => FuseSupport::Unavailable(format!("/dev/fuse not accessible: {e}")),
    }
}

#[cfg(all(target_os = "macos", feature = "fuse-sandbox"))]
fn probe() -> FuseSupport {
    // macFUSE installs its kext bundle here; presence is the cheapest
    // proxy for "user has the userland half ready". The actual mount
    // syscall validates the kext itself.
    if std::path::Path::new("/Library/Filesystems/macfuse.fs").exists() {
        FuseSupport::Available
    } else {
        FuseSupport::Unavailable(
            "macFUSE not installed (/Library/Filesystems/macfuse.fs missing)".to_string(),
        )
    }
}

#[cfg(not(any(
    all(target_os = "linux", feature = "fuse-sandbox"),
    all(target_os = "macos", feature = "fuse-sandbox"),
)))]
fn probe() -> FuseSupport {
    FuseSupport::Unavailable(
        "FUSE sandbox is gated on unix with feature `fuse-sandbox`".to_string(),
    )
}

/// Skip the enclosing `#[test]` when FUSE is unavailable on this host. Use
/// in tests that actually exercise a real FUSE mount; the body returns
/// early after printing a one-line skip reason.
#[macro_export]
macro_rules! require_fuse {
    () => {
        match $crate::support_check() {
            $crate::FuseSupport::Available => {}
            $crate::FuseSupport::Unavailable(reason) => {
                eprintln!("skipping: FUSE unavailable: {reason}");
                return;
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuse_sandbox_is_a_default_feature() {
        // The FUSE sandbox ships on by default (Linux: pure-Rust, no libfuse
        // link; macOS: libfuse). If someone drops `fuse-sandbox` from
        // `default` in Cargo.toml, this default-features `cargo test` build
        // stops seeing the feature and fails here.
        assert!(
            cfg!(feature = "fuse-sandbox"),
            "fuse-sandbox must remain a default Cargo feature"
        );
    }

    #[test]
    fn support_check_returns_stable_outcome() {
        let a = support_check();
        let b = support_check();
        // Same cached pointer.
        assert!(std::ptr::eq(a, b));
        // On dev hosts without /dev/fuse we expect Unavailable; on Linux CI
        // with FUSE it's Available. Either is fine — just verify the shape.
        match a {
            FuseSupport::Available => {}
            FuseSupport::Unavailable(msg) => assert!(!msg.is_empty()),
        }
    }
}
