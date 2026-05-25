use std::sync::OnceLock;

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

#[cfg(test)]
mod tests {
    use super::*;

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
