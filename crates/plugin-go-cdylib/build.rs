//! Weak-link libfuse on macOS (see `crates/sandboxfuse/build.rs` for the full
//! rationale). This crate's binary/cdylib transitively links `fuser` -> libfuse
//! through its hplugin/sandboxfuse deps (the default-on `fuse-sandbox` feature,
//! unified across the workspace). `rustc-link-arg` does NOT propagate from a
//! dependency's build script, so this crate must emit `-weak-lfuse` at its own
//! final link or its artifact hard-links `/usr/local/lib/libfuse.2.dylib` and
//! fails to launch/load on hosts without macFUSE — including CI runners.
fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo::rustc-link-arg=-weak-lfuse");
    }
}
