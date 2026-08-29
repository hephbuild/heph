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

/// **A driver inside a cdylib runs its target in the environment a runner
/// describes.**
///
/// This is the seam nothing else covers, and it shipped broken because of that.
/// A cdylib links its own copy of `execrunner`, whose host registry the engine
/// populated only in the *binary* — so a plugin driver under a runner failed
/// with "no runner host is installed in this component" no matter how correct
/// the engine side was. Every in-process test passed: `plugingo-e2e` links
/// `plugin-go` directly, and `exec_runner.rs` drives the builtin bash/exec
/// drivers. Neither crosses a real `dlopen`.
///
/// The proof does not need Go to succeed, or even to exist. `gotool = "host"`
/// resolves the toolchain by spawning a bare `go` **through the runner**, so a
/// `wrap` prefix that announces itself on stderr is evidence that a driver
/// living in the cdylib reached the host's registry across the ABI. Whatever
/// the Go build then does is beside the point.
#[test]
fn a_cdylib_driver_runs_its_target_through_the_hosts_runner() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let dylib = dist.plugin("go");
    assert!(dylib.is_file(), "missing {}", dylib.display());

    let manifest = ws.root().join("heph-go-plugin.json");
    let sum = sha256_file(&dylib).expect("hash go cdylib");
    write_manifest(&manifest, "go", &dylib, Some(&sum)).expect("write manifest");

    // The go plugin's `runner:` option is the only way a *generated* Go target
    // ever names a runner, so it is also the only way to reach this seam.
    ws.config(&format!(
        "{BASE_CONFIG}  - path: {}\n    options:\n      gotool: \"host\"\n      runner: \"//:runner\"\n",
        manifest.display()
    ))
    .expect("write config");

    // A `wrap` runner written by hand, which is the point of `runner.json` being
    // a plain artifact: no plugin is needed to produce one, so this test needs
    // neither devenv nor docker.
    //
    // The prefix is a program that deliberately does not exist. A `wrap` runner
    // makes its prefix head the program, so the driver's `go env GOROOT` probe
    // tries to exec *this* — and `hexecrunner::output` names what a runner
    // substituted, so the marker lands in the failure. Only reachable if the
    // rewrite crossed back from the host.
    //
    // No shell and no quoting on purpose: Starlark strips backslash escapes
    // inside a triple-quoted string, which silently turns an `exec "$@"` prefix
    // into malformed JSON.
    let runner_json = r#"{
  "version": 1,
  "fingerprint": "test:cdylib-seam",
  "runner": "wrap",
  "config": { "prefix": ["/heph-runner-reached-the-host"] }
}
"#;
    let (goos, goarch) = common::host_os_arch();
    // Both at the workspace root: `provider_state` applies to its own package and
    // descendants, and the Go package is `cmd/`, so a state in `pkg/` would not
    // reach it — the variant lookup walks *ancestors* within the module.
    ws.write(
        "BUILD",
        &format!(
            r#"target(
    name = "runner",
    driver = "textfile",
    text = """{runner_json}""",
    out = "runner.json",
)
provider_state(
    provider = "go",
    variants = {{"host": {{"goos": "{goos}", "goarch": "{goarch}"}}}},
)
"#
        ),
    )
    .expect("write BUILD");
    ws.write("go.mod", "module example.com/seam\n\ngo 1.21\n")
        .expect("write go.mod");
    ws.write("cmd/main.go", "package main\n\nfunc main() {}\n")
        .expect("write main.go");

    let out = ws.run(&dist, &["run", "//cmd:build@v=host"]).expect("run");
    let combined = describe(&out);

    // The failure this test exists for. Named separately so a regression reports
    // the cause rather than "the marker was missing".
    assert!(
        !combined.contains("no runner host is installed"),
        "the cdylib was not handed the host's runner registry — \
         `heph_plugin_set_runner_host` is the wiring that does it: {combined}"
    );
    // Only reachable if the rewrite crossed back: the driver in the cdylib built
    // a spec naming `go`, and what it actually tried to spawn was the host wrap
    // runner's prefix.
    assert!(
        combined.contains("/heph-runner-reached-the-host"),
        "a driver in the cdylib must spawn through the host's wrap runner: {combined}"
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
    // the engine came up with it attached. Subtree selection needs `-e`: a bare
    // positional argument is parsed strictly as an address.
    let out = ws.run(&dist, &["query", "-e", "//pkg/..."]).expect("run");
    assert!(out.status.success(), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("//pkg:ok"),
        "{}",
        describe(&out)
    );
}

/// The spawn-at-the-seam host mirror, across a REAL cdylib: the go provider's
/// `list` calls back into the engine executor (`states_under`, the module
/// variant universe) from a plugin-runtime worker. In-process tests
/// structurally cannot cover this — host and plugin share one tokio image
/// there, so the callback body always finds a runtime to run on. Here the
/// callback future is built by the host binary and polled by the plugin's own
/// workers: exactly the asymmetry that panics ("no reactor running") and then
/// aborts at the extern seam if the host side does not spawn the body onto the
/// engine runtime.
///
/// The fixture mirrors plugingo-e2e's `variant_sibling`: the `release` variant
/// is declared ONLY at `//cmd`, so it is absent from `//lib`'s ancestry and
/// `//lib:build_lib@v=release` can be listed only if the module universe came
/// back through the `states_under` callback — the assertion cannot pass
/// vacuously. `gotool: host` + query-only keeps the fixture offline: listing
/// runs no toolchain.
#[test]
fn shipped_go_cdylib_list_calls_back_states_under_across_the_seam() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let dylib = dist.plugin("go");
    assert!(dylib.is_file(), "missing {}", dylib.display());

    let manifest = ws.root().join("heph-go-plugin.json");
    let sum = sha256_file(&dylib).expect("hash go cdylib");
    write_manifest(&manifest, "go", &dylib, Some(&sum)).expect("write manifest");

    ws.config(&format!(
        "{BASE_CONFIG}  - path: {}\n    options:\n      gotool: \"host\"\n",
        manifest.display()
    ))
    .expect("write config");

    ws.write("go.mod", "module example.com/seam\n\ngo 1.21\n")
        .expect("write go.mod");
    ws.write(
        "lib/lib.go",
        "package lib\n\nfunc Greet() string { return \"hi\" }\n",
    )
    .expect("write lib.go");
    ws.write("cmd/main.go", "package main\n\nfunc main() {}\n")
        .expect("write main.go");
    ws.write(
        "cmd/BUILD",
        "provider_state(\n    provider = \"go\",\n    variants = {\"release\": {\"goos\": \"linux\", \"goarch\": \"amd64\"}},\n)\n",
    )
    .expect("write BUILD");

    let out = ws.run(&dist, &["query", "-e", "//lib/..."]).expect("run");
    assert!(out.status.success(), "{}", describe(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("//lib:build_lib@v=release"),
        "the sibling-declared variant must come back through the plugin's \
         states_under callback across the seam: {}",
        describe(&out)
    );
}

/// Regression test for the log-sink-before-`create` ordering bug: if the host
/// installs its log sink *after* calling the plugin's `create`, a
/// `tracing::error!` logged during construction failure has no subscriber to
/// go to and is silently dropped — right before the ABI seam turns the
/// failure into a non-unwinding abort with zero diagnostic output. Nothing
/// about this is observable in a linked test: it needs a real dlopen'd cdylib
/// whose own statically-linked `tracing` has no subscriber until the host
/// installs one.
///
/// The shipped go plugin already fails construction deterministically given a
/// malformed `walk_db` option (it decodes as a `PathBuf`; a YAML sequence
/// can't deserialize into one) — no purpose-built test hook needed.
#[test]
fn plugin_construction_failure_logs_before_the_abort() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let dylib = dist.plugin("go");
    assert!(dylib.is_file(), "missing {}", dylib.display());

    let manifest = ws.root().join("heph-go-plugin.json");
    let sum = sha256_file(&dylib).expect("hash go cdylib");
    write_manifest(&manifest, "go", &dylib, Some(&sum)).expect("write manifest");

    ws.config(&format!(
        "{BASE_CONFIG}  - path: {}\n    options:\n      walk_db: [1, 2, 3]\n",
        manifest.display()
    ))
    .expect("write config");

    let out = ws.run(&dist, &["inspect", "functions"]).expect("run");
    assert!(
        !out.status.success(),
        "plugin construction should have failed and aborted: {}",
        describe(&out)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("plugin construction failed") && stderr.contains("walk_db"),
        "construction-failure log was dropped — the log sink must be installed \
         before `create` is called: {}",
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

/// The oci cdylib, exercised through `inspect def` — which makes the host call
/// the plugin's `parse` across the seam.
///
/// `parse` is where `docker_build` asks buildx for its default platform, i.e. where
/// it shells out. A cdylib's statically-linked tokio is a *different instance*
/// from the host's, and the future the host polls carries none of it, so a
/// reactor touch there panics — and a panic across the ABI seam is a
/// non-unwinding abort that kills `heph` outright, with a backtrace instead of
/// an error. That is invisible to every in-process test: they run on the host's
/// runtime, where the reactor is right there.
///
/// So the assertion is not "the target parses" — without docker it cannot, and
/// that is fine. It is that the process came back at all, with a diagnosable
/// error rather than a SIGABRT.
#[test]
fn shipped_oci_cdylib_parses_across_the_abi_without_aborting() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let dylib = dist.plugin("oci");
    assert!(dylib.is_file(), "missing {}", dylib.display());

    let manifest = ws.root().join("heph-oci-plugin.json");
    let sum = sha256_file(&dylib).expect("hash oci cdylib");
    write_manifest(&manifest, "oci", &dylib, Some(&sum)).expect("write manifest");
    ws.config(&format!("{BASE_CONFIG}  - path: {}\n", manifest.display()))
        .expect("write config");

    // `platforms` is left unset on purpose: that is the branch that probes the
    // builder, and therefore the branch that shells out from `parse`.
    ws.write(
        "pkg/BUILD",
        "target(name = \"df\", driver = \"bash\", run = \"echo 'FROM scratch' > $OUT\", \
         out = \"Dockerfile\")\n\
         target(name = \"img\", driver = \"docker_build\", context = [\":df\"])\n",
    )
    .expect("write BUILD");

    let out = ws
        .run(&dist, &["inspect", "def", "//pkg:img"])
        .expect("run");

    // A non-unwinding abort leaves no exit code (killed by SIGABRT) and prints
    // the panic banner. Either is the failure this test exists to catch.
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.code().is_some(),
        "heph was killed by a signal — the plugin panicked across the ABI seam: {}",
        describe(&out)
    );
    assert!(
        !combined.contains("panic in a function that cannot unwind")
            && !combined.contains("there is no reactor running"),
        "plugin `parse` touched the reactor on the host's poll: {}",
        describe(&out)
    );

    // With a builder present the def resolves; without one the probe fails and
    // heph says so. Both are a working seam — an abort is not.
    assert!(
        out.status.success() || combined.contains("docker"),
        "expected a def or a docker-shaped failure, got: {}",
        describe(&out)
    );
}

/// The devenv cdylib loads across the ABI and its driver is reachable.
///
/// Every other shipped cdylib has a load test here; without this one the devenv
/// plugin would be the one artifact heph publishes whose `dlopen` nothing
/// proves — and an ABI mismatch in a cdylib is an abort at load, not an error a
/// user can read.
///
/// Deliberately does not run a `devenv_runner` target: that needs `devenv` and
/// a nix evaluation, which is what `crates/e2e/tests/devenv_runner.rs` covers
/// behind an opt-in. What is asserted here is what only the shipped artifact
/// can answer — that the library maps, the create entry's type report matches,
/// and the driver it exports is registered under the name a BUILD file uses.
#[test]
fn shipped_devenv_cdylib_loads_and_registers_its_driver() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let dylib = dist.plugin("devenv");
    assert!(dylib.is_file(), "missing {}", dylib.display());

    let manifest = ws.root().join("heph-devenv-plugin.json");
    let sum = sha256_file(&dylib).expect("hash devenv cdylib");
    write_manifest(&manifest, "devenv", &dylib, Some(&sum)).expect("write manifest");
    ws.config(&format!("{BASE_CONFIG}  - path: {}\n", manifest.display()))
        .expect("write config");

    ws.write(
        "pkg/BUILD",
        "target(name = \"runner\", driver = \"devenv_runner\", mode = \"wrap\")\n",
    )
    .expect("write BUILD");

    // `inspect def` parses the target without executing it, so this reaches the
    // driver across the seam and stops short of running devenv.
    let out = ws
        .run(&dist, &["inspect", "def", "//pkg:runner"])
        .expect("run");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.code().is_some(),
        "heph was killed by a signal — the plugin panicked across the ABI seam: {}",
        describe(&out)
    );
    assert!(
        !combined.contains("driver not found"),
        "the devenv plugin loaded but did not register `devenv_runner`: {}",
        describe(&out)
    );
    assert!(
        out.status.success(),
        "parsing a devenv_runner target should succeed: {}",
        describe(&out)
    );
}
