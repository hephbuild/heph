// Shared integration-test helper module — mirrors `crates/plugingo-e2e/tests/common/mod.rs`'s
// shape for the js plugin. `allow`, not `expect`: which items are used varies per test binary.
#![allow(
    dead_code,
    unused_imports,
    reason = "shared test harness; each test binary uses a different subset"
)]

use anyhow::Context as _;
use heph::engine::event::{BuildEvent, BuildEventKind, EventSender};
use heph::engine::{Engine, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use hwalk::{CachedWalker, Ignore};
use plugin_js::pluginjs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

pub use htestkit::{Workspace, WorkspaceBuilder, artifact_bytes, artifact_paths, artifact_string};

pub fn testdata(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name)
}

pub fn fixture(name: &str) -> anyhow::Result<TempDir> {
    htestkit::copy_dir_to_tempdir(&testdata(name))
}

macro_rules! require_npm {
    () => {
        if !crate::common::npm_available() {
            crate::common::no_npm_or_panic();
            return Ok(());
        }
    };
}
pub(crate) use require_npm;

pub fn npm_available() -> bool {
    std::process::Command::new("npm")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Skipping is fine on a dev machine without `npm`; in CI it is a broken job.
/// Mirrors `plugingo-e2e/tests/common/mod.rs`'s `no_go_or_panic` exactly, for
/// the identical reason: a runner without `npm` on `PATH` turns the whole
/// suite green in a fraction of a second, indistinguishable from a suite
/// that actually ran real `vitest`/`tsc` and passed.
pub fn no_npm_or_panic() {
    assert!(
        std::env::var_os("CI").is_none(),
        "npm is not on PATH, so this test would silently skip. In CI that is a \
         broken job, not a skip: the devenv shell provides `pkgs.nodejs_24` (see \
         devenv.nix), so reaching this means the test is not running inside it."
    );
    eprintln!("skipping: npm not in PATH");
}

/// Run a real `npm install` against `dir` (the fixture's own copied tempdir,
/// never the checked-in `testdata/` source — `npm` writes `node_modules` and
/// a refreshed `package-lock.json` in place). Requires network: the same
/// tradeoff `plugingo-e2e`'s hermetic-SDK tests already accept for real
/// toolchain coverage, just via `npm`'s registry instead of `go.dev`'s.
///
/// `--no-audit --no-fund` only trims noise; nothing here is a hermeticity
/// claim — this is host-side test *setup*, not something heph's own graph
/// depends on. What `js_test`'s driver actually resolves against
/// (`<workspace_root>/node_modules/.bin/vitest`) is real output of this
/// exact call, so the whole point is that this is not faked.
pub fn npm_install(dir: &Path) -> anyhow::Result<()> {
    let out = std::process::Command::new("npm")
        .args(["install", "--no-audit", "--no-fund", "--loglevel=error"])
        .current_dir(dir)
        .output()
        .context("run npm install")?;
    anyhow::ensure!(
        out.status.success(),
        "npm install failed ({}):\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    Ok(())
}

/// A fixture copied into its own tempdir with a real `npm install` already
/// run against it. Call [`require_npm!`] before this in every test that uses
/// it, so a host without `npm` skips (or fails loudly in CI) instead of
/// hitting a confusing `npm install` error.
pub fn npm_fixture(name: &str) -> anyhow::Result<TempDir> {
    let dir = fixture(name)?;
    npm_install(dir.path())?;
    Ok(dir)
}

/// Build a workspace wired with the real js provider + every managed driver
/// it needs, against a directory that already has `node_modules` installed
/// (see [`npm_fixture`]). `pkgmanager: Npm` matches `npm_install` above —
/// swap both together if a pnpm-flavored fixture is ever added.
pub fn make_workspace(dir: TempDir) -> anyhow::Result<Workspace> {
    WorkspaceBuilder::from_dir(dir)
        .with_provider(|init| {
            Box::new(pluginjs::Provider::with_config(
                init.root.to_path_buf(),
                pluginjs::Config {
                    pkgmanager: pluginjs::PkgManager::Npm,
                    skip: Arc::new(Ignore::default()),
                    walker: Arc::new(CachedWalker::disabled()),
                    // Real transitive deps of vitest that declare an
                    // install/postinstall lifecycle script (esbuild fetches
                    // its native binary; fsevents, macOS-only, builds a
                    // native watcher) — heph refuses these by default (see
                    // `driver_install.rs` module docs' Hermeticity section),
                    // so a fixture pulling in real vitest needs them
                    // explicit, exactly as a real project's own config
                    // would. Not a fixed list: whatever `js_test`'s real
                    // dependency closure needs allow-listed goes here.
                    allow_scripts: vec!["esbuild".to_string(), "fsevents".to_string()],
                    tstool: "host".to_string(),
                    testrunner: "vitest".to_string(),
                    test_glob: vec![
                        "**/*.test.{ts,tsx,js,jsx}".to_string(),
                        "**/*.spec.{ts,tsx,js,jsx}".to_string(),
                    ],
                    // `js_lint`'s linter is set (or left to auto-detect) per
                    // package via `provider_state(provider = "js", linter =
                    // ...)`, not a provider-construction option — every test
                    // here that touches `js_lint` exercises real detection
                    // from the fixture's own config file (see
                    // `toolchain::detect_linter`), not an assumed default.
                    bundler: "esbuild".to_string(),
                },
            ))
        })
        .with_managed_driver(Box::new(pluginjs::JsPackageInfoDriver::new()))
        .with_managed_driver(Box::new(pluginjs::JsInstallDriver::new()))
        .with_managed_driver(Box::new(pluginjs::JsTestDriver::new()))
        .with_managed_driver(Box::new(pluginjs::JsTypecheckDriver::new()))
        .with_managed_driver(Box::new(pluginjs::JsLintDriver::new()))
        .with_managed_driver(Box::new(pluginjs::JsBundleDriver::new()))
        .build()
        .context("build pluginjs workspace")
}

/// Run `addr` against `ws.engine` directly (bypassing `Workspace::run`, which
/// wires no event channel) and return both the result and every `BuildEvent`
/// emitted along the way — the only way to observe a `LocalCacheHit`/
/// `ExecuteStart`, since `js_test` declares no output artifacts at all (its
/// `EResult` is empty on every run, hit or miss alike — see
/// `driver_test.rs`'s `parse_no_outputs_and_caches_locally_and_remotely`).
pub async fn run_with_events(
    ws: &Workspace,
    addr_str: &str,
) -> anyhow::Result<(anyhow::Result<Arc<heph::engine::EResult>>, Vec<BuildEvent>)> {
    let addr = parse_addr(addr_str)?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<BuildEvent>();
    let rs = ws.engine.new_state_with_events(true, Some(tx));
    let result = ws
        .engine
        .clone()
        .result_addr(
            rs.clone(),
            &addr,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await;
    let result = match result {
        Ok(r) => Ok(r),
        Err(err) => {
            let failures = rs.take_failures();
            if failures.is_empty() {
                Err(err)
            } else {
                let mut msg = String::new();
                for f in &failures {
                    msg.push_str(&format!("{}: {:#}", f.addr.format(), f.source));
                    if let Some(tail) = &f.log_tail {
                        msg.push_str(&format!("\nlast log lines:\n{}", tail.text));
                    }
                    msg.push('\n');
                }
                Err(anyhow::anyhow!("{}", msg.trim_end()))
            }
        }
    };
    // The sender is dropped once `rs` (and every clone the request took) goes
    // out of scope; `rs` itself is still held above, so drop it explicitly
    // first or `try_recv` below never sees the channel close. Draining with
    // `try_recv` (not `.recv().await`) is safe precisely because everything
    // that could still send has already finished by the time `result_addr`
    // returns.
    drop(rs);
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    Ok((result, events))
}

pub fn is_local_cache_hit(events: &[BuildEvent], addr: &str) -> bool {
    events
        .iter()
        .any(|e| matches!(&e.kind, BuildEventKind::LocalCacheHit { addr: a } if a == addr))
}

pub fn execute_started(events: &[BuildEvent], addr: &str) -> bool {
    events
        .iter()
        .any(|e| matches!(&e.kind, BuildEventKind::ExecuteStart { addr: a, .. } if a == addr))
}
