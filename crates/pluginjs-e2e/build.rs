//! Weak-link libfuse on macOS — identical rationale to
//! `crates/plugingo-e2e/build.rs`: this crate's test binaries transitively
//! link `fuser` → libfuse through `heph`'s default `fuse-sandbox` feature.
//! Without the flag they would hard-link
//! `/usr/local/lib/libfuse.2.dylib` and abort at launch on machines without
//! macFUSE — including CI runners.
fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo::rustc-link-arg=-weak-lfuse");
    }
}
