//! Weak-link libfuse on macOS so the binary launches without macFUSE installed.
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
//! Linux FUSE is pure-Rust with no libfuse link, so nothing is emitted there.
fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    let macos = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos");
    // Only when libfuse is actually linked (the only config that links it).
    let fuse = std::env::var_os("CARGO_FEATURE_FUSE_SANDBOX").is_some();
    if macos && fuse {
        println!("cargo::rustc-link-arg=-weak-lfuse");
    }
}
