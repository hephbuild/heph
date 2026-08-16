//! `js_test` — runs the configured test runner (`vitest` default, `jest` alt)
//! against **one test file at a time**.
//!
//! Per-test-file granularity is the stated differentiator this milestone
//! exists to prove (`ai-docs/js-plugin-plan.md`'s "Caching / incrementality"
//! section): "per-test-file targets using heph's own import graph — this is
//! where an owned resolver beats every incumbent build system (Turborepo/Nx
//! never get finer than package granularity for tasks)." One `js_test`
//! target address exists per matched test file, distinguished by a `file`
//! addr arg (see `provider.rs`'s `Provider::list`/`Provider::get` handling of
//! [`crate::pluginjs::TEST_TARGET`]) — never a driver-per-tool name (the
//! design doc's naming rule: one `js_test`, tool selected by the `testrunner`
//! config option, never `js_test_vitest`/`js_test_jest`).
//!
//! ## Toolchain: `testrunner = "vitest"` (default) or `"jest"`, a disclosed
//! ## non-hermetic escape hatch
//!
//! Exactly the same shape as `js_typecheck`'s `tstool = "host"` (see
//! `driver_typecheck.rs`/`toolchain.rs` module docs): no hermetic Node/test-
//! runner toolchain exists anywhere in this plugin yet, so `Provider::get`
//! (`provider.rs::test_config`) resolves the configured runner's binary from
//! the host (`toolchain::resolve_host_test_runner`:
//! `<workspace_root>/node_modules/.bin/<vitest|jest>`, then `PATH`) and
//! queries its `--version` once, threading both through this target's config
//! for the driver to hash. Real, disclosed, not hidden — same class of gap as
//! `driver_install.rs`'s `.npmrc`-auth gap.
//!
//! ## Inputs / cache key
//!
//! The test file itself, plus its full runtime-transitive closure within its
//! owning package (`importgraph::build_test_closure` over
//! `ImportGraph::runtime_edges` — see that function's doc for exactly what
//! counts as "in the closure" vs. "one-hop external"), plus every third-party
//! dependency that closure's own unresolved bare specifiers name (resolved
//! via `deps::resolve_one_dependency`, the lockfile-driven mechanism — never
//! by walking `oxc_resolver` paths against an ambient `node_modules`, see
//! this crate's M3-review lesson recorded in `provider.rs`), plus the
//! resolved test-runner config file, if any (`vitest.config.ts` /
//! `jest.config.js` / `vite.config.ts`'s own `test: {...}` key / a jest
//! project's `package.json` `"jest"` field, walked up the ancestor chain the
//! same way a tsconfig is — see `importgraph::find_nearest_test_runner_config`
//! / `importgraph::find_nearest_jest_package_json_config`), plus every
//! additional file that config's own content names or imports, recursively
//! (`importgraph::resolve_runner_config_referenced_files`), plus the queried
//! runner version, plus the resolved tsconfig (the package's own, or the
//! nearest scoped ancestor's) and its whole `extends` chain, if any —
//! mirroring `js_typecheck`'s identical `"tsconfig"` group (fixed M4,
//! confirmed live: without it staged, a real `tsconfig-paths`-style Vite
//! plugin has no `tsconfig.json` to read at runtime, so a `paths` alias
//! like `@/*` fails to resolve even though the aliased file itself is
//! correctly staged via the closure above). See `provider.rs`'s
//! `test_deps_config` for the pure, runner-free Input-scoping function this
//! driver's `deps`/`test_file` config comes from — deliberately split out
//! the same way `typecheck_deps_config` is, so the per-test-file scoping is
//! unit-testable without a real `vitest`/`jest` binary (this is the task's
//! "single most important test in this milestone").
//!
//! Beyond the declared `Input`s (content-hashed automatically by the
//! engine), [`JsTestDef::hash`] additionally hashes `test_file` itself (so
//! *which* test file this target is for is part of the key, not just its
//! transitive content — two different test files with byte-identical
//! closures must still be different cache entries), `runner_config_content`
//! and `tsconfig_content` directly (same deliberate redundancy with their
//! declared Inputs that `js_typecheck`'s `tsconfig_content` has — see that
//! struct's doc), and `runner_version`. `runner_bin` (an absolute host
//! path) is deliberately **excluded**, mirroring `tsc_bin`'s exclusion —
//! see that field's doc in `driver_typecheck.rs` for the identical
//! cache-portability rationale.
//!
//! **Known scope trim, disclosed rather than silent**: `build_test_closure`
//! only recurses within the *owning package's* own import graph — a file
//! reached one hop into a workspace-sibling package (or, absent
//! `node_modules`, a third-party package) is declared as an `Input` but its
//! own further imports are not followed (`importgraph.rs`'s `TestClosure`
//! doc calls this "one-hop external", the identical trim
//! `js_typecheck`'s type-edge handling already accepts). TODO M4+: recurse
//! into a sibling package's own import graph once cross-package graph
//! construction exists.
//!
//! **Known scope trim, disclosed rather than silent**: a resolved runner
//! config's own `moduleNameMapper` (jest) / `resolve.alias` (vitest)
//! mock-path values, and a custom `testEnvironment` module, are not
//! extracted into the declared Input set (unlike `setupFiles`/
//! `setupFilesAfterEnv`/`globalSetup`/`globalTeardown`, and a relative
//! `import`/`require` of a shared base config — both of which *are* now
//! resolved and declared, recursively; see
//! `importgraph::resolve_runner_config_referenced_files`). A test reaching a
//! first-party file only through a runner-specific alias that isn't also a
//! tsconfig `paths` entry has that file silently missing from the declared
//! Input set — editing it produces a stale cache hit. TODO M4+.
//!
//! ## Invocation shape
//!
//! Both runners are invoked from the target's own package directory (`cwd =
//! sandbox_pkg_dir`), with the test file and (if any) runner config *also*
//! passed as absolute in-sandbox paths — mirroring `js_typecheck`'s
//! `--project <tsconfig_abs>` shape, so nothing about *finding* those two
//! files depends on `cwd` at all. `cwd` still matters for a third class of
//! file this driver doesn't control: a plugin loaded by the runner config
//! that does its *own* ambient, `process.cwd()`-relative config discovery —
//! confirmed live, `@lingui/vite-plugin` calls `@lingui/conf`'s
//! `getConfig()`, which (absent an explicit `configPath`) searches upward
//! from `process.cwd()` for `lingui.config.*`; that search only ever walks
//! *ancestor* directories, so with the previous `cwd = sandbox_ws_dir`
//! choice it could never find a config living in the package's own
//! directory (a *descendant* of the workspace root, never an ancestor) no
//! matter how correctly this driver staged it. `sandbox_pkg_dir` is also
//! the cwd a real, non-heph `vitest`/`jest` invocation actually runs
//! with in practice (`cd package && vitest`, or `pnpm --filter pkg test`) —
//! matching that convention is what makes ambient discovery work at all.
//! `vitest` gets `run <file>` (the non-watch, CI-friendly
//! subcommand — bare `vitest` defaults to interactive watch mode, which would
//! hang forever under heph); `jest` gets `--runTestsByPath <file>` (an exact
//! path match, not a `testPathPattern` regex, so the invocation can't
//! accidentally pick up more than the one intended file). `PATH` is passed
//! through to the child process — the same disclosed, minimal passthrough
//! `js_typecheck`'s `run()` uses for its own `node`-shebang binaries, and for
//! the identical reason (a `#!/usr/bin/env node`-shebang `vitest`/`jest`
//! binary needs `node` reachable via `PATH`). `TZ=UTC` and `LANG=C.UTF-8`
//! are additionally pinned (constants, not read from the host) rather than
//! left ambient: absent both, Node/ICU falls back to the host's own
//! timezone/locale, so a `Date`/`Intl`-sensitive test could otherwise pass on
//! one machine and fail on another with a byte-identical cache key. `CI=1` is
//! also set: this invocation is headless by construction (no TTY, no human
//! to answer a prompt), which is exactly what `CI` conventionally signals to
//! a well-behaved CLI tool, and vitest/jest both gate interactive behavior
//! beyond just watch-mode selection on `isCI`/`isTTY` checks.
//!
//! ## Failure reporting
//!
//! A failing test is a plain, non-zero-exit driver failure with both stdout
//! and stderr tails surfaced (`test_failure_detail`, identical shape to
//! `driver_typecheck.rs`'s `tsc_failure_detail`) — the runner's actual
//! output (assertion diffs, stack traces) must reach the user, never be
//! swallowed into a bare "failed" with no detail. This is the fallback path
//! only — see the next section for the primary one.
//!
//! ## Detecting real completion, not waiting for exit
//!
//! vitest (and jest) are known to sometimes not exit after finishing their
//! own work — a failed dependency-optimizer scan leaving a background
//! esbuild service alive is a confirmed live trigger — and nothing about a
//! plain subprocess wait bounds how long that takes to notice. Waiting on
//! *exit* was always the wrong signal: the runner's own results are final
//! the moment it finishes writing them, regardless of whether the process
//! then goes on to hang. So both runners are asked for a second, structured
//! report alongside their normal human-readable one — vitest gets
//! `--reporter=default --reporter=json --outputFile=<path>`, jest gets
//! `--json --outputFile=<path>` — written to [`RESULT_FILE_NAME`] inside the
//! target's own sandbox package dir, deliberately never a declared `Input`
//! or `Output` (see `_golist`'s `.heph-gocache` for the identical pattern:
//! `driver_golist.rs`'s use of `GOCACHE`). `exec_runner` polls for that file
//! concurrently with draining the child's stdout/stderr, and the instant it
//! appears and parses, treats the run as **over** — the verdict comes from
//! its `success` field, not from the process's eventual exit code — and
//! forces the child to exit right then via the same manually-triggerable
//! [`hcore::hasync::StdCancellationToken`] mechanism a real user Ctrl-C
//! already goes through (SIGINT → grace → SIGKILL, never a bare
//! `tokio::time::timeout` around the subprocess future, which would just
//! drop the future and leak the orphaned child). A well-behaved runner that
//! simply exits on its own is unaffected — the file is checked one last time
//! right after end-of-stream, so the richer JSON-derived detail is used
//! whenever it exists, whichever path got there first.
//!
//! [`RUNNER_TIMEOUT`] (20 minutes) still exists, but only as the last-resort
//! backstop for a runner that produces *neither* a usable exit, *nor* a
//! parseable result file, *nor* the specific known-unrecoverable condition
//! [`DEPENDENCY_SCAN_HANG_MARKER`] detects — a genuinely wedged process with
//! no signal to key off at all. It is not, any more, the mechanism that
//! makes a completed (or confirmed-unrecoverable) run end promptly; that is
//! the JSON-file detection above and the marker detection below.
//!
//! ## Surfacing output live, not just on completion
//!
//! A wedged runner producing none of the signals above used to also be
//! invisible *while* it was happening: `exec_runner` only ever reported
//! captured stdout/stderr once it returned, so a run stuck in the
//! [`RUNNER_TIMEOUT`] backstop showed nothing at all for up to 20 minutes —
//! the runner's own diagnostic (e.g. a dependency-scan resolution error,
//! printed the instant it happens) was already sitting in memory, just not
//! shown to anyone, forcing `heph run --shell` to reproduce by hand what had
//! already run. `exec_runner` now tees every chunk, as it arrives, to two
//! places alongside the existing in-memory accumulation: the engine's own
//! live terminal sink (`ManagedRunRequest`'s `stdout`/`stderr` — handed to
//! the single named target of an interactive run regardless of `--shell`,
//! see `run()`'s doc comment) when one is wired, and a `log.txt` file in the
//! sandbox root always, under the same `log.txt` filename `pluginexec` and
//! `plugin-oci`'s `docker_build.rs` use for their own equivalent file (this
//! driver does not yet wire it into `crates/engine`'s
//! `extract_log_tail`/`ProcessFailed` framed-box renderer the way those two
//! do — see [`RUNNER_LOG_FILE_NAME`]'s doc for what would still need to
//! change for that). This makes the output visible the instant the runner
//! prints it, independent of whether — or when — the process itself ever
//! finishes.
//!
//! ## Ending a specific, confirmed-unrecoverable hang without a timeout
//!
//! Visibility alone does not end the run: a runner that never writes
//! [`RESULT_FILE_NAME`] and never exits was still only actually ended by
//! [`RUNNER_TIMEOUT`], and 20 minutes is far too long to sit
//! failed-but-not-yet-reported. The single confirmed live trigger for this
//! (a failed Vite dependency-optimizer scan) is not a "still working, just
//! slow" case — it is a confirmed, unfixed upstream bug: the scan's error
//! path returns without ever resolving the internal promise every
//! subsequent module load is awaiting, so the process is not merely quiet,
//! it is provably never going to produce a result (vitejs/vite#22934,
//! vitest-dev/vitest#9799). Because Vite always prints the same
//! [`DEPENDENCY_SCAN_HANG_MARKER`] line the instant this happens, seeing it
//! is as reliable a completion signal as [`RESULT_FILE_NAME`] itself — the
//! same manually-triggered [`hcore::hasync::StdCancellationToken`] mechanism
//! (SIGINT → grace → SIGKILL) fires the moment it appears on either stream.
//!
//! This is deliberately narrower than a generic idle/stall heuristic (e.g.
//! "no output for N seconds"), which this module's own per-test-file
//! granularity makes an easy trap: a single test file legitimately produces
//! long stretches of silence (most reporters only print at the very end),
//! so time-since-last-byte is not a trustworthy signal here the way it might
//! be for a whole-suite run. Matching the *specific, versions-stable text*
//! of a *specific, confirmed-terminal* upstream bug avoids that false-positive
//! risk entirely — at the cost of only covering the one trigger this module
//! has actually seen live. A different wedge with no recognizable signal is
//! still only caught by [`RUNNER_TIMEOUT`].

use anyhow::Context;
use async_trait::async_trait;
use hcore::debug_hash::DebugHasher;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hplugin::driver::targetdef::{CacheConfig, Input, InputMode, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, TargetAddr,
};
use hplugin::htspec::Spec;
use hproc::proc_exec;
use std::collections::HashMap;
use std::ffi::OsString;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

use crate::pluginjs::toolchain;

/// Dep groups staged read-only (hardlinked into place — see
/// `hdriver_support::stage`'s `STAGE_PER_FILE_ANNOTATION`) rather than
/// copied into the sandbox. Mirrors plugin-go's own read-only annotation on
/// its SDK/toolchain dep group (`driver_compile.rs`), never on the
/// package's own sources: none of these three is ever written into by a
/// running test. `"external"` is resolved third-party/workspace-sibling
/// code — npm packages don't get mutated at test time.
/// `"runner_config"`/`"tsconfig"` are config files a test process only ever
/// reads (js_test never shells out to `tsc`, so no
/// `--incremental`/`"composite"` `.tsbuildinfo` write into the tsconfig's
/// own directory can happen here — that's `js_typecheck`'s target, not this
/// one).
///
/// **Per-file, not a whole-directory symlink**: a first attempt at this
/// used a plain directory symlink (`READ_ONLY_ANNOTATION` alone). That
/// broke real `js_test` runs — Node's own module resolution calls
/// `fs.realpath()` on a required file before walking up for `node_modules`,
/// and a symlinked ancestor directory resolves to the *stage's* isolated
/// path, which has no matching `node_modules` chain (or any of its
/// siblings) alongside it. `vitest.config.ts`'s own plugin imports
/// (`@vitejs/plugin-react`, etc, all genuinely present in the sandbox's
/// node_modules) failed to resolve for exactly this reason. `STAGE_PER_FILE_ANNOTATION`
/// (`link_tree`/`link_one` in `stage.rs`) hardlinks every file directly into
/// its real sandbox path instead — no symlinked ancestor anywhere in the
/// destination tree, so `realpath()` returns the sandbox path itself and
/// Node's resolution walk sees the real, complete `node_modules` layout.
/// Slightly more syscalls than a directory symlink (O(files) hardlinks vs.
/// O(depth) symlinks), but still avoids the full byte-copy this whole
/// change exists to skip.
///
/// Deliberately excludes two groups:
/// - The unnamed `""` group (the test file's own first-party closure):
///   Vitest's `toMatchSnapshot()` writes `.snap` files directly next to the
///   test file, so staging that tree read-only would turn a first-time
///   snapshot write (or `--update`) into an EACCES crash instead of the
///   silently-discarded-on-teardown write it is today.
/// - `"codegen"`: a `js_src` target's generated files land at "real,
///   deterministic source-tree paths" inside the package (see
///   `js_src_query_addr`'s doc) — the same co-located-with-hand-written-
///   source shape that makes `""` unsafe. Its own Content shape (one file
///   vs. a directory tree, and whether it could overlap a path something
///   else in the sandbox needs to write) also isn't established the way it
///   is for the other three groups, so it gets the same conservative
///   treatment rather than an unverified assumption.
///
/// Toggling this set is cache-key-stable: `annotations` never reaches the
/// input hash or `JsTestDef::hash` (Source/closure content is hashed via the
/// artifact itself, not the `Input` wrapper) — no existing cached result
/// goes stale by adding or removing a group here.
const READ_ONLY_DEP_GROUPS: &[&str] = &["external", "runner_config", "tsconfig"];

/// Config for a `js_test` target. Entirely engine-generated by the `js`
/// provider's `Provider::get` (see `pluginjs::provider::Provider::test_config`)
/// — never authored by hand in a BUILD file.
#[derive(Spec)]
struct JsTestSpec {
    /// Which test runner this target invokes: `"vitest"` or `"jest"` (see
    /// `toolchain::is_supported_testrunner`).
    #[spec(required)]
    testrunner: String,
    /// Absolute host path to the resolved runner binary
    /// (`toolchain::resolve_host_test_runner`). Deliberately **not** part of
    /// `JsTestDef`'s hash — see that struct's `runner_bin` field doc.
    #[spec(required)]
    runner_bin: String,
    /// The runner's own `--version` output, trimmed, queried once by
    /// `Provider::get`. Hashed: a host runner upgrade/downgrade must bust the
    /// cache.
    #[spec(required)]
    runner_version: String,
    /// Workspace-root-relative path to the one test file this target runs.
    /// Hashed (see module docs) and used by `run()` to build the runner
    /// invocation's file argument.
    #[spec(required)]
    test_file: String,
    /// Workspace-root-relative path to the resolved runner config
    /// (`vitest.config.ts` / `jest.config.js`), or empty when none exists on
    /// the ancestor chain.
    runner_config_path: String,
    /// The resolved runner config's own raw bytes, hashed directly — see
    /// module docs' "Inputs / cache key" section for why this is not purely
    /// redundant with the declared `"runner_config"` dep group.
    runner_config_content: String,
    /// Workspace-root-relative path to the tsconfig in effect for this
    /// package (the package's own, or the nearest ancestor's — see
    /// `importgraph::find_nearest_tsconfig`), or empty when none exists on
    /// the ancestor chain. Mirrors `js_typecheck`'s identically-named field.
    tsconfig_path: String,
    /// The resolved tsconfig's own raw bytes (plus its whole `extends`
    /// chain), hashed directly — same deliberate redundancy with the
    /// declared `"tsconfig"` dep group that `runner_config_content` has with
    /// `"runner_config"`. Not merely a cache-key nicety here: vitest
    /// transforms TS via Vite's own esbuild-based transform, and a
    /// `tsconfig-paths`-style Vite plugin reads `tsconfig.json` directly at
    /// runtime — without it staged into the sandbox at all, a `paths` alias
    /// (`@/*`) fails to resolve even though the aliased file itself is
    /// correctly staged via the `""` closure group (confirmed live, M4
    /// review: this was a real, shipped gap, not a hypothetical one).
    tsconfig_content: String,
    /// Dependencies, grouped by name → target addresses: `""` = the test
    /// file's own runtime-transitive first-party closure (always includes
    /// the test file itself), `"external"` = every file that closure reaches
    /// just outside its owning package (workspace sibling or third-party,
    /// one-hop only) plus any lockfile-resolved third-party addr for an
    /// import that never resolved on disk at all, `"runner_config"` = the
    /// resolved runner config file, if any, plus every additional file its
    /// own content names (`setupFiles`/`setupFilesAfterEnv`/`globalSetup`/
    /// `globalTeardown`) or imports (a relative `import`/`require` of a
    /// shared base config), recursively — see
    /// `importgraph::resolve_runner_config_referenced_files` and module
    /// docs' "Inputs / cache key" section, `"tsconfig"` = the resolved
    /// tsconfig file and its whole `extends` chain.
    deps: HashMap<String, Vec<String>>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct JsTestDef {
    testrunner: String,
    runner_version: String,
    test_file: String,
    runner_config_path: String,
    runner_config_content: String,
    tsconfig_path: String,
    tsconfig_content: String,
    /// Absolute host runner path — carried through so `run()` can exec it,
    /// but see `Hash` impl below: deliberately excluded from the cache key.
    runner_bin: String,
}

/// Bump to invalidate every cached `js_test` result whenever the invocation
/// shape (flags, runner-config-lookup rule, what counts as a hashed config
/// field) changes in a way the declared `Input` content hash alone would not
/// already capture.
///
/// `2`: added `--reporter=json --outputFile=<path>` (vitest) /
/// `--json --outputFile=<path>` (jest) to every invocation — see module
/// docs' "Detecting real completion, not waiting for exit". Both flags are
/// designed by their respective tools to be additive/non-invasive, but a
/// cached result from before this change never actually ran with them, so a
/// stale cache hit must not stand in for having done so.
///
/// `3`: added `tsconfig_path`/`tsconfig_content` and the `"tsconfig"` dep
/// group — a resolved tsconfig previously wasn't declared as an Input at
/// all, so it was never staged into the sandbox; a cached result from
/// before this change ran without it physically present, which a real
/// `tsconfig-paths`-style Vite plugin depends on to resolve `paths`
/// aliases (confirmed live, M4 review).
const JS_TEST_FORMAT_VERSION: u32 = 3;

impl Hash for JsTestDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        JS_TEST_FORMAT_VERSION.hash(state);
        self.testrunner.hash(state);
        self.runner_version.hash(state);
        self.test_file.hash(state);
        self.runner_config_path.hash(state);
        self.runner_config_content.hash(state);
        self.tsconfig_path.hash(state);
        self.tsconfig_content.hash(state);
        // `runner_bin` is deliberately NOT hashed — an absolute host
        // filesystem path that differs across machines/checkouts for the
        // exact same effective toolchain; see `JsTypecheckDef::hash`'s
        // identical `tsc_bin` exclusion for the full rationale.
        //
        // Source/closure file *content* arrives via declared `Input`s; the
        // engine hashes that separately (architecture.md's "Automatic
        // hashing"), so it is not duplicated here.
    }
}

pub struct JsTestDriver;

impl JsTestDriver {
    pub fn new() -> Self {
        Self
    }
}

impl Default for JsTestDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ManagedDriver for JsTestDriver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "js_test".to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        JsTestSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let addr = &req.target_spec.addr;
        let pkg = addr.package.clone();
        let spec = JsTestSpec::from(&req.target_spec.config).context("parse js_test config")?;

        anyhow::ensure!(
            toolchain::is_supported_testrunner(&spec.testrunner),
            "js_test: unsupported testrunner {:?} for {} — expected \"vitest\" or \"jest\"",
            spec.testrunner,
            addr.format()
        );

        // Dep groups arrive from a HashMap — sort by group name so the
        // resulting `inputs` (and thus anything order-sensitive downstream)
        // is deterministic across parses, not HashMap-iteration order. Same
        // regression class as `js_typecheck`/`js_package_info`'s own
        // dep-input ordering.
        let mut groups: Vec<(&String, &Vec<String>)> = spec.deps.iter().collect();
        groups.sort_by_key(|(group, _)| *group);
        let mut inputs: Vec<Input> = Vec::new();
        for (group, addrs) in groups {
            let annotations: std::collections::BTreeMap<String, String> =
                if READ_ONLY_DEP_GROUPS.contains(&group.as_str()) {
                    std::collections::BTreeMap::from([
                        (
                            hdriver_support::stage::READ_ONLY_ANNOTATION.to_string(),
                            "true".to_string(),
                        ),
                        (
                            hdriver_support::stage::STAGE_PER_FILE_ANNOTATION.to_string(),
                            "true".to_string(),
                        ),
                    ])
                } else {
                    Default::default()
                };
            for (i, addr_str) in addrs.iter().enumerate() {
                inputs.push(Input {
                    r#ref: TargetAddr::parse(addr_str, &pkg)
                        .with_context(|| format!("parse dep addr {addr_str}"))?,
                    mode: InputMode::Standard,
                    origin_id: format!("dep|{group}|{i}"),
                    annotations: annotations.clone(),
                    hashed: true,
                    runtime: true,
                });
            }
        }

        let def = JsTestDef {
            testrunner: spec.testrunner,
            runner_version: spec.runner_version,
            test_file: spec.test_file,
            runner_config_path: spec.runner_config_path,
            runner_config_content: spec.runner_config_content,
            tsconfig_path: spec.tsconfig_path,
            tsconfig_content: spec.tsconfig_content,
            runner_bin: spec.runner_bin,
        };

        let hash = {
            let mut h =
                DebugHasher::new(Xxh3Default::new(), || format!("js_test_{}", addr.format()));
            def.hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs,
                // No output artifact: a `js_test` target's only observable
                // effect is "did the test pass" — mirrors
                // `js_typecheck`'s read-only, produces-nothing shape.
                outputs: vec![],
                support_files: vec![],
                cache: CacheConfig::on(true),
                pty: false,
                hash,
                transparent: false,
            },
        })
    }

    async fn apply_transitive(
        &self,
        req: ApplyTransitiveRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ApplyTransitiveResponse> {
        Ok(ApplyTransitiveResponse {
            target_def: req.target_def,
        })
    }

    async fn run<'a, 'io>(
        &self,
        mut req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let def = req.request.target.def_de::<JsTestDef>();
        // Defense in depth: `Provider::get` already rejects an absolute or
        // `..`-escaping `file`/`runner_config_path` before ever building this
        // `TargetDef` (see `provider::reject_path_escape`'s doc — a
        // code-quality review BLOCKER), but a cached `JsTestDef` read back
        // from disk is untrusted input to this driver too, so the same check
        // runs again here, before either ever reaches `sandbox_ws_dir.join`.
        crate::pluginjs::provider::reject_path_escape("test_file", &def.test_file)
            .context("validating cached js_test def before running")?;
        if !def.runner_config_path.is_empty() {
            crate::pluginjs::provider::reject_path_escape(
                "runner_config_path",
                &def.runner_config_path,
            )
            .context("validating cached js_test def before running")?;
        }
        let runner_bin = std::path::PathBuf::from(&def.runner_bin);
        let test_file_abs = req.sandbox_ws_dir.join(&def.test_file);

        // `testrunner = "vitest"/"jest"` are explicitly non-hermetic (see
        // module docs): the resolved binary is very likely a
        // `#!/usr/bin/env node` shebang script, so executing it needs `node`
        // reachable via PATH — mirrors `js_typecheck::run`'s identical
        // PATH-only passthrough and its own doc comment for why only `PATH`
        // (not `HOME`/`TMPDIR`/etc.) is forwarded.
        //
        // `TZ`/`LANG` are pinned rather than left to the host's ambient
        // configuration: absent both, Node/ICU falls back to the host's own
        // timezone/locale (`/etc/localtime` etc.), so a `Date`/`Intl` test
        // could legitimately pass on a `TZ=UTC` CI host and fail on a
        // `TZ=America/Los_Angeles` laptop (or vice versa) while producing the
        // byte-identical `JsTestDef` hash — a hermeticity M4 review finding.
        // Pinning both makes the child's environment part of what the cache
        // key actually reflects (identical on every host), rather than a
        // silent, unhashed source of divergence.
        // `CI=1`: this invocation is headless by construction — no TTY, no
        // human able to answer a prompt or press a key — which is exactly
        // what `CI` conventionally signals to a well-behaved CLI tool.
        // Defense-in-depth alongside the explicit `run` subcommand above
        // (which already defeats vitest's *own* interactive-watch-mode
        // default): vitest and jest both gate other interactive behavior
        // (reporter live-redraw, keypress handling) on `isCI`/`isTTY`
        // checks beyond just the watch/run selection, and neither driver
        // nor its child should ever assume an interactive terminal exists.
        let mut env: HashMap<String, String> = HashMap::new();
        if let Ok(v) = std::env::var("PATH") {
            env.insert("PATH".to_string(), v);
        }
        env.insert("TZ".to_string(), "UTC".to_string());
        env.insert("LANG".to_string(), "C.UTF-8".to_string());
        env.insert("CI".to_string(), "1".to_string());

        // Where the runner is asked to write its own structured, JSON
        // completion signal — see module docs' "Detecting real completion,
        // not waiting for exit". `exec_runner` polls this same path
        // (derived the same way, from the same `cwd`), so the two can never
        // disagree about where to look.
        let result_path = req.sandbox_pkg_dir.join(RESULT_FILE_NAME);

        let mut args: Vec<OsString> = Vec::new();
        match def.testrunner.as_str() {
            toolchain::VITEST => {
                // `run`, not bare `vitest`: bare `vitest` defaults to
                // interactive watch mode, which would hang forever under
                // heph (see module docs' "Invocation shape").
                args.push(OsString::from("run"));
                if !def.runner_config_path.is_empty() {
                    args.push(OsString::from("--config"));
                    args.push(
                        req.sandbox_ws_dir
                            .join(&def.runner_config_path)
                            .into_os_string(),
                    );
                }
                // `--reporter=default` keeps the normal human-readable
                // output (still captured for the fallback failure detail);
                // `--reporter=json` + `--outputFile` is the completion
                // signal `exec_runner` polls for.
                args.push(OsString::from("--reporter=default"));
                args.push(OsString::from("--reporter=json"));
                args.push(outputfile_arg(&result_path));
                args.push(test_file_abs.into_os_string());
            }
            toolchain::JEST => {
                if !def.runner_config_path.is_empty() {
                    args.push(OsString::from("--config"));
                    args.push(
                        req.sandbox_ws_dir
                            .join(&def.runner_config_path)
                            .into_os_string(),
                    );
                }
                // `--json` writes the same Jest-compatible structured
                // result `exec_runner` polls for (vitest's own `json`
                // reporter is explicitly modeled on it); jest additionally
                // redirects its normal human output to stderr in this mode
                // (its own documented `--json` behavior), which is still
                // captured for the fallback failure detail.
                args.push(OsString::from("--json"));
                args.push(outputfile_arg(&result_path));
                // An exact path match, not a `testPathPattern` regex — see
                // module docs' "Invocation shape".
                args.push(OsString::from("--runTestsByPath"));
                args.push(test_file_abs.into_os_string());
            }
            other => anyhow::bail!(
                "js_test: unsupported testrunner {other:?} — expected \"vitest\" or \"jest\" \
                 (this should have been rejected at parse() time)"
            ),
        }

        // Taken regardless of `--shell`: the engine hands the single named
        // target a live terminal whenever the run is interactive (see
        // `hplugin::driver::RunRequest`'s doc and `docker_build.rs`'s
        // `ToolIo`) — `--shell` only additionally wires stdin. Without this,
        // the sinks are simply `None` and `exec_runner`'s tee below is a
        // no-op, exactly like every other consumer of this field.
        let stdout_sink = req.request.stdout.take();
        let stderr_sink = req.request.stderr.take();

        self.exec_runner(
            &runner_bin,
            args,
            &env,
            &req.sandbox_pkg_dir,
            &req.sandbox_dir,
            stdout_sink,
            stderr_sink,
            ctoken,
        )
        .await?;

        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

/// Relative filename (joined onto the target's own sandbox package dir —
/// the runner's `cwd`) that the runner is asked to write its structured JSON
/// completion report to, alongside its normal human-readable output. Never a
/// declared `Input` or `Output` — purely `exec_runner`'s own completion-
/// detection bookkeeping, gone once `run()` returns along with the rest of
/// the sandbox. Mirrors `driver_golist.rs`'s identically-undeclared
/// `.heph-gocache`. See module docs' "Detecting real completion, not
/// waiting for exit".
const RESULT_FILE_NAME: &str = ".heph-js-test-result.json";

/// Where `exec_runner` tees the runner's merged stdout+stderr, live, as it
/// arrives — the same filename `pluginexec` and `docker_build.rs` use for
/// their own equivalent file. Unlike [`RESULT_FILE_NAME`] this is not a
/// heph-internal scratch file: it is what makes a failing (or hung) run's
/// output inspectable on disk without needing `heph run --shell` to attach
/// a real terminal and reproduce it by hand — see module docs' "Surfacing
/// output live, not just on completion". **Not** currently wired into
/// `crates/engine`'s `extract_log_tail`/`ProcessFailed` framed-box renderer
/// or `--cat-out`/`--copy-out` the way `pluginexec`'s/`docker_build`'s own
/// `log.txt` are — those require the driver to return a `ProcessFailed{
/// log, .. }` error and declare a matching `Output`, neither of which this
/// driver does (`js_test` deliberately declares no outputs at all, see
/// `parse_no_outputs_and_caches_locally_and_remotely`). This file's value
/// today is the live tee to `stdout_sink`/`stderr_sink` above, plus being
/// left on disk in the sandbox for manual inspection on failure (the engine
/// preserves a failed target's sandbox for diagnostics either way).
const RUNNER_LOG_FILE_NAME: &str = "log.txt";

/// How often `exec_runner` checks for [`RESULT_FILE_NAME`] while the runner
/// is still producing output. Cheap enough (one `stat`+maybe-`read` of a
/// small file) that a short interval costs nothing over a whole run, and
/// short enough that detection reads as immediate to a human waiting on it.
const RESULT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// How long a test-runner subprocess gets before it's killed and reported
/// as a timeout, when it produces *neither* a normal exit *nor* a parseable
/// [`RESULT_FILE_NAME`] — the last-resort backstop for a runner with no
/// signal to key off at all. Generous on purpose: this guards against a
/// truly wedged child, not a performance budget — a real, large,
/// slow-but-working suite must never spuriously trip it. See module docs'
/// "Detecting real completion, not waiting for exit" for why this is no
/// longer the primary mechanism that ends a completed run.
const RUNNER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20 * 60);

/// The exact line Vite's logger prints, to either stream, the instant its
/// dependency-optimizer scan fails — always led by Vite's own `(!) ` warning
/// glyph, which this deliberately includes rather than matching the bare
/// phrase: a code-quality review flagged that an unanchored substring could
/// false-match a project's own unrelated log/assertion text containing the
/// same words, killing an otherwise-passing run before it ever gets a
/// chance to write [`RESULT_FILE_NAME`]. Confirmed, unfixed upstream as of
/// this writing: the scan's error path returns early and never resolves the
/// `depOptimizationProcessing` promise every subsequent module load is
/// awaiting, so the runner hangs **forever** — not slow, not eventually
/// self-resolving — with no exit and no [`RESULT_FILE_NAME`] ever coming
/// (vitejs/vite#22934, vitest-dev/vitest#9799; also vite#19364, #19316).
/// Vitest has defaulted this optimizer *off* since v1.3.0, so hitting this
/// at all means a project's own `vitest.config` explicitly re-enabled it
/// (`test.deps.optimizer.{web,ssr}.enabled = true`, `client` replacing `web`
/// in v4) — commonly for CJS/ESM interop, not for speed (heph's hermetic,
/// cold-cache-every-run sandbox gets no benefit from the optimizer either
/// way). Because this is a *confirmed-unrecoverable* condition rather than
/// mere silence, seeing this exact line is treated as a positive completion
/// signal — same as [`RESULT_FILE_NAME`] appearing — instead of relying on
/// [`RUNNER_TIMEOUT`]: unlike a generic idle/stall heuristic (which would
/// risk misfiring on a legitimately slow-but-silent single test file), this
/// only fires on the literal, versions-stable diagnostic text for a bug with
/// no other outcome. Not part of the hash: this is runtime driver control
/// flow, not a declared `Input` — if a future vitest/jest ever changes this
/// wording, the fast path just silently stops firing and every target falls
/// back to the pre-existing, already-accepted [`RUNNER_TIMEOUT`] backstop
/// (slower, never wrong).
const DEPENDENCY_SCAN_HANG_MARKER: &str = "(!) Failed to run dependency scan";

/// How long [`DEPENDENCY_SCAN_HANG_MARKER`] alone is trusted before the poll
/// task acts on it — a real result file discovered during this window still
/// wins (checked first, every tick; see the poll loop below). This is what
/// keeps the marker match itself safely broad (any occurrence, either
/// stream, no stricter anchoring than the `(!) ` prefix): even a coincidental
/// or borderline match costs the run at most this much extra latency, never
/// a wrong verdict, because a run that was always going to finish on its own
/// still gets to. Short relative to [`RUNNER_TIMEOUT`] on purpose — the
/// confirmed real trigger produces no further progress at all once printed,
/// so there is nothing to lose by not waiting long, and a lot to lose (the
/// original 20-minute complaint) by waiting anywhere near as long as that.
const DEPENDENCY_SCAN_HANG_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Feed `chunk` through a small rolling window (bounded to `marker.len() -
/// 1` bytes of carry-over between calls) and report whether `marker`
/// appears anywhere in the combined bytes — catching an occurrence split
/// across a chunk boundary without ever rescanning more than one chunk's
/// worth of previously-seen bytes (i.e. O(chunk length), not O(bytes seen so
/// far) — load-bearing since this runs on every chunk of a stream that is
/// otherwise unbounded for the life of the run).
fn rolling_window_contains(tail: &mut Vec<u8>, chunk: &[u8], marker: &[u8]) -> bool {
    tail.extend_from_slice(chunk);
    let found = tail.windows(marker.len()).any(|w| w == marker);
    let keep_from = tail.len().saturating_sub(marker.len().saturating_sub(1));
    tail.drain(..keep_from);
    found
}

/// The explanation surfaced when [`DEPENDENCY_SCAN_HANG_GRACE`] elapses
/// after [`DEPENDENCY_SCAN_HANG_MARKER`] with no result file — shared by the
/// poll task's primary path and `exec_runner`'s own fallback net for the
/// (rare) race where the child is reaped for an unrelated reason before the
/// poll task itself gets to conclude.
fn dependency_scan_hang_error(stdout: &str, stderr: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "js_test: the test runner hit Vite's dependency-optimizer scan failure and then hung — \
         this is a confirmed, unfixed upstream bug: the failed scan leaves an internal promise \
         permanently unresolved, so every subsequent module load blocks forever \
         (vitejs/vite#22934, vitest-dev/vitest#9799). heph killed the runner rather than waiting \
         out the {}-minute timeout. Fix the resolution error the runner reported below, or \
         disable the dependency optimizer in this project's vitest config \
         (`test.deps.optimizer.web.enabled = false` and `...ssr.enabled = false` — `web` is \
         named `client` in vitest v4 — vitest has defaulted this off since v1.3.0, so it is very \
         likely explicitly re-enabled here):\n{}",
        RUNNER_TIMEOUT.as_secs() / 60,
        test_failure_detail(stdout, stderr),
    )
}

/// A [`Cancellable`] that fires either when `inner` does or once `deadline`
/// elapses — whichever comes first — without otherwise changing `inner`'s
/// own semantics. Composing this way (rather than wrapping the subprocess
/// future in a bare `tokio::time::timeout`) reuses `proc_exec`'s existing
/// SIGINT→grace→SIGKILL teardown (see `imp_linux.rs`/`imp_macos.rs`), so a
/// deadline-triggered kill cleans up the child exactly like a real
/// cancellation does — a raw `timeout` would just drop the future and leak
/// the orphaned process. The *original* `ctoken` a caller already had is
/// left untouched: only the derived, request-local copy passed to the
/// runner ever observes the deadline, so a target that merely timed out is
/// never mistaken upstream for a user-cancelled run (which engine code
/// separately keys off the real, unwrapped token).
struct DeadlineCancellable {
    inner: Arc<dyn Cancellable + Send + Sync>,
    deadline: tokio::time::Instant,
}

impl Cancellable for DeadlineCancellable {
    fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled() || tokio::time::Instant::now() >= self.deadline
    }

    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            tokio::select! {
                () = self.inner.cancelled() => {}
                () = tokio::time::sleep_until(self.deadline) => {}
            }
        })
    }

    fn clone_arc(&self) -> Arc<dyn Cancellable + Send + Sync> {
        Arc::new(DeadlineCancellable {
            inner: self.inner.clone(),
            deadline: self.deadline,
        })
    }
}

/// A [`Cancellable`] that fires the moment either `a` or `b` does. Used to
/// layer `exec_runner`'s own manually-triggered "the runner's structured
/// result appeared" signal on top of the caller's real cancellation token,
/// so both feed the same [`DeadlineCancellable`]-driven kill path.
struct EitherCancellable {
    a: Arc<dyn Cancellable + Send + Sync>,
    b: Arc<dyn Cancellable + Send + Sync>,
}

impl Cancellable for EitherCancellable {
    fn is_cancelled(&self) -> bool {
        self.a.is_cancelled() || self.b.is_cancelled()
    }

    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            tokio::select! {
                () = self.a.cancelled() => {}
                () = self.b.cancelled() => {}
            }
        })
    }

    fn clone_arc(&self) -> Arc<dyn Cancellable + Send + Sync> {
        Arc::new(EitherCancellable {
            a: self.a.clone(),
            b: self.b.clone(),
        })
    }
}

/// Builds the `--outputFile=<path>` argument as a single `OsString`,
/// concatenated rather than formatted through `Path::display` so a
/// non-UTF-8 sandbox path (rare, but the sandbox root is host-controlled,
/// not guaranteed ASCII) survives intact instead of being lossily rendered.
fn outputfile_arg(path: &std::path::Path) -> OsString {
    let mut s = OsString::from("--outputFile=");
    s.push(path.as_os_str());
    s
}

/// A single test file's entry in the runner's Jest-compatible JSON report —
/// only the fields this driver actually reads. `message` is the same
/// preformatted failure text (assertion diffs, stack traces) a human would
/// see in the console reporter for this file, already assembled by the
/// runner itself — richer and more precise than scraping it back out of
/// captured stdout.
#[derive(serde::Deserialize)]
struct RunnerTestFileResult {
    #[serde(default)]
    message: String,
}

/// The runner's own structured completion report — the Jest `--json`
/// shape, which vitest's `json` reporter is explicitly modeled on (both
/// exist for exactly this kind of external-tooling consumption). `success`
/// is the sole source of truth for the target's pass/fail verdict once this
/// file has been seen — never the process's eventual exit code, which is
/// exactly the signal that can't be trusted here (see module docs).
#[derive(serde::Deserialize)]
struct RunnerJsonResult {
    success: bool,
    #[serde(default, rename = "testResults")]
    test_results: Vec<RunnerTestFileResult>,
}

/// What `exec_runner`'s poll task concluded: either the runner produced its
/// structured result (the normal case, whatever its verdict), or
/// [`DEPENDENCY_SCAN_HANG_GRACE`] elapsed after [`DEPENDENCY_SCAN_HANG_MARKER`]
/// with no result ever appearing (the confirmed-unrecoverable case).
enum PollOutcome {
    Result(Result<RunnerJsonResult, String>),
    DependencyScanHang,
}

/// Polls-once: `None` means "not ready yet" (missing, or present but not
/// yet valid JSON — a partial write in progress) and the caller should keep
/// waiting; `Some` means the runner is done producing it, either with the
/// expected shape (`Ok`) or something else entirely (`Err`, a human-readable
/// diagnostic) — schema drift must surface as a named, fast error, never a
/// silent mis-report of pass as fail or vice versa (see `[[Fail or fix,
/// never ignore]]`).
fn try_read_runner_result(path: &std::path::Path) -> Option<Result<RunnerJsonResult, String>> {
    let bytes = std::fs::read(path).ok()?;
    // Not yet valid JSON at all — almost certainly a partial write still in
    // flight (the runner's own `writeFileSync`-style call hasn't landed the
    // last byte yet), not a schema problem. Keep polling.
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    match serde_json::from_value::<RunnerJsonResult>(value) {
        Ok(r) => Some(Ok(r)),
        Err(e) => Some(Err(format!(
            "runner wrote {path:?} but its content didn't have the expected shape (missing/\
             invalid `success`): {e} — got: {}",
            hplugin::error::head_and_tail_lines(&String::from_utf8_lossy(&bytes), 20)
        ))),
    }
}

/// Build the failure detail for a `RunnerJsonResult` with `success: false`.
/// Prefers each failing test file's own `message` (the runner's own
/// preformatted diagnostic — see `RunnerTestFileResult` doc); falls back to
/// the captured raw stdout/stderr tail when no structured message exists at
/// all (e.g. a crash before any test file was even collected), and appends
/// that raw tail as a supplement otherwise — a warning printed outside any
/// test result (a Vite dependency-scan failure, say) lives only in the raw
/// stream, never in `testResults`, and must not be lost just because a
/// structured message was also available. Skipped when the raw tail already
/// contains the structured message verbatim (the common case: vitest's own
/// `Error:`-prefixed terminal dump and the JSON reporter's `message` field
/// both derive from the same underlying error) — appending it there would
/// just print the same failure twice, once terse and once in full.
fn failure_detail_from_json(result: &RunnerJsonResult, stdout: &str, stderr: &str) -> String {
    let mut structured = String::new();
    for tr in &result.test_results {
        let msg = tr.message.trim();
        if msg.is_empty() {
            continue;
        }
        if !structured.is_empty() {
            structured.push_str("\n\n");
        }
        structured.push_str(msg);
    }
    if structured.is_empty() {
        return test_failure_detail(stdout, stderr);
    }
    if stdout.trim().is_empty() && stderr.trim().is_empty() {
        return structured;
    }
    let raw = test_failure_detail(stdout, stderr);
    if raw.contains(structured.as_str()) {
        return raw;
    }
    format!("{structured}\n\n--- raw stdout/stderr ---\n{raw}")
}

impl JsTestDriver {
    #[expect(
        clippy::too_many_arguments,
        reason = "runner_bin/args/env/cwd/sandbox_dir/stdout/stderr/ctoken are each independently \
                  needed by the spawn below; bundling them into a struct would just move the same \
                  fields one level down for no reduction in what a caller has to supply"
    )]
    async fn exec_runner(
        &self,
        runner_bin: &std::path::Path,
        args: Vec<OsString>,
        env: &HashMap<String, String>,
        cwd: &std::path::Path,
        sandbox_dir: &std::path::Path,
        mut stdout_sink: Option<&mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin)>,
        mut stderr_sink: Option<&mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin)>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<()> {
        let env_pairs: Vec<(OsString, OsString)> = env
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect();
        let result_path = cwd.join(RESULT_FILE_NAME);
        // A stale file from a prior invocation that reused this sandbox
        // path (e.g. a retry after a build-system-level failure) must never
        // be mistaken for this run's own result.
        drop(std::fs::remove_file(&result_path));

        // Merged stdout+stderr, in arrival order — see
        // `RUNNER_LOG_FILE_NAME`'s doc for exactly what reads this today
        // (the live sink tee below, and manual inspection of the preserved
        // sandbox) versus what it does not yet plug into. Truncated fresh
        // each run: a stale tail from a previous invocation of this same
        // sandbox path must never be read as this run's own output.
        let log_path = sandbox_dir.join(RUNNER_LOG_FILE_NAME);
        let log = std::sync::Mutex::new(
            std::fs::File::create(&log_path).context("create js_test log file")?,
        );

        let spec = proc_exec::Spec {
            program: runner_bin.to_path_buf(),
            args,
            env: env_pairs,
            cwd: cwd.to_path_buf(),
            stdin: proc_exec::StdioSpec::Null,
            stdout: proc_exec::StdioSpec::Piped,
            stderr: proc_exec::StdioSpec::Piped,
            setsid: false,
            ctty: false,
        };

        let deadline = tokio::time::Instant::now() + RUNNER_TIMEOUT;
        // Manually fired the instant this function itself decides the run
        // is over (a parseable result file appeared) — composed alongside
        // the caller's real cancellation token and the deadline backstop so
        // all three feed the same kill path.
        let done = hcore::hasync::StdCancellationToken::new();
        let bounded: Arc<dyn Cancellable + Send + Sync> = Arc::new(DeadlineCancellable {
            inner: Arc::new(EitherCancellable {
                a: ctoken.clone_arc(),
                b: done.clone_arc(),
            }),
            deadline,
        });

        let mut handle = proc_exec::spawn(spec).context("spawn test runner")?;
        let mut reader = handle
            .take_output()
            .context("test runner spawned with neither stdout nor stderr piped")?;
        let mut wait = handle.spawn_wait(bounded);

        // Poll for the runner's own completion signal on a *separate* task
        // from the one draining stdout/stderr below — never a `select!`
        // racing the two in one task. `OutputReader::recv`'s macOS backend
        // parks the calling task in `block_in_place` while waiting for a
        // chunk, which is never `Pending`: a `select!` arm alongside it
        // cannot be polled until `recv` itself returns, so a poll timer in
        // the same task would starve for as long as the runner stays
        // silent — exactly the wedged-runner case this mechanism exists to
        // catch (confirmed live: reproduced the resulting hang under the
        // `multi_thread` flavor every production runtime actually uses).
        // Two independent tasks keep the poll ticking regardless of what
        // the drain side is doing, matching the already-safe shape
        // `pluginexec` and `plugin-oci`'s `docker_build.rs` use for the
        // same `OutputReader`.
        // Set once (`Some(Instant)`), from the drain loop below, the moment
        // `DEPENDENCY_SCAN_HANG_MARKER` is seen on either stream. Shared
        // rather than acted on immediately by the drain loop itself so a
        // result file appearing within `DEPENDENCY_SCAN_HANG_GRACE` still
        // wins — checked first, every tick, below.
        let marker_seen_at: Arc<std::sync::Mutex<Option<tokio::time::Instant>>> =
            Arc::new(std::sync::Mutex::new(None));

        let poll_result_path = result_path.clone();
        let poll_done = done.clone();
        let poll_marker_seen_at = Arc::clone(&marker_seen_at);
        let poll_task = tokio::spawn(async move {
            let mut poll = tokio::time::interval(RESULT_POLL_INTERVAL);
            poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                poll.tick().await;
                let path = poll_result_path.clone();
                if let Some(found) =
                    hcore::blocking::run(move || try_read_runner_result(&path)).await
                {
                    poll_done.cancel();
                    return PollOutcome::Result(found);
                }
                let hang_confirmed = poll_marker_seen_at
                    .lock()
                    .expect("marker_seen_at poisoned")
                    .is_some_and(|seen_at| seen_at.elapsed() >= DEPENDENCY_SCAN_HANG_GRACE);
                if hang_confirmed {
                    poll_done.cancel();
                    return PollOutcome::DependencyScanHang;
                }
            }
        });

        // Drain both streams — required, an unread 64 KiB pipe blocks the
        // child in `write()` forever — raced against the child being
        // reaped, mirroring `proc_exec::output()`'s own structure and for
        // the identical reason: this driver always spawns with `setsid:
        // false` (matching a real vitest/jest invocation, never made a
        // session leader), so `killpg` cannot reach a descendant the
        // runner backgrounds that inherits its stdout/stderr — vitest's
        // dependency-optimizer esbuild service is exactly this. Killing
        // the direct child does not close that descendant's copy of the
        // write end, so an *unbounded* drain here would just trade the
        // original "process never exits" hang for "the drain never
        // reaches EOF" — the same symptom, reached through the new
        // mechanism instead of the old one (confirmed live: this is
        // exactly what the first version of this fix did, and it still
        // hung). `DRAIN_DEADLINE` bounds it exactly as it already bounds
        // `output()`'s identical situation.
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut read_error: Option<std::io::Error> = None;
        // Local skip-optimization only — avoids re-scanning every
        // subsequent chunk once the marker has already been recorded into
        // `marker_seen_at`; the actual cross-task signal is that `Mutex`,
        // not this `bool`.
        let mut marker_seen_locally = false;
        // Scoped so the pinned `drain` future — which mutably borrows
        // `stdout`/`stderr`/`read_error` for as long as it's alive — is
        // dropped before those are read again below.
        let wait_outcome = {
            // Tee, not buffer-then-report: every chunk lands in `log.txt` and
            // (when the engine handed this target a live terminal — see
            // `run()`) the caller's own stdout/stderr *as it arrives*, not
            // only once this function eventually returns. A wedged runner
            // (the vitest/esbuild case module docs describe) may never
            // return at all within any reasonable bound; what it already
            // printed before wedging must not wait on that. Mirrors
            // `pluginexec::tee_output`/`docker_build.rs::tee`, simplified to
            // this driver's single-reader shape (no absorption-time
            // accounting: a test runner's output is orders of magnitude
            // smaller than a container build's).
            use std::io::Write as _;
            use tokio::io::AsyncWriteExt as _;
            let marker = DEPENDENCY_SCAN_HANG_MARKER.as_bytes();
            let mut marker_scan_tail: Vec<u8> = Vec::new();
            let drain = async {
                loop {
                    match reader.recv().await {
                        Ok(Some((proc_exec::StreamId::Stdout, bytes))) => {
                            if let Ok(mut f) = log.lock() {
                                drop(f.write_all(&bytes));
                            }
                            if let Some(out) = stdout_sink.as_mut() {
                                drop(out.write_all(&bytes).await);
                                drop(out.flush().await);
                            }
                            if !marker_seen_locally
                                && rolling_window_contains(&mut marker_scan_tail, &bytes, marker)
                            {
                                marker_seen_locally = true;
                                if let Ok(mut seen_at) = marker_seen_at.lock() {
                                    *seen_at = Some(tokio::time::Instant::now());
                                }
                            }
                            stdout.extend_from_slice(&bytes);
                        }
                        Ok(Some((proc_exec::StreamId::Stderr, bytes))) => {
                            if let Ok(mut f) = log.lock() {
                                drop(f.write_all(&bytes));
                            }
                            if let Some(out) = stderr_sink.as_mut() {
                                drop(out.write_all(&bytes).await);
                                drop(out.flush().await);
                            }
                            if !marker_seen_locally
                                && rolling_window_contains(&mut marker_scan_tail, &bytes, marker)
                            {
                                marker_seen_locally = true;
                                if let Ok(mut seen_at) = marker_seen_at.lock() {
                                    *seen_at = Some(tokio::time::Instant::now());
                                }
                            }
                            stderr.extend_from_slice(&bytes);
                        }
                        Ok(None) => break,
                        Err(e) => {
                            read_error = Some(e);
                            break;
                        }
                    }
                }
            };
            tokio::pin!(drain);
            // The drain reaching EOF does not end the wait, and the wait
            // ending does not (yet) end the drain — whichever lands first,
            // we keep polling the other.
            let mut all_drained = false;
            let wait_outcome = loop {
                tokio::select! {
                    () = &mut drain, if !all_drained => all_drained = true,
                    res = &mut wait => break res,
                }
            };
            if !all_drained
                && tokio::time::timeout(proc_exec::DRAIN_DEADLINE, &mut drain)
                    .await
                    .is_err()
            {
                tracing::warn!(
                    deadline_ms = proc_exec::DRAIN_DEADLINE.as_millis(),
                    "js_test: stdout/stderr drain did not reach EOF after the test runner was \
                     reaped — a descendant it spawned likely inherited the pipe; using whatever \
                     was captured so far",
                );
            }
            wait_outcome
        };

        // The child is reaped either way now. Stop the poller (a no-op if
        // it had already returned) and see whether it found anything; one
        // more direct check covers the race where the result file appeared
        // in the same instant the poller's next tick hadn't yet fired.
        poll_task.abort();
        let detection = match poll_task.await {
            Ok(found) => Some(found),
            Err(join_err) if join_err.is_cancelled() => None,
            Err(join_err) => return Err(join_err).context("result-file poll task panicked"),
        };
        let detection =
            detection.or_else(|| try_read_runner_result(&result_path).map(PollOutcome::Result));

        if let Some(found) = detection {
            return match found {
                PollOutcome::Result(Ok(r)) if r.success => Ok(()),
                PollOutcome::Result(Ok(r)) => anyhow::bail!(
                    "js_test failed:\n{}",
                    failure_detail_from_json(
                        &r,
                        &String::from_utf8_lossy(&stdout),
                        &String::from_utf8_lossy(&stderr)
                    )
                ),
                PollOutcome::Result(Err(msg)) => anyhow::bail!("js_test: {msg}"),
                PollOutcome::DependencyScanHang => Err(dependency_scan_hang_error(
                    &String::from_utf8_lossy(&stdout),
                    &String::from_utf8_lossy(&stderr),
                )),
            };
        }

        // Fallback net for the rare race where the child was reaped for an
        // unrelated reason (the caller's own cancellation, or
        // `DRAIN_DEADLINE`) before the poll task's own grace window had a
        // chance to elapse and conclude `DependencyScanHang` on its own: the
        // marker was still seen, so this still gives that specific
        // explanation rather than falling through to the generic
        // exit-code/timeout message below, which would otherwise be the
        // only thing a user sees for up to `RUNNER_TIMEOUT` (20 minutes).
        if marker_seen_at
            .lock()
            .expect("marker_seen_at poisoned")
            .is_some()
        {
            return Err(dependency_scan_hang_error(
                &String::from_utf8_lossy(&stdout),
                &String::from_utf8_lossy(&stderr),
            ));
        }

        // No structured result ever appeared — fall back to the plain
        // exit-code-based verdict this driver always used.
        let status = match wait_outcome {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => {
                if !ctoken.is_cancelled() && tokio::time::Instant::now() >= deadline {
                    anyhow::bail!(
                        "js_test: test runner ({runner_bin:?}) did not exit within {}s and \
                         produced no usable result — this is a known vitest/jest failure mode \
                         (a failed dependency-optimizer scan can leave a background service \
                         alive after the test run itself finished); fix whatever the runner's \
                         own diagnostics reported before this point, or disable its dependency \
                         optimizer:\n{}",
                        RUNNER_TIMEOUT.as_secs(),
                        test_failure_detail(
                            &String::from_utf8_lossy(&stdout),
                            &String::from_utf8_lossy(&stderr)
                        ),
                    );
                }
                return Err(e).with_context(|| format!("wait for test runner ({runner_bin:?})"));
            }
            Err(join_err) => {
                return Err(join_err).context("test runner's wait task panicked");
            }
        };
        // A genuine stream-read error (as opposed to plain EOF) with no
        // usable result to fall back on must stay visible — surfacing it
        // as a bare exit-code failure would silently shorten the captured
        // output tail with no indication why.
        if let Some(e) = read_error {
            return Err(e).with_context(|| {
                format!("reading output from test runner ({runner_bin:?}) while it was running")
            });
        }
        if !status.success() {
            anyhow::bail!(
                "js_test failed ({}):\n{}",
                status,
                test_failure_detail(
                    &String::from_utf8_lossy(&stdout),
                    &String::from_utf8_lossy(&stderr)
                )
            );
        }
        Ok(())
    }
}

/// Build the human-readable detail for a failed test-runner invocation —
/// both stdout and stderr included (head and tail, see
/// [`hplugin::error::head_and_tail_lines`]), mirroring
/// `driver_typecheck.rs`'s `tsc_failure_detail` for the identical reason: a
/// failing test's actual output (assertion diffs, stack traces) must reach
/// the user, never come back silently blank — including an exception's
/// leading name/message when a long serialized stack would otherwise push
/// it out of a tail-only cap.
fn test_failure_detail(stdout: &str, stderr: &str) -> String {
    let stdout_tail = hplugin::error::head_and_tail_lines(stdout.trim(), 60);
    let stderr_tail = hplugin::error::head_and_tail_lines(stderr.trim(), 60);
    let mut detail = String::new();
    if !stdout_tail.is_empty() {
        detail.push_str("stdout:\n");
        detail.push_str(&stdout_tail);
    }
    if !stderr_tail.is_empty() {
        if !detail.is_empty() {
            detail.push('\n');
        }
        detail.push_str("stderr:\n");
        detail.push_str(&stderr_tail);
    }
    if detail.is_empty() {
        "<no output on stdout or stderr>".to_string()
    } else {
        detail
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use hcore::htvalue::Value;
    use hmodel::htaddr::Addr;
    use hmodel::htpkg::PkgBuf;
    use hplugin::provider::TargetSpec;
    use std::collections::HashMap;

    fn driver() -> JsTestDriver {
        JsTestDriver::new()
    }

    fn ctoken() -> StdCancellationToken {
        StdCancellationToken::new()
    }

    fn config(extra: &[(&str, Value)]) -> HashMap<String, Value> {
        let mut c: HashMap<String, Value> = HashMap::from([
            (
                "testrunner".to_string(),
                Value::String("vitest".to_string()),
            ),
            (
                "runner_bin".to_string(),
                Value::String("/usr/bin/vitest".to_string()),
            ),
            (
                "runner_version".to_string(),
                Value::String("vitest/1.6.0".to_string()),
            ),
            (
                "test_file".to_string(),
                Value::String("packages/a/src/index.test.ts".to_string()),
            ),
            (
                "runner_config_path".to_string(),
                Value::String(String::new()),
            ),
            (
                "runner_config_content".to_string(),
                Value::String(String::new()),
            ),
        ]);
        for (k, v) in extra {
            c.insert((*k).to_string(), v.clone());
        }
        c
    }

    fn make_parse_request(extra: &[(&str, Value)]) -> ParseRequest {
        ParseRequest {
            request_id: "test".to_string(),
            target_spec: Arc::new(TargetSpec {
                addr: Addr::new(
                    PkgBuf::from("packages/a"),
                    "js_test".to_string(),
                    std::collections::BTreeMap::from([(
                        "file".to_string(),
                        "packages/a/src/index.test.ts".to_string(),
                    )]),
                ),
                driver: "js_test".to_string(),
                config: config(extra),
                ..Default::default()
            }),
        }
    }

    #[tokio::test]
    async fn driver_name_is_js_test() {
        let resp = driver().config(ConfigRequest {}).unwrap();
        assert_eq!(resp.name, "js_test");
    }

    #[tokio::test]
    async fn parse_missing_required_field_errors() {
        let ct = ctoken();
        let req = ParseRequest {
            request_id: "test".to_string(),
            target_spec: Arc::new(TargetSpec {
                addr: Addr::new(
                    PkgBuf::from("packages/a"),
                    "js_test".to_string(),
                    Default::default(),
                ),
                driver: "js_test".to_string(),
                ..Default::default()
            }),
        };
        assert!(driver().parse(req, &ct).await.is_err());
    }

    #[tokio::test]
    async fn parse_rejects_unsupported_testrunner() {
        let ct = ctoken();
        let req = make_parse_request(&[("testrunner", Value::String("mocha".to_string()))]);
        let err = driver()
            .parse(req, &ct)
            .await
            .err()
            .expect("an unsupported testrunner must fail parse");
        assert!(format!("{err:#}").contains("mocha"));
    }

    #[tokio::test]
    async fn parse_accepts_jest_as_well_as_vitest() {
        let ct = ctoken();
        let req = make_parse_request(&[("testrunner", Value::String("jest".to_string()))]);
        driver()
            .parse(req, &ct)
            .await
            .expect("jest must be accepted");
    }

    #[tokio::test]
    async fn parse_hash_stable_across_identical_parses() {
        let ct = ctoken();
        let a = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        let b = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        assert_eq!(a.target_def.hash, b.target_def.hash);
    }

    /// Task requirement: a runner-version change must bust the cache.
    #[tokio::test]
    async fn parse_hash_changes_when_runner_version_changes() {
        let ct = ctoken();
        let a = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        let b = driver()
            .parse(
                make_parse_request(&[(
                    "runner_version",
                    Value::String("vitest/2.0.0".to_string()),
                )]),
                &ct,
            )
            .await
            .unwrap();
        assert_ne!(a.target_def.hash, b.target_def.hash);
    }

    /// Task requirement: a different test file must be a different cache
    /// entry, even holding everything else constant.
    #[tokio::test]
    async fn parse_hash_changes_when_test_file_changes() {
        let ct = ctoken();
        let a = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        let b = driver()
            .parse(
                make_parse_request(&[(
                    "test_file",
                    Value::String("packages/a/src/other.test.ts".to_string()),
                )]),
                &ct,
            )
            .await
            .unwrap();
        assert_ne!(a.target_def.hash, b.target_def.hash);
    }

    /// Task requirement: a runner config content change must bust the cache.
    #[tokio::test]
    async fn parse_hash_changes_when_runner_config_content_changes() {
        let ct = ctoken();
        let a = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        let b = driver()
            .parse(
                make_parse_request(&[(
                    "runner_config_content",
                    Value::String("export default { test: {} };".to_string()),
                )]),
                &ct,
            )
            .await
            .unwrap();
        assert_ne!(a.target_def.hash, b.target_def.hash);
    }

    /// Same requirement as `runner_config_content` above, for the tsconfig
    /// that shapes how a `paths` alias (`@/*`) resolves — see this module's
    /// doc note on `JS_TEST_FORMAT_VERSION = 3`.
    #[tokio::test]
    async fn parse_hash_changes_when_tsconfig_content_changes() {
        let ct = ctoken();
        let a = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        let b = driver()
            .parse(
                make_parse_request(&[(
                    "tsconfig_content",
                    Value::String(r#"{"compilerOptions":{"strict":true}}"#.to_string()),
                )]),
                &ct,
            )
            .await
            .unwrap();
        assert_ne!(a.target_def.hash, b.target_def.hash);
    }

    /// `runner_bin` is an absolute host path — must NOT affect the cache key.
    #[tokio::test]
    async fn parse_hash_unaffected_by_runner_bin_path_difference() {
        let ct = ctoken();
        let a = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        let b = driver()
            .parse(
                make_parse_request(&[(
                    "runner_bin",
                    Value::String("/home/someone/project/node_modules/.bin/vitest".to_string()),
                )]),
                &ct,
            )
            .await
            .unwrap();
        assert_eq!(
            a.target_def.hash, b.target_def.hash,
            "a different host runner path for the same effective toolchain must not bust the cache"
        );
    }

    #[tokio::test]
    async fn parse_no_outputs_and_caches_locally_and_remotely() {
        let ct = ctoken();
        let resp = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        assert!(resp.target_def.outputs.is_empty());
        assert!(resp.target_def.cache.enabled);
        assert!(resp.target_def.cache.remote_enabled);
    }

    fn make_parse_request_with_deps(deps: Vec<(&str, Vec<&str>)>) -> ParseRequest {
        let mut c = config(&[]);
        let deps_map: HashMap<String, Value> = deps
            .into_iter()
            .map(|(k, vs)| {
                (
                    k.to_string(),
                    Value::List(
                        vs.into_iter()
                            .map(|v| Value::String(v.to_string()))
                            .collect(),
                    ),
                )
            })
            .collect();
        c.insert("deps".to_string(), Value::Map(deps_map));
        ParseRequest {
            request_id: "test".to_string(),
            target_spec: Arc::new(TargetSpec {
                addr: Addr::new(
                    PkgBuf::from("packages/a"),
                    "js_test".to_string(),
                    Default::default(),
                ),
                driver: "js_test".to_string(),
                config: c,
                ..Default::default()
            }),
        }
    }

    #[tokio::test]
    async fn parse_deps_become_target_dep_inputs_ordered_by_group() {
        let ct = ctoken();
        let req = make_parse_request_with_deps(vec![
            (
                "external",
                vec!["//@heph/fs:file@f=packages/b/src/index.ts"],
            ),
            ("", vec!["//@heph/fs:file@f=packages/a/src/index.test.ts"]),
            ("runner_config", vec!["//@heph/fs:file@f=vitest.config.ts"]),
        ]);
        let resp = driver().parse(req, &ct).await.unwrap();
        let origin_ids: Vec<&str> = resp
            .target_def
            .inputs
            .iter()
            .map(|i| i.origin_id.as_str())
            .collect();
        // Sorted group order: "" < "external" < "runner_config".
        assert_eq!(
            origin_ids,
            vec!["dep||0", "dep|external|0", "dep|runner_config|0"]
        );
    }

    /// `"external"`/`"runner_config"`/`"tsconfig"` deps get staged read-only
    /// *and* per-file (a hardlink, not a directory symlink — see
    /// `READ_ONLY_DEP_GROUPS`'s doc for why exactly these three, and why
    /// per-file specifically); the test file's own first-party closure
    /// (`""`) and `"codegen"` must stay writable and unannotated, or a
    /// Vitest snapshot write (or a co-located `js_src` codegen output) would
    /// hit EACCES.
    #[tokio::test]
    async fn parse_marks_only_external_runner_config_and_tsconfig_deps_read_only_per_file() {
        let ct = ctoken();
        let req = make_parse_request_with_deps(vec![
            ("", vec!["//@heph/fs:file@f=packages/a/src/index.test.ts"]),
            (
                "external",
                vec!["//@heph/fs:file@f=packages/b/src/index.ts"],
            ),
            ("runner_config", vec!["//@heph/fs:file@f=vitest.config.ts"]),
            ("tsconfig", vec!["//@heph/fs:file@f=tsconfig.json"]),
            (
                "codegen",
                vec!["//@heph/query:query@e=label(js_src)&x=js"],
            ),
        ]);
        let resp = driver().parse(req, &ct).await.unwrap();
        let read_only_per_file_groups: Vec<&str> = resp
            .target_def
            .inputs
            .iter()
            .filter(|i| {
                i.annotations
                    .get(hdriver_support::stage::READ_ONLY_ANNOTATION)
                    .map(|v| v == "true")
                    .unwrap_or(false)
                    && i.annotations
                        .get(hdriver_support::stage::STAGE_PER_FILE_ANNOTATION)
                        .map(|v| v == "true")
                        .unwrap_or(false)
            })
            .map(|i| i.origin_id.split('|').nth(1).unwrap_or(""))
            .collect();
        assert_eq!(
            read_only_per_file_groups,
            vec!["external", "runner_config", "tsconfig"]
        );
        let unannotated_groups: Vec<&str> = resp
            .target_def
            .inputs
            .iter()
            .filter(|i| i.annotations.is_empty())
            .map(|i| i.origin_id.split('|').nth(1).unwrap_or(""))
            .collect();
        assert_eq!(unannotated_groups, vec!["", "codegen"]);
    }

    // ---- run(): gated on a real vitest/jest binary being available in this devenv ----
    //
    // Everything above tests cache-key/Input-declaration behavior and needs no
    // real test runner. These exercise the actual subprocess invocation and
    // its success/failure surfacing (task requirement 5) — they require a
    // real `vitest`/`jest` (checked via PATH / `node_modules/.bin` convention)
    // and are #[ignore]d, with an honest message, when neither is present
    // rather than silently skipping.

    fn find_real_bin(name: &str) -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            let cand = dir.join(name);
            if std::fs::metadata(&cand)
                .map(|m| m.is_file())
                .unwrap_or(false)
            {
                return Some(cand);
            }
        }
        None
    }

    fn write(dir: &std::path::Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(path, contents).expect("write fixture file");
    }

    fn make_run_request<'a>(
        target: &'a TargetDef,
        request_id: &'a String,
        ws_dir: std::path::PathBuf,
        pkg_dir: std::path::PathBuf,
        hashin: &'a str,
    ) -> ManagedRunRequest<'a, 'a> {
        use hplugin::driver::{RunInput, RunRequest};
        ManagedRunRequest {
            request: RunRequest {
                request_id,
                target,
                tree_root_path: ws_dir.clone(),
                inputs: Vec::<RunInput>::new(),
                hashin,
                stdin: None,
                stdout: None,
                stderr: None,
                sandbox_dir: ws_dir.clone(),
            },
            sandbox_dir: ws_dir.clone(),
            sandbox_ws_dir: ws_dir,
            sandbox_pkg_dir: pkg_dir,
            inputs: Vec::new(),
        }
    }

    /// Confirmed live: with `cwd = sandbox_ws_dir` (the previous choice), a
    /// plugin loaded by the runner config that does its own ambient,
    /// `process.cwd()`-relative config discovery (`@lingui/vite-plugin` via
    /// `@lingui/conf`'s `getConfig()`) could never find a config file living
    /// in the package's own directory — that search only ever walks
    /// *ancestor* directories, and the package dir is a *descendant* of the
    /// workspace root, never an ancestor. No real `vitest`/`jest` needed:
    /// `runner_bin` just has to be an executable that reports its own `cwd`
    /// — a fake binary that always exits non-zero, whose stderr this
    /// driver's own failure-detail path already surfaces.
    #[tokio::test]
    async fn run_uses_the_packages_own_directory_as_cwd_not_the_workspace_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake_runner = dir.path().join("fake-runner.sh");
        std::fs::write(&fake_runner, "#!/bin/sh\necho \"CWD=$PWD\" >&2\nexit 1\n")
            .expect("write fake runner");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake_runner)
                .expect("metadata")
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake_runner, perms).expect("chmod");
        }

        let ct = ctoken();
        let req = make_parse_request(&[(
            "runner_bin",
            Value::String(fake_runner.to_string_lossy().into_owned()),
        )]);
        let parsed = driver().parse(req, &ct).await.unwrap();

        let request_id = "test".to_string();
        let pkg_dir = dir.path().join("packages/a");
        std::fs::create_dir_all(&pkg_dir).expect("create pkg dir");
        let pkg_dir_canonical = pkg_dir.canonicalize().expect("canonicalize pkg dir");
        let run_req = make_run_request(
            &parsed.target_def,
            &request_id,
            dir.path().to_path_buf(),
            pkg_dir,
            "deadbeef",
        );
        let err = driver()
            .run(run_req, &ct)
            .await
            .err()
            .expect("the fake runner always exits non-zero");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&format!("CWD={}", pkg_dir_canonical.display())),
            "expected the runner's own cwd to be the package directory, not the workspace root: \
             {msg}"
        );
    }

    /// Even with no live terminal at all (`stdout`/`stderr` both `None` —
    /// the common CI/batch shape, per `run()`'s doc comment), the runner's
    /// merged output must still land on disk in `log.txt` so it is
    /// inspectable by hand without `--shell` (see `RUNNER_LOG_FILE_NAME`'s
    /// doc for the scope of that — this file is not yet wired into the
    /// engine's own failure-box renderer).
    #[tokio::test]
    async fn run_writes_the_runners_merged_output_to_log_txt_in_the_sandbox() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake_runner = write_fake_runner(
            dir.path(),
            "fake-vitest.sh",
            "echo 'stdout line' \necho 'stderr line' >&2\nexit 1",
        );

        let ct = ctoken();
        let req = make_parse_request(&[(
            "runner_bin",
            Value::String(fake_runner.to_string_lossy().into_owned()),
        )]);
        let parsed = driver().parse(req, &ct).await.unwrap();

        let request_id = "test".to_string();
        let pkg_dir = dir.path().join("packages/a");
        std::fs::create_dir_all(&pkg_dir).expect("create pkg dir");
        let run_req = make_run_request(
            &parsed.target_def,
            &request_id,
            dir.path().to_path_buf(),
            pkg_dir,
            "deadbeef",
        );

        driver()
            .run(run_req, &ct)
            .await
            .err()
            .expect("the fake runner always exits non-zero");

        let log = std::fs::read_to_string(dir.path().join("log.txt")).expect("read log.txt");
        assert!(
            log.contains("stdout line") && log.contains("stderr line"),
            "log.txt must carry both streams even when no live sink was wired: {log:?}"
        );
    }

    #[tokio::test]
    async fn run_sets_ci_1_in_the_runners_environment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake_runner = dir.path().join("fake-runner.sh");
        std::fs::write(&fake_runner, "#!/bin/sh\necho \"CI=$CI\" >&2\nexit 1\n")
            .expect("write fake runner");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake_runner)
                .expect("metadata")
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake_runner, perms).expect("chmod");
        }

        let ct = ctoken();
        let req = make_parse_request(&[(
            "runner_bin",
            Value::String(fake_runner.to_string_lossy().into_owned()),
        )]);
        let parsed = driver().parse(req, &ct).await.unwrap();

        let request_id = "test".to_string();
        let pkg_dir = dir.path().join("packages/a");
        std::fs::create_dir_all(&pkg_dir).expect("create pkg dir");
        let run_req = make_run_request(
            &parsed.target_def,
            &request_id,
            dir.path().to_path_buf(),
            pkg_dir,
            "deadbeef",
        );
        let err = driver()
            .run(run_req, &ct)
            .await
            .err()
            .expect("the fake runner always exits non-zero");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("CI=1"),
            "the test runner must see CI=1 in its environment — heph's execution model is \
             headless by construction, and vitest/jest both gate interactive behavior beyond \
             just watch-mode selection on isCI/isTTY checks: {msg}"
        );
    }

    /// Reproduces the real failure mode this session's report described:
    /// vitest finishing its work but never exiting on its own. Tests
    /// `DeadlineCancellable` directly against a real subprocess that
    /// actively ignores `SIGINT` (`trap '' INT`) and sleeps far past any
    /// reasonable test budget — exactly the shape of a wedged child that
    /// won't respond to the plain interrupt `proc_exec`'s cancellation
    /// already sends. Bypasses `exec_runner`'s real (20-minute)
    /// `RUNNER_TIMEOUT` on purpose — this asserts the *mechanism* works,
    /// not that the production constant is short. The whole test is
    /// wrapped in its own outer deadline so a regression here fails loudly
    /// instead of hanging the suite.
    #[tokio::test]
    async fn deadline_cancellable_kills_a_child_that_ignores_sigint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("ignores-sigint.sh");
        std::fs::write(&script, "#!/bin/sh\ntrap '' INT\nsleep 100\n").expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).expect("metadata").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).expect("chmod");
        }

        let spec = proc_exec::Spec {
            program: script,
            args: vec![],
            env: vec![],
            cwd: dir.path().to_path_buf(),
            stdin: proc_exec::StdioSpec::Null,
            stdout: proc_exec::StdioSpec::Piped,
            stderr: proc_exec::StdioSpec::Piped,
            setsid: false,
            ctty: false,
        };
        let bounded = DeadlineCancellable {
            inner: ctoken().clone_arc(),
            deadline: tokio::time::Instant::now() + std::time::Duration::from_millis(200),
        };

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            proc_exec::output(spec, &bounded),
        )
        .await;

        let inner = result.expect(
            "DeadlineCancellable must make proc_exec::output return well within the test's own \
             10s outer budget (200ms deadline + proc_exec's ~2s SIGINT grace) — a hang here \
             means the deadline-kill path itself is broken",
        );
        assert!(
            inner.is_err(),
            "a child killed via the deadline must surface as an error, not a normal exit: \
             {inner:?}"
        );
        assert!(
            bounded.is_cancelled(),
            "the deadline must have been observed as elapsed"
        );
    }

    /// Same shape as [`make_run_request`], but with `stderr` wired to a
    /// caller-supplied live sink — what the engine hands the single named
    /// target of an interactive run (see `run()`'s doc comment on why this
    /// is unconditional on `--shell`).
    fn make_run_request_with_stderr<'a>(
        target: &'a TargetDef,
        request_id: &'a String,
        ws_dir: std::path::PathBuf,
        pkg_dir: std::path::PathBuf,
        hashin: &'a str,
        stderr: &'a mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin),
    ) -> ManagedRunRequest<'a, 'a> {
        use hplugin::driver::{RunInput, RunRequest};
        ManagedRunRequest {
            request: RunRequest {
                request_id,
                target,
                tree_root_path: ws_dir.clone(),
                inputs: Vec::<RunInput>::new(),
                hashin,
                stdin: None,
                stdout: None,
                stderr: Some(stderr),
                sandbox_dir: ws_dir.clone(),
            },
            sandbox_dir: ws_dir.clone(),
            sandbox_ws_dir: ws_dir,
            sandbox_pkg_dir: pkg_dir,
            inputs: Vec::new(),
        }
    }

    /// The actual complaint this mechanism exists to fix: a real user could
    /// not see *why* a run was stuck without `heph run --shell`, because
    /// `exec_runner` used to only ever report captured output once it
    /// returned — and a wedged runner (the vitest/esbuild dependency-scan
    /// case) may not return for a very long time. The fix is a live tee, not
    /// faster detection: what the runner already printed must reach the
    /// caller's sink as it's printed, independent of whether — or when —
    /// the process itself ever finishes.
    ///
    /// Proven by racing the read side against the runner's own sleep, not by
    /// asserting on `run()`'s total elapsed time (that would only show the
    /// eventual JSON-file detection still works, which other tests already
    /// cover): the diagnostic must arrive well before the sleep — which
    /// stands in for a hang of arbitrary length — elapses, not merely
    /// before `run()` itself returns.
    ///
    /// Deliberately NOT the real dependency-scan diagnostic text — that has
    /// its own fast-kill path now (see `DEPENDENCY_SCAN_HANG_MARKER` /
    /// `run_fails_fast_on_the_dependency_scan_hang_marker_instead_of_the_20_minute_timeout`)
    /// and this test wants a hang the tee has to survive for its own
    /// (unrelated) reason, not one heph now recognizes and kills early.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_streams_output_live_to_the_callers_sink_not_only_on_completion() {
        use tokio::io::AsyncReadExt as _;

        const MARKER: &str = "some unrelated diagnostic: cannot resolve @/gqlv3";
        const HANG_SECS: u64 = 5;

        let dir = tempfile::tempdir().expect("tempdir");
        let fake_runner = write_fake_runner(
            dir.path(),
            "fake-vitest.sh",
            &format!(
                "echo '{MARKER}' >&2\nsleep {HANG_SECS}\necho '{{\"success\":true}}' > \
                 .heph-js-test-result.json\n"
            ),
        );

        let ct = ctoken();
        let req = make_parse_request(&[(
            "runner_bin",
            Value::String(fake_runner.to_string_lossy().into_owned()),
        )]);
        let parsed = driver().parse(req, &ct).await.unwrap();

        let request_id = "test".to_string();
        let pkg_dir = dir.path().join("packages/a");
        std::fs::create_dir_all(&pkg_dir).expect("create pkg dir");

        let (mut read_half, mut write_half) = tokio::io::duplex(4096);
        let run_req = make_run_request_with_stderr(
            &parsed.target_def,
            &request_id,
            dir.path().to_path_buf(),
            pkg_dir,
            "deadbeef",
            &mut write_half,
        );

        let started = std::time::Instant::now();
        // Spawned onto its own task, not raced via `select!`/`join!` in
        // `run()`'s own task: `OutputReader::recv`'s macOS backend parks its
        // caller in `block_in_place`, which blocks the whole task it runs
        // in — including any other future joined into that same task. A
        // `join!` here would starve this reader for exactly the reason
        // `exec_runner`'s own `poll_task` had to move to a separate spawned
        // task (see module docs). Only `read_half` needs to be `'static`
        // here, so the spawn is straightforward.
        let read_task = tokio::spawn(async move {
            let mut seen = Vec::new();
            let mut buf = [0u8; 256];
            loop {
                let n = read_half.read(&mut buf).await.expect("read live sink");
                assert_ne!(
                    n, 0,
                    "sink closed (runner exited) before the marker ever arrived"
                );
                seen.extend_from_slice(&buf[..n]);
                if String::from_utf8_lossy(&seen).contains(MARKER) {
                    return started.elapsed();
                }
            }
        });

        driver()
            .run(run_req, &ct)
            .await
            .expect("the fake runner eventually reports success");

        let marker_at = tokio::time::timeout(std::time::Duration::from_secs(1), read_task)
            .await
            .expect("marker must have already arrived well before run() itself returned")
            .expect("read task panicked");
        assert!(
            marker_at < std::time::Duration::from_secs(HANG_SECS - 1),
            "the marker took {marker_at:?} to arrive — it must reach the live sink almost \
             immediately after the runner prints it, not only once the {HANG_SECS}s sleep (a \
             stand-in for an indefinite hang) finishes"
        );
    }

    /// Writes a fake runner script into `dir` that unconditionally drops
    /// `body` at its top, ignoring whatever CLI args `run()` actually built
    /// (it never reads `$@`) — matching how every other fake-runner test in
    /// this module already stands in for vitest/jest.
    fn write_fake_runner(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        let script = dir.join(name);
        std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).expect("write fake runner");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).expect("metadata").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).expect("chmod");
        }
        script
    }

    /// The bug report this mechanism exists to fix, reproduced exactly:
    /// the runner finishes and reports success, then hangs (here: ignores
    /// `SIGINT` and sleeps far past any reasonable budget, mirroring the
    /// real "esbuild service left alive" trigger). `RUNNER_TIMEOUT` is 20
    /// minutes — this asserts the target still completes in well under a
    /// second past `CANCEL_GRACE`, proving the JSON-file detection is what
    /// ended it, not the timeout backstop.
    ///
    /// `flavor = "multi_thread"`, not the default current-thread flavor:
    /// production always runs multi-threaded (`bootstrap.rs`), and only
    /// under that flavor does `OutputReader::recv`'s macOS backend take the
    /// `block_in_place` path this mechanism's poll task must stay
    /// responsive through — the same reason `proc_exec`'s own tests opt
    /// into it wherever the macOS backend's behavior matters.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_ends_promptly_once_the_result_file_appears_even_if_the_process_then_hangs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake_runner = write_fake_runner(
            dir.path(),
            "fake-vitest.sh",
            "echo '{\"success\":true}' > .heph-js-test-result.json\ntrap '' INT\nsleep 100",
        );

        let ct = ctoken();
        let req = make_parse_request(&[(
            "runner_bin",
            Value::String(fake_runner.to_string_lossy().into_owned()),
        )]);
        let parsed = driver().parse(req, &ct).await.unwrap();

        let request_id = "test".to_string();
        let pkg_dir = dir.path().join("packages/a");
        std::fs::create_dir_all(&pkg_dir).expect("create pkg dir");
        let run_req = make_run_request(
            &parsed.target_def,
            &request_id,
            dir.path().to_path_buf(),
            pkg_dir,
            "deadbeef",
        );

        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            driver().run(run_req, &ct),
        )
        .await
        .expect(
            "run() must return well within 15s — a hang here means completion is still \
                 keyed off the process's own exit, not the result file",
        );
        let elapsed = started.elapsed();

        result.expect(
            "a runner reporting success=true must succeed, whatever the process does afterward",
        );
        assert!(
            elapsed < std::time::Duration::from_secs(8),
            "run() took {elapsed:?} — expected roughly one poll interval plus the ~2s SIGINT \
             grace, nowhere near RUNNER_TIMEOUT",
        );
    }

    /// The real bug this mechanism actually shipped with, caught by CI and
    /// by a real-world report — not by any test written before this one.
    /// This driver always spawns with `setsid: false` (matching a real
    /// vitest/jest invocation), so `killpg` can never reach a descendant
    /// the runner backgrounds that inherits its stdout/stderr — exactly
    /// what vitest's own dependency-optimizer esbuild service does. The
    /// first version of this fix drained stdout/stderr in a plain unbounded
    /// loop; once the direct child exited (or was killed), a backgrounded
    /// descendant still holding the pipe's write end open made the drain
    /// wait for an EOF that was never coming — trading the original
    /// "process never exits" hang for the identical symptom reached through
    /// the new mechanism instead of the old one. This reproduces that shape
    /// directly: the runner writes the result file, backgrounds a
    /// long-lived descendant that inherits the pipe, and exits normally.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_ends_promptly_when_a_backgrounded_descendant_still_holds_the_pipe_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake_runner = write_fake_runner(
            dir.path(),
            "fake-vitest.sh",
            "echo '{\"success\":true}' > .heph-js-test-result.json\n( sleep 30 ) &\nexit 0",
        );

        let ct = ctoken();
        let req = make_parse_request(&[(
            "runner_bin",
            Value::String(fake_runner.to_string_lossy().into_owned()),
        )]);
        let parsed = driver().parse(req, &ct).await.unwrap();

        let request_id = "test".to_string();
        let pkg_dir = dir.path().join("packages/a");
        std::fs::create_dir_all(&pkg_dir).expect("create pkg dir");
        let run_req = make_run_request(
            &parsed.target_def,
            &request_id,
            dir.path().to_path_buf(),
            pkg_dir,
            "deadbeef",
        );

        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            driver().run(run_req, &ct),
        )
        .await
        .expect(
            "run() must return well within 15s — a hang here means the drain is waiting on a \
             backgrounded descendant's pipe instead of being bounded by DRAIN_DEADLINE",
        );
        let elapsed = started.elapsed();

        result.expect(
            "a runner reporting success=true must succeed regardless of a lingering descendant",
        );
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "run() took {elapsed:?} — the drain must be bounded by DRAIN_DEADLINE (500ms), not \
             wait on the backgrounded sleep",
        );
    }

    /// The real-world trigger this whole mechanism exists for (see module
    /// docs' "Surfacing output live, not just on completion" and
    /// `DEPENDENCY_SCAN_HANG_MARKER`): a failed Vite dependency-optimizer
    /// scan leaves an internal promise permanently unresolved, so the
    /// runner never writes [`RESULT_FILE_NAME`] and never exits — nothing
    /// short of `RUNNER_TIMEOUT` (20 minutes) would otherwise end this run.
    /// `trap '' INT` makes the fake runner ignore the initial SIGINT, so a
    /// pass here proves the full SIGINT → grace → SIGKILL path actually
    /// runs off the marker detection, not just that the *drain* stopped
    /// early.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_fails_fast_on_the_dependency_scan_hang_marker_instead_of_the_20_minute_timeout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake_runner = write_fake_runner(
            dir.path(),
            "fake-vitest.sh",
            "trap '' INT\necho '(!) Failed to run dependency scan. Skipping dependency \
             pre-bundling. Error: cannot resolve @/gqlv3' >&2\nsleep 100",
        );

        let ct = ctoken();
        let req = make_parse_request(&[(
            "runner_bin",
            Value::String(fake_runner.to_string_lossy().into_owned()),
        )]);
        let parsed = driver().parse(req, &ct).await.unwrap();

        let request_id = "test".to_string();
        let pkg_dir = dir.path().join("packages/a");
        std::fs::create_dir_all(&pkg_dir).expect("create pkg dir");
        let run_req = make_run_request(
            &parsed.target_def,
            &request_id,
            dir.path().to_path_buf(),
            pkg_dir,
            "deadbeef",
        );

        let started = std::time::Instant::now();
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            driver().run(run_req, &ct),
        )
        .await
        .expect(
            "run() must return well within 15s — a hang here means the marker was never \
             detected and this fell through to RUNNER_TIMEOUT",
        )
        .err()
        .expect("a dependency-scan hang must be reported as a failure, not silently succeed");
        let elapsed = started.elapsed();

        assert!(
            elapsed < DEPENDENCY_SCAN_HANG_GRACE + std::time::Duration::from_secs(5),
            "run() took {elapsed:?} — expected roughly DEPENDENCY_SCAN_HANG_GRACE plus the ~2s \
             SIGINT grace, nowhere near the 100s the fake runner sleeps for or RUNNER_TIMEOUT",
        );
        let msg = format!("{err:#}");
        assert!(
            msg.contains("vite#22934") && msg.contains("cannot resolve @/gqlv3"),
            "expected the specific known-bug explanation plus the runner's own captured output, \
             got: {msg}"
        );
    }

    /// A code-quality review flagged the real risk `DEPENDENCY_SCAN_HANG_MARKER`
    /// introduces: it is a substring match over unstructured output, so a
    /// project's own unrelated test/log text could coincidentally contain
    /// it (even with the `(!) ` anchor: a project could be asserting on that
    /// exact string, or piping through a tool that reproduces it verbatim).
    /// `DEPENDENCY_SCAN_HANG_GRACE` is the mitigation — the marker alone is
    /// never immediately fatal; a real result file discovered within the
    /// grace window still wins the race (checked first, every poll tick).
    /// This fake runner prints the marker text and then legitimately
    /// finishes and reports success, well inside `DEPENDENCY_SCAN_HANG_GRACE`
    /// — proving that race is won by the result file, not by the marker.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_lets_a_coincidental_marker_match_finish_normally_within_the_grace_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake_runner = write_fake_runner(
            dir.path(),
            "fake-vitest.sh",
            "echo '(!) Failed to run dependency scan: unrelated to this run, just the same \
             wording' >&2\necho '{\"success\":true}' > .heph-js-test-result.json",
        );

        let ct = ctoken();
        let req = make_parse_request(&[(
            "runner_bin",
            Value::String(fake_runner.to_string_lossy().into_owned()),
        )]);
        let parsed = driver().parse(req, &ct).await.unwrap();

        let request_id = "test".to_string();
        let pkg_dir = dir.path().join("packages/a");
        std::fs::create_dir_all(&pkg_dir).expect("create pkg dir");
        let run_req = make_run_request(
            &parsed.target_def,
            &request_id,
            dir.path().to_path_buf(),
            pkg_dir,
            "deadbeef",
        );

        tokio::time::timeout(DEPENDENCY_SCAN_HANG_GRACE, driver().run(run_req, &ct))
            .await
            .expect(
                "run() must succeed well within DEPENDENCY_SCAN_HANG_GRACE — a hang/timeout here \
             means a coincidental marker match was treated as fatal instead of losing the race \
             to the result file the runner actually wrote",
            )
            .expect(
                "success=true plus a coincidental marker-text match must still succeed, not be \
             killed as a false dependency-scan hang",
            );
    }

    /// Mirror of the success case: the runner reports `success: false` via
    /// its structured result and then hangs. The failure and its detail
    /// must come from that JSON, not from a generic "did not exit" timeout
    /// message the user already rejected as insufficient.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_reports_the_json_failure_message_even_though_the_process_then_hangs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake_runner = write_fake_runner(
            dir.path(),
            "fake-vitest.sh",
            "cat > .heph-js-test-result.json <<'EOF'\n\
             {\"success\":false,\"testResults\":[{\"message\":\"AssertionError: expected 1 to be 2\"}]}\n\
             EOF\ntrap '' INT\nsleep 100",
        );

        let ct = ctoken();
        let req = make_parse_request(&[(
            "runner_bin",
            Value::String(fake_runner.to_string_lossy().into_owned()),
        )]);
        let parsed = driver().parse(req, &ct).await.unwrap();

        let request_id = "test".to_string();
        let pkg_dir = dir.path().join("packages/a");
        std::fs::create_dir_all(&pkg_dir).expect("create pkg dir");
        let run_req = make_run_request(
            &parsed.target_def,
            &request_id,
            dir.path().to_path_buf(),
            pkg_dir,
            "deadbeef",
        );

        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            driver().run(run_req, &ct),
        )
        .await
        .expect("run() must return well within 15s");
        let elapsed = started.elapsed();

        let err = result
            .err()
            .expect("success=false in the structured result must fail the target");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("AssertionError: expected 1 to be 2"),
            "expected the runner's own structured failure message, not a generic timeout: {msg}"
        );
        assert!(!msg.contains("did not exit within"), "{msg}");
        assert!(
            elapsed < std::time::Duration::from_secs(8),
            "run() took {elapsed:?} — expected roughly one poll interval plus the ~2s SIGINT \
             grace, nowhere near RUNNER_TIMEOUT",
        );
    }

    /// If the assumption behind `RunnerJsonResult`'s shape is ever wrong —
    /// a runner version whose `--json`/`--reporter=json` output doesn't
    /// carry `success` where expected — this must surface as a fast, named
    /// diagnostic, never a silent mis-report of pass as fail (or the
    /// reverse) and never another 20-minute wait.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_reports_a_malformed_result_file_diagnosably_rather_than_hanging() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake_runner = write_fake_runner(
            dir.path(),
            "fake-vitest.sh",
            "echo '{\"unexpected\":\"shape\"}' > .heph-js-test-result.json\ntrap '' INT\nsleep 100",
        );

        let ct = ctoken();
        let req = make_parse_request(&[(
            "runner_bin",
            Value::String(fake_runner.to_string_lossy().into_owned()),
        )]);
        let parsed = driver().parse(req, &ct).await.unwrap();

        let request_id = "test".to_string();
        let pkg_dir = dir.path().join("packages/a");
        std::fs::create_dir_all(&pkg_dir).expect("create pkg dir");
        let run_req = make_run_request(
            &parsed.target_def,
            &request_id,
            dir.path().to_path_buf(),
            pkg_dir,
            "deadbeef",
        );

        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            driver().run(run_req, &ct),
        )
        .await
        .expect("run() must return well within 15s even for a malformed result file");
        let elapsed = started.elapsed();

        let err = result
            .err()
            .expect("a result file with no `success` field must fail the target, not pass it");
        let msg = format!("{err:#}");
        assert!(msg.contains("expected shape"), "{msg}");
        assert!(
            elapsed < std::time::Duration::from_secs(8),
            "run() took {elapsed:?} — a malformed result file must still end the run promptly",
        );
    }

    /// The headline claim of this whole mechanism — "the verdict comes from
    /// the JSON, not the process's exit code" — proven for the *ordinary*
    /// case: a runner that writes the result file and then exits
    /// immediately (no hang), with a `success: false` verdict directly
    /// contradicting its own `exit 0`. Every other test in this module has
    /// the runner hang after writing the file, which only exercises the
    /// in-loop `poll.tick()` detection branch — this one exercises the
    /// separate post-EOF fallback check, the path an ordinary, well-behaved
    /// run actually takes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_uses_the_json_verdict_over_a_contradicting_zero_exit_code() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake_runner = write_fake_runner(
            dir.path(),
            "fake-vitest.sh",
            "echo '{\"success\":false,\"testResults\":[{\"message\":\"boom\"}]}' > \
             .heph-js-test-result.json\nexit 0",
        );

        let ct = ctoken();
        let req = make_parse_request(&[(
            "runner_bin",
            Value::String(fake_runner.to_string_lossy().into_owned()),
        )]);
        let parsed = driver().parse(req, &ct).await.unwrap();

        let request_id = "test".to_string();
        let pkg_dir = dir.path().join("packages/a");
        std::fs::create_dir_all(&pkg_dir).expect("create pkg dir");
        let run_req = make_run_request(
            &parsed.target_def,
            &request_id,
            dir.path().to_path_buf(),
            pkg_dir,
            "deadbeef",
        );

        let err = driver().run(run_req, &ct).await.err().expect(
            "success=false in the JSON must fail the target even though the process exited 0",
        );
        assert!(format!("{err:#}").contains("boom"));
    }

    /// The mirror case: `success: true` in the JSON, but the process itself
    /// exits non-zero (e.g. a runner whose own exit code is unreliable, or
    /// a post-report crash unrelated to the tests themselves). The target
    /// must still succeed — the JSON, once present, is authoritative in
    /// both directions.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_uses_the_json_verdict_over_a_contradicting_nonzero_exit_code() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake_runner = write_fake_runner(
            dir.path(),
            "fake-vitest.sh",
            "echo '{\"success\":true}' > .heph-js-test-result.json\nexit 1",
        );

        let ct = ctoken();
        let req = make_parse_request(&[(
            "runner_bin",
            Value::String(fake_runner.to_string_lossy().into_owned()),
        )]);
        let parsed = driver().parse(req, &ct).await.unwrap();

        let request_id = "test".to_string();
        let pkg_dir = dir.path().join("packages/a");
        std::fs::create_dir_all(&pkg_dir).expect("create pkg dir");
        let run_req = make_run_request(
            &parsed.target_def,
            &request_id,
            dir.path().to_path_buf(),
            pkg_dir,
            "deadbeef",
        );

        driver().run(run_req, &ct).await.expect(
            "success=true in the JSON must pass the target even though the process exited \
             non-zero",
        );
    }

    /// A stale result file left behind by an earlier attempt in a reused
    /// sandbox path must never be mistaken for the *current* run's
    /// verdict — the whole reason `exec_runner` removes it up front. This
    /// pre-seeds exactly that file with the opposite verdict from what the
    /// current (slower) runner will actually report, so the test would
    /// spuriously pass almost instantly if the stale-file guard were ever
    /// removed or the result path miscomputed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_ignores_a_stale_result_file_left_by_a_previous_invocation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake_runner = write_fake_runner(dir.path(), "fake-vitest.sh", "sleep 0.3\nexit 1");

        let ct = ctoken();
        let req = make_parse_request(&[(
            "runner_bin",
            Value::String(fake_runner.to_string_lossy().into_owned()),
        )]);
        let parsed = driver().parse(req, &ct).await.unwrap();

        let request_id = "test".to_string();
        let pkg_dir = dir.path().join("packages/a");
        std::fs::create_dir_all(&pkg_dir).expect("create pkg dir");
        std::fs::write(pkg_dir.join(RESULT_FILE_NAME), "{\"success\":true}")
            .expect("seed stale result file");
        let run_req = make_run_request(
            &parsed.target_def,
            &request_id,
            dir.path().to_path_buf(),
            pkg_dir,
            "deadbeef",
        );

        let err = driver().run(run_req, &ct).await.err().expect(
            "a stale success=true file from a previous run must not mask this run's own failure",
        );
        assert!(
            format!("{err:#}").contains("js_test failed"),
            "expected the exit-code-based failure path (no fresh result file was written), got: \
             {err:#}"
        );
    }

    /// `run()`'s jest branch (`--json --outputFile=<path>` insertion) has
    /// its own argument-building code path, entirely separate from
    /// vitest's — every other test in this module defaults to vitest (see
    /// `config()`), so nothing else here proves jest's args are
    /// well-formed or that detection works for it at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_detects_completion_via_the_result_file_for_jest_too() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake_runner = write_fake_runner(
            dir.path(),
            "fake-jest.sh",
            "echo '{\"success\":true}' > .heph-js-test-result.json\ntrap '' INT\nsleep 100",
        );

        let ct = ctoken();
        let req = make_parse_request(&[
            ("testrunner", Value::String("jest".to_string())),
            (
                "runner_bin",
                Value::String(fake_runner.to_string_lossy().into_owned()),
            ),
        ]);
        let parsed = driver().parse(req, &ct).await.unwrap();

        let request_id = "test".to_string();
        let pkg_dir = dir.path().join("packages/a");
        std::fs::create_dir_all(&pkg_dir).expect("create pkg dir");
        let run_req = make_run_request(
            &parsed.target_def,
            &request_id,
            dir.path().to_path_buf(),
            pkg_dir,
            "deadbeef",
        );

        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            driver().run(run_req, &ct),
        )
        .await
        .expect("run() must return well within 15s for jest too");
        let elapsed = started.elapsed();

        result.expect("a jest runner reporting success=true must succeed");
        assert!(
            elapsed < std::time::Duration::from_secs(8),
            "run() took {elapsed:?} — jest's detection must be just as prompt as vitest's",
        );
    }

    #[tokio::test]
    #[ignore = "requires a real `vitest` on PATH — devenv.nix provisions no Node/vitest \
                toolchain (see toolchain.rs module docs); run explicitly with \
                `cargo test -- --ignored` on a host with vitest installed"]
    async fn run_succeeds_on_a_passing_test_file() {
        let vitest = find_real_bin("vitest").expect(
            "this test is #[ignore]d precisely because vitest isn't guaranteed on PATH — it was \
             run explicitly, so a missing vitest here is a real failure, not a skip",
        );

        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/src/index.test.ts",
            "import { expect, test } from 'vitest';\ntest('passes', () => { expect(1 + 1).toBe(2); });\n",
        );

        let ct = ctoken();
        let req = make_parse_request(&[(
            "runner_bin",
            Value::String(vitest.to_string_lossy().into_owned()),
        )]);
        let parsed = driver().parse(req, &ct).await.unwrap();

        let request_id = "test".to_string();
        let run_req = make_run_request(
            &parsed.target_def,
            &request_id,
            dir.path().to_path_buf(),
            dir.path().join("packages/a"),
            "deadbeef",
        );
        driver()
            .run(run_req, &ct)
            .await
            .expect("a passing test file must succeed");
    }

    #[tokio::test]
    #[ignore = "requires a real `vitest` on PATH — devenv.nix provisions no Node/vitest \
                toolchain (see toolchain.rs module docs); run explicitly with \
                `cargo test -- --ignored` on a host with vitest installed"]
    async fn run_fails_and_surfaces_runner_output_on_a_real_failing_test() {
        let vitest = find_real_bin("vitest").expect(
            "this test is #[ignore]d precisely because vitest isn't guaranteed on PATH — it was \
             run explicitly, so a missing vitest here is a real failure, not a skip",
        );

        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/src/index.test.ts",
            "import { expect, test } from 'vitest';\ntest('fails', () => { expect(1 + 1).toBe(3); });\n",
        );

        let ct = ctoken();
        let req = make_parse_request(&[(
            "runner_bin",
            Value::String(vitest.to_string_lossy().into_owned()),
        )]);
        let parsed = driver().parse(req, &ct).await.unwrap();

        let request_id = "test".to_string();
        let run_req = make_run_request(
            &parsed.target_def,
            &request_id,
            dir.path().to_path_buf(),
            dir.path().join("packages/a"),
            "deadbeef",
        );
        let err = driver()
            .run(run_req, &ct)
            .await
            .err()
            .expect("a real failing test must fail the driver, not succeed silently");
        let msg = format!("{err:#}");
        assert!(msg.contains("js_test failed"), "{msg}");
    }

    #[test]
    fn test_failure_detail_keeps_the_leading_exception_past_a_long_stack_dump() {
        // Reproduces a real report: vitest's own error dump prints the
        // exception name/message first, then a JSON-serialized stack with
        // one multi-line frame per entry — easily 60+ lines on its own. A
        // tail-only cap silently dropped the name/message and left only
        // trailing frames, so the user saw no error at all, just orphaned
        // `file:`/`line:`/`column:` fields.
        let mut stderr = String::from("Error: Failed to resolve entry for package\n");
        for i in 0..80 {
            stderr.push_str(&format!(
                "    {{\n      method: 'frame{i}',\n      file: 'f{i}.js',\n      line: {i},\n      column: 3\n    }},\n"
            ));
        }
        let detail = test_failure_detail("", &stderr);
        assert!(
            detail.contains("Error: Failed to resolve entry for package"),
            "{detail}"
        );
        assert!(detail.contains("lines omitted"), "{detail}");
    }

    /// Reproduces a real report: the structured `testResults[].message` is a
    /// terse prefix ("Failed to import test file ...") of the full error
    /// vitest already printed to stderr (which additionally carries the
    /// `Caused by:` chain) — appending the raw tail as a "supplement" then
    /// printed the same failure twice, once terse and once in full.
    #[test]
    fn failure_detail_from_json_skips_raw_supplement_when_it_already_contains_the_message() {
        let result = RunnerJsonResult {
            success: false,
            test_results: vec![RunnerTestFileResult {
                message: "Failed to import test file /ws/browser.setup.tsx".to_string(),
            }],
        };
        let stderr = "Error: Failed to import test file /ws/browser.setup.tsx\n\
                       Caused by: TypeError: Failed to fetch dynamically imported module\n";
        let detail = failure_detail_from_json(&result, "", stderr);
        assert_eq!(
            detail
                .matches("Failed to import test file /ws/browser.setup.tsx")
                .count(),
            1,
            "{detail}"
        );
        assert!(detail.contains("Caused by"), "{detail}");
        assert!(!detail.contains("--- raw stdout/stderr ---"), "{detail}");
    }

    /// The opposite case: the raw stream carries a warning the structured
    /// message never mentions (e.g. printed outside any test result) — that
    /// must still reach the user, so the supplement is kept.
    #[test]
    fn failure_detail_from_json_keeps_raw_supplement_when_it_adds_new_information() {
        let result = RunnerJsonResult {
            success: false,
            test_results: vec![RunnerTestFileResult {
                message: "some test assertion failed".to_string(),
            }],
        };
        let stderr = "unrelated warning printed outside any test result\n";
        let detail = failure_detail_from_json(&result, "", stderr);
        assert!(detail.contains("some test assertion failed"), "{detail}");
        assert!(detail.contains("--- raw stdout/stderr ---"), "{detail}");
        assert!(
            detail.contains("unrelated warning printed outside any test result"),
            "{detail}"
        );
    }
}
