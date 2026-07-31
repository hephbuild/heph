//! Host TypeScript toolchain resolution for `js_typecheck` — the disclosed,
//! non-hermetic escape hatch this milestone ships with. Mirrors the Go
//! plugin's `gotool` axis (`crates/plugin-go/src/plugingo/toolchain.rs`) in
//! spirit — an explicit, provider-level, checked option rather than an
//! unconditional assumption baked into the driver — but far narrower: no
//! hermetic Node/TypeScript toolchain exists anywhere in this plugin yet
//! (M1's `js_install` fetches individual npm packages one at a time, not a
//! pinned Node/TypeScript SDK), so there is exactly one working value today.
//!
//! Selected via the `js` provider's `tstool` option. `tstool` still exists as
//! a named, checked option — not an assumption silently baked into the
//! driver — so a caller who sets it to anything else gets a clear, named
//! error instead of `tsc` silently being assumed reachable. TODO M4+: a
//! pinned hermetic Node/TypeScript download (mirroring `plugin-go`'s
//! `toolchain.rs`) turns this into a real multi-way `Toolchain` enum the way
//! Go's `gotool` already is (`host` / a pinned version / a target ref).
//!
//! ## Resolution order ([`resolve_host_tsc`])
//!
//! 1. `<workspace_root>/node_modules/.bin/tsc` — the conventional location a
//!    real (host-run, outside heph) `npm install`/`pnpm install` leaves the
//!    `typescript` package's binary. Checked first since it's the more
//!    specific, workspace-pinned answer when present.
//! 2. The process's own `PATH` — a globally-installed `tsc`.
//!
//! **Known gap, TODO M4+**: this does *not* resolve a `tsc` binary from a
//! package heph itself fetched via `js_install`. `Provider::get` runs at
//! spec-resolution time, strictly before any target (including `js_install`)
//! ever executes — see `importgraph.rs` module docs' "Hermeticity" section —
//! so no heph-materialized `node_modules` tree exists yet at the point this
//! resolution runs. Wiring that in needs `js_typecheck` to depend on a
//! reconstructed per-target `node_modules` (`ai-docs/js-plugin-plan.md`'s
//! "Per-target `node_modules` reconstruction", not yet built for any driver
//! in this crate) — out of scope for M3.
//!
//! ## Why the version is queried at `Provider::get` time, not `run()` time
//!
//! Every existing toolchain-resolution precedent in this codebase
//! (`plugin-go`'s `resolve_toolchain_go`) defers the host query to `run()`,
//! because Go's `gotool` is always a *pinned version string* — known without
//! invoking anything. `tstool = "host"` has no pinned-version equivalent: the
//! only way to know which `tsc` a host actually has is to run it. Since the
//! driver's cache-key hash (`JsTypecheckDef::hash`) is computed in `parse()`,
//! strictly before `run()` — and a cache hit skips `run()` entirely — a
//! version bump would never bust the cache if the query were deferred to
//! `run()`. So `Provider::get` queries `tsc --version` once, synchronously,
//! and threads the resulting string through the target's config for the
//! driver to hash — see `driver_typecheck.rs`'s module docs.

use anyhow::Context;
use std::path::{Path, PathBuf};

/// Sentinel `tstool` value selecting the (only, for now) host toolchain.
/// Mirrors `plugin-go::toolchain::HOST`.
pub const HOST: &str = "host";

/// Whether `spec` selects the host toolchain — the only supported value in
/// this milestone. See module docs.
pub fn is_host(spec: &str) -> bool {
    spec == HOST
}

/// `<workspace_root>/node_modules/.bin/<bin_name>` first, then the process's
/// own `PATH` — the shared resolution order behind both [`resolve_host_tsc`]
/// and `js_test`'s `testrunner` binary resolution (`resolve_host_test_runner`
/// in this module). `None` when neither has it; each caller wraps that into
/// its own error naming both places checked, per this milestone's explicit
/// "do not silently assume the binary is on PATH" requirement.
fn find_host_bin(workspace_root: &Path, bin_name: &str) -> Option<PathBuf> {
    let local = workspace_root
        .join("node_modules")
        .join(".bin")
        .join(bin_name);
    if std::fs::metadata(&local)
        .map(|m| m.is_file())
        .unwrap_or(false)
    {
        return Some(local);
    }

    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let cand = dir.join(bin_name);
            if std::fs::metadata(&cand)
                .map(|m| m.is_file())
                .unwrap_or(false)
            {
                return Some(cand);
            }
        }
    }
    None
}

/// Resolve the `tsc` binary to run: `<workspace_root>/node_modules/.bin/tsc`
/// first, then the process's own `PATH`. Fails loudly, naming both places
/// checked, rather than silently assuming `tsc` is reachable — per this
/// milestone's explicit "do not silently assume tsc is on PATH" requirement.
pub(crate) fn resolve_host_tsc(workspace_root: &Path) -> anyhow::Result<PathBuf> {
    let local = workspace_root.join("node_modules").join(".bin").join("tsc");
    find_host_bin(workspace_root, "tsc").ok_or_else(|| {
        anyhow::anyhow!(
            "js_typecheck: no `tsc` binary found at {local:?} or on PATH — install TypeScript \
             (`npm install -D typescript` / `pnpm add -D typescript`) so `node_modules/.bin/tsc` \
             exists, or make a `tsc` binary available on PATH. There is no hermetic TypeScript \
             toolchain yet in this plugin — `tstool=\"host\"` (the only supported value in this \
             milestone) requires one of those two to be reachable."
        )
    })
}

/// Sentinel `testrunner` values `js_test` accepts — see this module's
/// `resolve_host_test_runner` doc and `driver_test.rs` module docs for the
/// same disclosed non-hermetic-toolchain shape `tstool = "host"` already has.
pub const VITEST: &str = "vitest";
pub const JEST: &str = "jest";

/// Whether `testrunner` names a supported test runner — the only two values
/// `js_test` recognizes in this milestone (see `ai-docs/js-plugin-plan.md`'s
/// `js_test` row: `testrunner` defaults to `vitest`, alt `jest`).
pub fn is_supported_testrunner(testrunner: &str) -> bool {
    testrunner == VITEST || testrunner == JEST
}

/// Resolve the configured `testrunner`'s binary:
/// `<workspace_root>/node_modules/.bin/<vitest|jest>` first, then the
/// process's own `PATH` — the same disclosed non-hermetic escape hatch
/// `tstool = "host"` already has (see [`resolve_host_tsc`]), extended to
/// `js_test`'s toolchain axis. Fails loudly, naming both places checked, when
/// neither has it.
pub(crate) fn resolve_host_test_runner(
    workspace_root: &Path,
    testrunner: &str,
) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        is_supported_testrunner(testrunner),
        "js_test: unsupported testrunner {testrunner:?} — expected \"vitest\" or \"jest\""
    );
    let local = workspace_root
        .join("node_modules")
        .join(".bin")
        .join(testrunner);
    find_host_bin(workspace_root, testrunner).ok_or_else(|| {
        anyhow::anyhow!(
            "js_test: no `{testrunner}` binary found at {local:?} or on PATH — install it \
             (`npm install -D {testrunner}` / `pnpm add -D {testrunner}`) so \
             `node_modules/.bin/{testrunner}` exists, or make a `{testrunner}` binary available \
             on PATH. There is no hermetic {testrunner} toolchain yet in this plugin — \
             `testrunner=\"{testrunner}\"` requires one of those two to be reachable."
        )
    })
}

/// Query the resolved test runner's own `--version` output, trimmed —
/// hashed into `JsTestDef` so a host runner upgrade/downgrade busts the
/// cache, mirroring [`query_tsc_version`]'s "query once at `Provider::get`
/// time, not `run()` time" rationale (see that function's doc): the
/// driver's cache-key hash is computed in `parse()`, strictly before `run()`,
/// so a version bump queried only inside `run()` would never bust a cache
/// hit.
pub(crate) fn query_test_runner_version(runner_bin: &Path) -> anyhow::Result<String> {
    let out = std::process::Command::new(runner_bin)
        .arg("--version")
        .output()
        .with_context(|| format!("run {runner_bin:?} --version"))?;
    anyhow::ensure!(
        out.status.success(),
        "`{runner_bin:?} --version` failed ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let version = String::from_utf8(out.stdout)
        .with_context(|| format!("{runner_bin:?} --version output is not utf8"))?
        .trim()
        .to_string();
    anyhow::ensure!(
        !version.is_empty(),
        "`{runner_bin:?} --version` returned empty output"
    );
    Ok(version)
}

/// Query `tsc --version`'s full trimmed output (e.g. `"Version 5.6.2"`) —
/// hashed into `JsTypecheckDef` so a host tsc upgrade/downgrade busts the
/// cache. See module docs for why this runs at `Provider::get` time.
pub(crate) fn query_tsc_version(tsc_bin: &Path) -> anyhow::Result<String> {
    let out = std::process::Command::new(tsc_bin)
        .arg("--version")
        .output()
        .with_context(|| format!("run {tsc_bin:?} --version"))?;
    anyhow::ensure!(
        out.status.success(),
        "`{tsc_bin:?} --version` failed ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let version = String::from_utf8(out.stdout)
        .with_context(|| format!("{tsc_bin:?} --version output is not utf8"))?
        .trim()
        .to_string();
    anyhow::ensure!(
        !version.is_empty(),
        "`{tsc_bin:?} --version` returned empty output"
    );
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_host_recognizes_only_the_host_sentinel() {
        assert!(is_host(HOST));
        assert!(is_host("host"));
        assert!(!is_host("1.2.3"));
        assert!(!is_host("//pkg:tsc"));
    }

    #[test]
    fn resolve_host_tsc_prefers_local_node_modules_bin_over_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let local_bin_dir = dir.path().join("node_modules").join(".bin");
        std::fs::create_dir_all(&local_bin_dir).expect("mkdir");
        let local_tsc = local_bin_dir.join("tsc");
        std::fs::write(&local_tsc, b"#!/bin/sh\necho local\n").expect("write local tsc");

        let found = resolve_host_tsc(dir.path()).expect("resolve");
        assert_eq!(found, local_tsc);
    }

    #[test]
    fn resolve_host_tsc_errors_clearly_when_absent_everywhere() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Deliberately empty PATH so a real host tsc (if any) can't be found
        // by accident, keeping this assertion meaningful across machines.
        let prior = std::env::var_os("PATH");
        // SAFETY: test-only, single-threaded within this process for the
        // duration of the mutation; restored immediately below.
        unsafe { std::env::set_var("PATH", "") };
        let result = resolve_host_tsc(dir.path());
        match &prior {
            // SAFETY: test-only, restoring the prior value we saved above.
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            // SAFETY: test-only, restoring the prior (unset) state.
            None => unsafe { std::env::remove_var("PATH") },
        }
        let err = result.expect_err("no tsc anywhere must error, not silently succeed");
        let msg = format!("{err:#}");
        assert!(msg.contains("tsc"), "{msg}");
        assert!(msg.contains("PATH"), "{msg}");
    }

    #[test]
    fn query_tsc_version_reads_trimmed_stdout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake_tsc = dir.path().join("tsc");
        std::fs::write(&fake_tsc, "#!/bin/sh\necho 'Version 5.6.2'\n").expect("write fake tsc");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake_tsc)
                .expect("metadata")
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake_tsc, perms).expect("chmod");
        }
        let version = query_tsc_version(&fake_tsc).expect("query version");
        assert_eq!(version, "Version 5.6.2");
    }

    #[test]
    fn query_tsc_version_errors_on_nonzero_exit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake_tsc = dir.path().join("tsc");
        std::fs::write(&fake_tsc, "#!/bin/sh\nexit 3\n").expect("write fake tsc");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake_tsc)
                .expect("metadata")
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake_tsc, perms).expect("chmod");
        }
        let err = query_tsc_version(&fake_tsc).expect_err("nonzero exit must error");
        assert!(format!("{err:#}").contains("failed"));
    }

    // ---- testrunner (js_test) ----

    #[test]
    fn is_supported_testrunner_recognizes_only_vitest_and_jest() {
        assert!(is_supported_testrunner(VITEST));
        assert!(is_supported_testrunner(JEST));
        assert!(!is_supported_testrunner("mocha"));
        assert!(!is_supported_testrunner(""));
    }

    #[test]
    fn resolve_host_test_runner_rejects_unsupported_value() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = resolve_host_test_runner(dir.path(), "mocha")
            .expect_err("unsupported testrunner must error");
        assert!(format!("{err:#}").contains("mocha"));
    }

    #[test]
    fn resolve_host_test_runner_prefers_local_node_modules_bin_over_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let local_bin_dir = dir.path().join("node_modules").join(".bin");
        std::fs::create_dir_all(&local_bin_dir).expect("mkdir");
        let local_vitest = local_bin_dir.join(VITEST);
        std::fs::write(&local_vitest, b"#!/bin/sh\necho local\n").expect("write local vitest");

        let found = resolve_host_test_runner(dir.path(), VITEST).expect("resolve");
        assert_eq!(found, local_vitest);
    }

    #[test]
    fn resolve_host_test_runner_errors_clearly_when_absent_everywhere() {
        let dir = tempfile::tempdir().expect("tempdir");
        let prior = std::env::var_os("PATH");
        // SAFETY: test-only, single-threaded within this process for the
        // duration of the mutation; restored immediately below.
        unsafe { std::env::set_var("PATH", "") };
        let result = resolve_host_test_runner(dir.path(), JEST);
        match &prior {
            // SAFETY: test-only, restoring the prior value we saved above.
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            // SAFETY: test-only, restoring the prior (unset) state.
            None => unsafe { std::env::remove_var("PATH") },
        }
        let err = result.expect_err("no jest anywhere must error, not silently succeed");
        let msg = format!("{err:#}");
        assert!(msg.contains("jest"), "{msg}");
        assert!(msg.contains("PATH"), "{msg}");
    }

    #[test]
    fn query_test_runner_version_reads_trimmed_stdout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake_bin = dir.path().join("vitest");
        std::fs::write(&fake_bin, "#!/bin/sh\necho 'vitest/1.6.0'\n").expect("write fake runner");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake_bin)
                .expect("metadata")
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake_bin, perms).expect("chmod");
        }
        let version = query_test_runner_version(&fake_bin).expect("query version");
        assert_eq!(version, "vitest/1.6.0");
    }

    #[test]
    fn query_test_runner_version_errors_on_nonzero_exit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake_bin = dir.path().join("jest");
        std::fs::write(&fake_bin, "#!/bin/sh\nexit 3\n").expect("write fake runner");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake_bin)
                .expect("metadata")
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake_bin, perms).expect("chmod");
        }
        let err = query_test_runner_version(&fake_bin).expect_err("nonzero exit must error");
        assert!(format!("{err:#}").contains("failed"));
    }
}
