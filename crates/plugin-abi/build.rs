//! Weak-link libfuse on macOS (see `crates/sandboxfuse/build.rs` for the full
//! rationale). This crate transitively links `fuser` -> libfuse through its
//! `hplugin`/`sandboxfuse` dependencies (the `fuse-sandbox` feature, default-on
//! at the `heph` bin and unified across the workspace by `cargo test --all`).
//! `rustc-link-arg` does NOT propagate from a dependency's build script to a
//! dependent's binaries, so every test-binary-producing crate that links fuser
//! must emit `-weak-lfuse` at its own final link or its test binary hard-links
//! `/usr/local/lib/libfuse.2.dylib` and dyld aborts at launch on hosts without
//! macFUSE — including CI runners.
fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo::rustc-link-arg=-weak-lfuse");
    }
}
