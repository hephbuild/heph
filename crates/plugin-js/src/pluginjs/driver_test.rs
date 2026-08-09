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
//! runner version. See `provider.rs`'s `test_deps_config` for the pure,
//! runner-free Input-scoping function this driver's `deps`/`test_file` config
//! comes from — deliberately split out the same way `typecheck_deps_config`
//! is, so the per-test-file scoping is unit-testable without a real
//! `vitest`/`jest` binary (this is the task's "single most important test in
//! this milestone").
//!
//! Beyond the declared `Input`s (content-hashed automatically by the
//! engine), [`JsTestDef::hash`] additionally hashes `test_file` itself (so
//! *which* test file this target is for is part of the key, not just its
//! transitive content — two different test files with byte-identical
//! closures must still be different cache entries), `runner_config_content`
//! directly (same deliberate redundancy with its declared Input that
//! `js_typecheck`'s `tsconfig_content` has — see that struct's doc), and
//! `runner_version`. `runner_bin` (an absolute host path) is deliberately
//! **excluded**, mirroring `tsc_bin`'s exclusion — see that field's doc in
//! `driver_typecheck.rs` for the identical cache-portability rationale.
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
//! one machine and fail on another with a byte-identical cache key.
//!
//! ## Failure reporting
//!
//! A failing test is a plain, non-zero-exit driver failure with both stdout
//! and stderr tails surfaced (`test_failure_detail`, identical shape to
//! `driver_typecheck.rs`'s `tsc_failure_detail`) — the runner's actual
//! output (assertion diffs, stack traces) must reach the user, never be
//! swallowed into a bare "failed" with no detail.

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
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

use crate::pluginjs::toolchain;

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
    /// docs' "Inputs / cache key" section.
    deps: HashMap<String, Vec<String>>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct JsTestDef {
    testrunner: String,
    runner_version: String,
    test_file: String,
    runner_config_path: String,
    runner_config_content: String,
    /// Absolute host runner path — carried through so `run()` can exec it,
    /// but see `Hash` impl below: deliberately excluded from the cache key.
    runner_bin: String,
}

/// Bump to invalidate every cached `js_test` result whenever the invocation
/// shape (flags, runner-config-lookup rule, what counts as a hashed config
/// field) changes in a way the declared `Input` content hash alone would not
/// already capture.
const JS_TEST_FORMAT_VERSION: u32 = 1;

impl Hash for JsTestDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        JS_TEST_FORMAT_VERSION.hash(state);
        self.testrunner.hash(state);
        self.runner_version.hash(state);
        self.test_file.hash(state);
        self.runner_config_path.hash(state);
        self.runner_config_content.hash(state);
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
            for (i, addr_str) in addrs.iter().enumerate() {
                inputs.push(Input {
                    r#ref: TargetAddr::parse(addr_str, &pkg)
                        .with_context(|| format!("parse dep addr {addr_str}"))?,
                    mode: InputMode::Standard,
                    origin_id: format!("dep|{group}|{i}"),
                    annotations: Default::default(),
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
        req: ManagedRunRequest<'a, 'io>,
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
        let mut env: HashMap<String, String> = HashMap::new();
        if let Ok(v) = std::env::var("PATH") {
            env.insert("PATH".to_string(), v);
        }
        env.insert("TZ".to_string(), "UTC".to_string());
        env.insert("LANG".to_string(), "C.UTF-8".to_string());

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

        self.exec_runner(&runner_bin, args, &env, &req.sandbox_pkg_dir, ctoken)
            .await?;

        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

impl JsTestDriver {
    async fn exec_runner(
        &self,
        runner_bin: &std::path::Path,
        args: Vec<OsString>,
        env: &HashMap<String, String>,
        cwd: &std::path::Path,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<()> {
        let env_pairs: Vec<(OsString, OsString)> = env
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect();
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
        let output = proc_exec::output(spec, ctoken)
            .await
            .with_context(|| format!("wait for test runner ({runner_bin:?})"))?;
        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "js_test failed ({}):\n{}",
                output.status,
                test_failure_detail(&stdout, &stderr)
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
}
