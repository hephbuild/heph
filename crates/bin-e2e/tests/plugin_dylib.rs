//! The cdylib plugin loader, against the cdylibs CI actually publishes.
//!
//! In-process tests construct `plugin-go` / `plugin-gha` directly and call
//! them through Rust generics — the dynamic seam is never crossed. Everything
//! that can only break at the seam is invisible to them: a symbol missing from
//! the built `.so`/`.dylib`, an ABI version the host refuses, a plugin whose
//! own statically-linked tokio/tracing misbehaves once loaded into a foreign
//! process, and the manifest → checksum → `dlopen` resolution chain.

mod common;

use common::{BASE_CONFIG, Dist, Workspace, describe, sha256_file, write_manifest};

/// A shipped cdylib must load *and answer*. Merely dlopening proves little —
/// `inspect functions` makes the host call `functions()` on the loaded provider
/// and render the signature it returns, so a passing assertion means a real
/// round trip across the ABI seam with real data coming back.
#[test]
fn shipped_go_cdylib_loads_and_answers_across_the_abi() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let dylib = dist.plugin("go");
    assert!(dylib.is_file(), "missing {}", dylib.display());

    let manifest = ws.root().join("heph-go-plugin.json");
    let sum = sha256_file(&dylib).expect("hash go cdylib");
    write_manifest(&manifest, "go", &dylib, Some(&sum)).expect("write manifest");

    // `gotool: host` keeps the fixture offline: toolchain resolution is lazy, so
    // nothing is downloaded and no `go` is executed unless a Go target is built
    // — and none is. This test is about the loader, not about Go.
    ws.config(&format!(
        "{BASE_CONFIG}  - path: {}\n    options:\n      gotool: \"host\"\n",
        manifest.display()
    ))
    .expect("write config");

    let out = ws.run(&dist, &["inspect", "functions"]).expect("run");
    assert!(out.status.success(), "{}", describe(&out));

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("build_addr"),
        "go provider's `build_addr` did not come back across the seam: {}",
        describe(&out)
    );
}

/// The second shipped cdylib, exporting a hook rather than a provider — a
/// different export kind over the same seam, so a loader that only handles
/// providers fails here and nowhere else.
#[test]
fn shipped_gha_cdylib_loads() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let dylib = dist.plugin("gha");
    assert!(dylib.is_file(), "missing {}", dylib.display());

    let manifest = ws.root().join("heph-gha-plugin.json");
    let sum = sha256_file(&dylib).expect("hash gha cdylib");
    write_manifest(&manifest, "gha", &dylib, Some(&sum)).expect("write manifest");

    ws.config(&format!("{BASE_CONFIG}  - path: {}\n", manifest.display()))
        .expect("write config");
    ws.write(
        "pkg/BUILD",
        "target(name = \"ok\", driver = \"bash\", run = \"echo e2e-ok\", cache = False)\n",
    )
    .expect("write BUILD");

    // A query still resolving the workspace graph means the hook registered and
    // the engine came up with it attached.
    let out = ws.run(&dist, &["query", "//pkg/..."]).expect("run");
    assert!(out.status.success(), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("//pkg:ok"),
        "{}",
        describe(&out)
    );
}

/// The checksum in the manifest is the supply-chain guard on a dylib that is
/// about to be mapped into the process with full privileges. It must reject,
/// loudly, before loading. Nothing about this path exists in a linked test —
/// there is no manifest and no artifact to verify.
#[test]
fn manifest_checksum_mismatch_is_rejected() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let dylib = dist.plugin("go");

    let manifest = ws.root().join("heph-go-plugin.json");
    let wrong = format!("sha256:{}", "0".repeat(64));
    write_manifest(&manifest, "go", &dylib, Some(&wrong)).expect("write manifest");

    ws.config(&format!(
        "{BASE_CONFIG}  - path: {}\n    options:\n      gotool: \"host\"\n",
        manifest.display()
    ))
    .expect("write config");

    let out = ws.run(&dist, &["inspect", "functions"]).expect("run");
    assert!(
        !out.status.success(),
        "loaded a cdylib whose checksum did not match: {}",
        describe(&out)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("checksum"),
        "rejection did not say why: {}",
        describe(&out)
    );
}
