//! Weak-link libfuse on macOS so binaries launch without macFUSE installed.
//!
//! With the default `fuse-sandbox` feature, `fuser` links libfuse on macOS
//! (there is no pure-Rust mount path there). A normal link makes
//! `/usr/local/lib/libfuse.2.dylib` a hard dependency: dyld aborts the whole
//! process at launch if it is missing — before any config or `support_check`
//! runs. Emitting `-weak-lfuse` turns it into an `LC_LOAD_WEAK_DYLIB`: dyld
//! tolerates the dylib being absent and binds its symbols to null. Those
//! symbols are only ever called behind the runtime `support_check` gate (which
//! requires macFUSE present), so a null is never dereferenced.
//!
//! Emitted here so this crate's own test binary links cleanly; every other
//! binary-producing workspace crate (the `heph` bin, e2e, testkit) emits the
//! same flag for its final link.
//!
//! Linux FUSE is pure-Rust with no libfuse link, so nothing is emitted there.
fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    let macos = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos");
    let fuse = std::env::var_os("CARGO_FEATURE_FUSE_SANDBOX").is_some();
    if macos && fuse {
        println!("cargo::rustc-link-arg=-weak-lfuse");
    }
}
