//! Weak-link libfuse on macOS (see the workspace-root `build.rs` for the full
//! rationale). This crate's test binaries transitively link `fuser` → libfuse
//! through its `rheph` dependency (default `fuse-sandbox` feature). Without the
//! flag they would hard-link `/usr/local/lib/libfuse.2.dylib` and abort at
//! launch on machines without macFUSE — including CI runners. The flag makes
//! the dylib an optional weak load; libfuse symbols are only reached behind
//! rheph's runtime `support_check` gate.
fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo::rustc-link-arg=-weak-lfuse");
    }
}
