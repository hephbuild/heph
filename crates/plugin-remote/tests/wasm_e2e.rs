//! wasm transport vertical slice (M4 de-risk).
//!
//! Builds the `echo` guest component with cargo-component, loads it through the
//! wasmtime host in `plugin_remote::wasm`, and calls its `greet` export. Proves
//! the cargo-component guest ↔ wasmtime host contract end-to-end before the full
//! provider/driver WIT is brought up.
//!
//! Gated behind `--features wasm` (the wasm toolchain — cargo-component,
//! wasm32-wasip1 std — is only present in the devenv shell). Run with:
//!   devenv shell -- cargo test -p plugin-remote --features wasm
#![cfg(feature = "wasm")]

use std::path::PathBuf;
use std::process::Command;

/// Build the echo guest component into `out_dir` and return the path to the
/// produced `.wasm` component.
fn build_echo_component(out_dir: &std::path::Path) -> PathBuf {
    let guest_manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../wasm-guests/echo/Cargo.toml")
        .canonicalize()
        .expect("echo guest manifest exists");

    let status = Command::new("cargo")
        .args(["component", "build"])
        .arg("--manifest-path")
        .arg(&guest_manifest)
        .arg("--target")
        .arg("wasm32-wasip1")
        .arg("--target-dir")
        .arg(out_dir)
        .status()
        .expect("cargo-component is on PATH (run inside `devenv shell`)");
    assert!(status.success(), "cargo component build failed");

    let wasm = out_dir.join("wasm32-wasip1/debug/echo.wasm");
    assert!(wasm.exists(), "expected component at {}", wasm.display());
    wasm
}

#[test]
fn echo_component_greets_over_wasmtime_host() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let wasm_path = build_echo_component(tmp.path());
    let wasm = std::fs::read(&wasm_path).expect("read built component");

    // greet() calls back into the host's `host-lookup` import (-> "host:heph")
    // and folds it into the reply — exercising both call directions over wasm.
    let out = plugin_remote::wasm::instantiate_and_greet(&wasm, "heph")
        .expect("instantiate + call greet");
    assert_eq!(out, "hello heph (host:heph)");
}
