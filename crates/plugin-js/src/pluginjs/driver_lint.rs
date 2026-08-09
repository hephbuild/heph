//! `js_lint` — runs a configured linter over one package at a time.
//!
//! One driver name, tool selected by config (`linter = "oxlint"` default,
//! `"eslint"` alt) — matching the established naming rule this crate already
//! follows for `js_test`'s `testrunner` axis: never a driver-per-tool
//! (`js_lint_oxlint`/`js_lint_eslint`), see `ai-docs/js-plugin-plan.md`'s
//! tool-selection table.
//!
//! ## Granularity: per-package, for both tools
//!
//! Per the design doc's "Caching / incrementality" section: oxlint (a fast,
//! syntactic-rules-only linter, no cross-file type information) may be cheap
//! enough that per-file caching isn't worth the bookkeeping; eslint's
//! type-aware rules need the same tsconfig-derived type graph `js_typecheck`
//! needs, which only exists at package granularity. Rather than having
//! `linter` silently change what a `js_lint` addr even *means* (per-file for
//! one tool, per-package for the other — which would also make switching
//! `linter` a breaking addr-shape change for anything depending on a
//! specific `js_lint` target), this driver is per-package for **both**
//! tools, matching `js_typecheck`'s own granularity. This is the simpler,
//! uniform answer and the one the design doc's own reasoning already points
//! to for eslint; nothing about oxlint's speed argues *for* finer than
//! per-package here, only that per-file wouldn't have been prohibitively
//! expensive if chosen — so per-package is not just simpler but has no
//! displaced correctness or performance win being left on the table.
//!
//! ## Toolchain: `linter = host`, the same disclosed non-hermetic escape
//! ## hatch as `tstool`/`testrunner`
//!
//! No hermetic Node toolchain exists anywhere in this plugin yet.
//! `Provider::get` (see `provider.rs::lint_config`) resolves the configured
//! linter's binary from the host (`toolchain::resolve_host_linter`) and
//! threads its absolute path plus its queried `--version` string through
//! this target's config, queried once per `Provider` lifetime
//! (`Provider::linter_cache`) rather than once per package — the exact
//! per-call subprocess-spawn mistake the M4 review already caught once for
//! `tsc`/the test runner, not to be repeated a third time here.
//!
//! ## Inputs / cache key
//!
//! `""` = the package's own first-party source files (no tsconfig
//! `include`/`exclude` filtering — a linter reads raw source files directly,
//! unlike `tsc`'s project-scoped compilation); `"config"` = the resolved
//! linter config file, if any (`.oxlintrc.json` for oxlint; the modern flat
//! `eslint.config.*` or a legacy `.eslintrc.*` for eslint, walked up the
//! ancestor chain the same way `tsconfig.json`/test-runner configs are — see
//! `provider.rs`'s `lint_config_candidates`). Deliberately **no**
//! `package.json`-field fallback (`"oxlint"`/`"eslintConfig"`) — an earlier
//! version had one, but neither tool actually reads a `package.json` that
//! way when invoked with the `-c <path>` flag this driver always passes (a
//! feature-quality M5 review finding); see
//! `importgraph::find_nearest_package_json_field_config`'s doc.
//!
//! `"tsconfig"` (eslint type-aware rules only) = the tsconfig(s) named by
//! every `parserOptions.project` occurrence (a multi-entry flat config can
//! have more than one — every one is resolved, not just the first, per a
//! code-quality M5 review finding) plus each one's whole `extends` chain,
//! exactly the same Input/hash treatment `js_typecheck` gives its own
//! tsconfig — **this is the specific gap named by the M5 task**: an eslint
//! config with type-aware rules gets its type information from that tsconfig
//! the same way `tsc` does, so a change anywhere in it (or its `extends`
//! chain) must bust this target's cache the same way it busts
//! `js_typecheck`'s, and M3/M4 already caught this exact class of miss (an
//! under-declared dependency graph) for two other drivers; `"eslint_plugins"`
//! (eslint only) = every `extends`/`plugins` entry that names an npm
//! package, resolved through the lockfile (`deps::resolve_one_dependency`) —
//! never treated as a raw filesystem path, the same M3/M4-review-class
//! mistake named again here; `"config_refs"` (eslint only) = every
//! *relative-path* shared config file the leaf config's own `extends`/
//! `plugins` (legacy) or relative `import`/`require` (modern flat config)
//! names — see `importgraph::resolve_eslint_config_referenced_files`'s doc
//! for the hermeticity M5 review finding this closes (a shared base config
//! edited without touching the leaf config previously left the cache key
//! unchanged). See `provider.rs`'s `lint_deps_config` and its tests for the
//! input-scoping proof, mirroring `typecheck_deps_config`'s role for
//! `js_typecheck`.
//!
//! Beyond the declared `Input`s, [`JsLintDef::hash`] additionally hashes the
//! resolved config's own content and (for eslint type-aware rules) the
//! tsconfig chain's content directly, plus the queried `linter_version`
//! string — the same deliberate redundancy `JsTypecheckDef::hash` already
//! has with its own declared tsconfig Input, for the identical reason (the
//! explicit hash keeps this target's cache-sensitivity independently
//! verifiable at the `parse()` level without a full engine round-trip). Like
//! `tsc_bin`, `linter_bin` (an absolute host path) is deliberately
//! **excluded** — see that field's own doc comment.
//!
//! ## Failure reporting
//!
//! Both oxlint and eslint exit non-zero when they find lint errors (as
//! opposed to `go_lint`'s `-json`-always-exits-0 analyze/gate split, which
//! exists there to let facts propagate to dependents even when a package has
//! findings — `js_lint` has no such fact-propagation concern, mirroring
//! `js_typecheck`'s identical single-driver shape). A non-zero exit fails the
//! driver with both stdout and stderr tails included: which files, which
//! rules, and why is the whole point of a lint failure, so nothing here
//! swallows the linter's own output — see [`lint_failure_detail`].

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
use std::io::BufRead;
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

/// Config for a `js_lint` target. Entirely engine-generated by the `js`
/// provider's `Provider::get` (see `pluginjs::provider::Provider::lint_config`)
/// — never authored by hand in a BUILD file.
#[derive(Spec)]
struct JsLintSpec {
    /// Which linter this target runs — `"oxlint"` or `"eslint"`. Not itself
    /// used to pick the invocation shape at `run()` time beyond a `-c`
    /// config flag both tools share; carried through mainly so a driver-side
    /// bug surfaces as "ran the wrong linter" rather than silently assuming
    /// one.
    #[spec(required)]
    linter: String,
    /// Absolute host path to the resolved linter binary
    /// (`toolchain::resolve_host_linter`). Deliberately **not** part of
    /// `JsLintDef`'s hash — see that struct's `linter_bin` field doc.
    #[spec(required)]
    linter_bin: String,
    /// `<linter> --version`'s trimmed output, queried once by `Provider::get`.
    /// Hashed: a host linter upgrade/downgrade must bust the cache.
    #[spec(required)]
    linter_version: String,
    /// Workspace-root-relative path to the resolved linter config, if any.
    /// Empty when no config file was found anywhere on the ancestor chain
    /// (the linter then runs with its own built-in defaults).
    config_path: String,
    /// The resolved config's own raw bytes, hashed directly — see module
    /// docs' "Inputs / cache key" section for why this is not purely
    /// redundant with the `"config"` dep group below.
    config_content: String,
    /// (eslint type-aware rules only) workspace-root-relative path to the
    /// tsconfig named by `parserOptions.project` (the first one, when
    /// multiple are configured). Empty for oxlint, and for an eslint config
    /// with no type-aware rules configured.
    tsconfig_path: String,
    /// The resolved tsconfig's own raw bytes plus its whole `extends`
    /// chain's, concatenated — see module docs' "Inputs / cache key" section
    /// for why this is the specific gap the M5 task named.
    tsconfig_content: String,
    /// Dependencies, grouped by name → list of target addresses: `""` = the
    /// package's own first-party source files, `"config"` = the resolved
    /// linter config file (0 or 1 entries), `"tsconfig"` (eslint type-aware
    /// only) = the tsconfig(s) plus their `extends` chain, `"eslint_plugins"`
    /// (eslint only) = every `extends`/`plugins` npm package — see module
    /// docs' "Inputs / cache key" section.
    deps: HashMap<String, Vec<String>>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct JsLintDef {
    linter: String,
    linter_version: String,
    config_path: String,
    config_content: String,
    tsconfig_path: String,
    tsconfig_content: String,
    /// Absolute host linter path — carried through so `run()` can exec it,
    /// but see `Hash` impl below: deliberately excluded from the cache key.
    linter_bin: String,
}

/// Bump to invalidate every cached `js_lint` result whenever the invocation
/// shape (flags, config-lookup rule, what counts as a hashed config field)
/// changes in a way the declared `Input` content hash alone would not
/// already capture.
const JS_LINT_FORMAT_VERSION: u32 = 1;

impl Hash for JsLintDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        JS_LINT_FORMAT_VERSION.hash(state);
        self.linter.hash(state);
        self.linter_version.hash(state);
        self.config_path.hash(state);
        self.config_content.hash(state);
        self.tsconfig_path.hash(state);
        self.tsconfig_content.hash(state);
        // `linter_bin` is deliberately NOT hashed: it's an absolute host
        // filesystem path that differs across machines/checkouts for the
        // exact same effective toolchain — hashing it would needlessly
        // break cache portability (including remote-cache sharing) for a
        // toolchain that is otherwise identical. `linter_version` is the
        // actual cache-relevant signal, mirroring `JsTypecheckDef`'s
        // identical `tsc_bin` exclusion.
        //
        // Source/config file *content* arrives via declared `Input`s; the
        // engine hashes that separately (architecture.md's "Automatic
        // hashing"), so it is not duplicated here — same discipline as
        // `JsTypecheckDef`.
    }
}

pub struct JsLintDriver;

impl JsLintDriver {
    pub fn new() -> Self {
        Self
    }
}

impl Default for JsLintDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ManagedDriver for JsLintDriver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "js_lint".to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        JsLintSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let addr = &req.target_spec.addr;
        let pkg = addr.package.clone();
        let spec = JsLintSpec::from(&req.target_spec.config).context("parse js_lint config")?;

        // Dep groups arrive from a HashMap — sort by group name so the
        // resulting `inputs` is deterministic across parses, not
        // HashMap-iteration order. Same regression class as
        // `driver_typecheck.rs`/`driver_test.rs`.
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

        let def = JsLintDef {
            linter: spec.linter,
            linter_version: spec.linter_version,
            config_path: spec.config_path,
            config_content: spec.config_content,
            tsconfig_path: spec.tsconfig_path,
            tsconfig_content: spec.tsconfig_content,
            linter_bin: spec.linter_bin,
        };

        let hash = {
            let mut h =
                DebugHasher::new(Xxh3Default::new(), || format!("js_lint_{}", addr.format()));
            def.hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs,
                // No output artifact: a `js_lint` target's only observable
                // effect is "did it succeed" — mirrors `js_typecheck`'s
                // identical read-only, produces-nothing shape.
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
        let def = req.request.target.def_de::<JsLintDef>();
        let linter_bin = std::path::PathBuf::from(&def.linter_bin);

        let srcs = self.group_staged_paths(&req, "");
        if srcs.is_empty() {
            // No first-party source files staged. Unlike `js_test` (which
            // only ever lists a target for a file discovery actually found),
            // `js_lint` is listed unconditionally for every package with a
            // `package.json` (see `Provider::list`'s doc) — a very plausible
            // case: the workspace-root `package.json`, a types-only
            // re-export package, or a package mid-scaffolding. Hard-failing
            // here (an earlier version did) made every such package's
            // `js_lint` target permanently, unavoidably red with no way to
            // opt out — a feature-quality M5 review finding. Nothing to
            // lint, so succeed trivially, mirroring `js_typecheck`'s intent
            // (not its letter — that guard only fires in its no-tsconfig
            // fallback branch) of not hard-failing on an empty input set.
            return Ok(ManagedRunResponse { artifacts: vec![] });
        }

        // `linter = "host"`-equivalent non-hermetic escape hatch (see module
        // docs): the resolved binary is very likely a `#!/usr/bin/env node`
        // shebang script (eslint always is; oxlint ships a native binary but
        // its `node_modules/.bin` wrapper may still shell out), so executing
        // it needs `node` reachable via PATH — mirrors `js_typecheck`'s
        // identical PATH passthrough. Only `PATH` is forwarded.
        let mut env: HashMap<String, String> = HashMap::new();
        if let Ok(v) = std::env::var("PATH") {
            env.insert("PATH".to_string(), v);
        }

        let mut args: Vec<OsString> = Vec::new();
        if !def.config_path.is_empty() {
            let config_abs = req.sandbox_ws_dir.join(&def.config_path);
            args.push(OsString::from("-c"));
            args.push(config_abs.into_os_string());
        }
        args.extend(srcs.into_iter().map(OsString::from));

        // `cwd = sandbox_pkg_dir`, not `sandbox_ws_dir` — every path
        // argument above is already absolute (`config_abs`, and `srcs`,
        // both `sandbox_ws_dir.join(...)`/`group_staged_paths`-sourced), so
        // `cwd` only matters for the linter's own ambient,
        // `process.cwd()`-relative behavior (eslint's own config-cascade
        // walk, a plugin's ambient config discovery) — the package's own
        // directory is what a real, non-heph invocation runs with in
        // practice. See `driver_test.rs`'s module docs for the confirmed
        // live bug this mirrors the fix for.
        self.exec_linter(&linter_bin, args, &env, &req.sandbox_pkg_dir, ctoken)
            .await?;

        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

impl JsLintDriver {
    /// All staged file paths for the inputs in dep `group` (origin
    /// `dep|group|*`). Mirrors `driver_typecheck.rs`/`driver_test.rs`'s
    /// identically-named helper.
    fn group_staged_paths(&self, req: &ManagedRunRequest<'_, '_>, group: &str) -> Vec<String> {
        let prefix = format!("dep|{group}|");
        let mut out: Vec<String> = Vec::new();
        for m in &req.inputs {
            if !m.input.origin_id.starts_with(&prefix) {
                continue;
            }
            let Ok(list_path) = m.require_list_path() else {
                continue;
            };
            if let Ok(f) = std::fs::File::open(list_path) {
                for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
                    if !line.is_empty() {
                        out.push(line);
                    }
                }
            }
        }
        out
    }

    async fn exec_linter(
        &self,
        linter_bin: &std::path::Path,
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
            program: linter_bin.to_path_buf(),
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
            .with_context(|| format!("wait for linter ({linter_bin:?})"))?;
        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "lint failed ({}):\n{}",
                output.status,
                lint_failure_detail(&stdout, &stderr)
            );
        }
        Ok(())
    }
}

/// Build the human-readable detail for a failed lint invocation. Both oxlint
/// and eslint print findings to stdout by default; stderr is included too
/// (a config/fatal error commonly lands there instead) — mirrors
/// `driver_typecheck.rs`'s `tsc_failure_detail` for the identical reason: a
/// tool whose failure output lands on either stream depending on the failure
/// mode must never come back silently blank.
fn lint_failure_detail(stdout: &str, stderr: &str) -> String {
    let stdout_tail = hplugin::error::last_n_lines(stdout.trim(), 60);
    let stderr_tail = hplugin::error::last_n_lines(stderr.trim(), 40);
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

    fn driver() -> JsLintDriver {
        JsLintDriver::new()
    }

    fn ctoken() -> StdCancellationToken {
        StdCancellationToken::new()
    }

    fn config(extra: &[(&str, Value)]) -> HashMap<String, Value> {
        let mut c: HashMap<String, Value> = HashMap::from([
            ("linter".to_string(), Value::String("oxlint".to_string())),
            (
                "linter_bin".to_string(),
                Value::String("/usr/bin/oxlint".to_string()),
            ),
            (
                "linter_version".to_string(),
                Value::String("0.15.0".to_string()),
            ),
            (
                "config_path".to_string(),
                Value::String("packages/a/.oxlintrc.json".to_string()),
            ),
            (
                "config_content".to_string(),
                Value::String("{}".to_string()),
            ),
            ("tsconfig_path".to_string(), Value::String(String::new())),
            ("tsconfig_content".to_string(), Value::String(String::new())),
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
                    "js_lint".to_string(),
                    Default::default(),
                ),
                driver: "js_lint".to_string(),
                config: config(extra),
                ..Default::default()
            }),
        }
    }

    #[tokio::test]
    async fn driver_name_is_js_lint() {
        let resp = driver().config(ConfigRequest {}).unwrap();
        assert_eq!(resp.name, "js_lint");
    }

    #[tokio::test]
    async fn parse_missing_required_field_errors() {
        let ct = ctoken();
        let req = ParseRequest {
            request_id: "test".to_string(),
            target_spec: Arc::new(TargetSpec {
                addr: Addr::new(
                    PkgBuf::from("packages/a"),
                    "js_lint".to_string(),
                    Default::default(),
                ),
                driver: "js_lint".to_string(),
                ..Default::default()
            }),
        };
        assert!(driver().parse(req, &ct).await.is_err());
    }

    #[tokio::test]
    async fn parse_hash_stable_across_identical_parses() {
        let ct = ctoken();
        let a = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        let b = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        assert_eq!(a.target_def.hash, b.target_def.hash);
    }

    /// Task requirement: a linter *version* change must bust the cache.
    #[tokio::test]
    async fn parse_hash_changes_when_linter_version_changes() {
        let ct = ctoken();
        let a = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        let b = driver()
            .parse(
                make_parse_request(&[("linter_version", Value::String("0.16.0".to_string()))]),
                &ct,
            )
            .await
            .unwrap();
        assert_ne!(a.target_def.hash, b.target_def.hash);
    }

    /// Task requirement: the cache key changes when the resolved linter
    /// config changes, and is stable when it does not.
    #[tokio::test]
    async fn parse_hash_changes_when_config_content_changes_and_stable_when_not() {
        let ct = ctoken();
        let a = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        let b = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        assert_eq!(
            a.target_def.hash, b.target_def.hash,
            "identical config content must hash identically"
        );

        let c = driver()
            .parse(
                make_parse_request(&[(
                    "config_content",
                    Value::String(r#"{"rules":{"no-console":"error"}}"#.to_string()),
                )]),
                &ct,
            )
            .await
            .unwrap();
        assert_ne!(
            a.target_def.hash, c.target_def.hash,
            "a changed lint config content must change the cache key"
        );
    }

    /// Task requirement (the specific M5 gap): for eslint with type-aware
    /// rules configured, the cache key changes when the tsconfig extends
    /// chain changes — proxied here at the `parse()` level via
    /// `tsconfig_content` exactly the way `driver_typecheck.rs`'s own
    /// identical test proxies a tsconfig content change (the config-value's
    /// derivation from the real extends chain is proven separately in
    /// `provider.rs`'s `lint_deps_config` tests; this proves the *driver*
    /// half — a changed `tsconfig_content` value actually reaches the hash).
    #[tokio::test]
    async fn parse_hash_changes_when_eslint_tsconfig_content_changes() {
        let ct = ctoken();
        let base = &[
            ("linter", Value::String("eslint".to_string())),
            (
                "tsconfig_path",
                Value::String("packages/a/tsconfig.json".to_string()),
            ),
            (
                "tsconfig_content",
                Value::String(r#"{"compilerOptions":{}}"#.to_string()),
            ),
        ];
        let a = driver().parse(make_parse_request(base), &ct).await.unwrap();
        let b = driver()
            .parse(
                make_parse_request(&[
                    ("linter", Value::String("eslint".to_string())),
                    (
                        "tsconfig_path",
                        Value::String("packages/a/tsconfig.json".to_string()),
                    ),
                    (
                        // Simulates the extends chain changing (e.g. the
                        // base tsconfig it extends flipping `strict`),
                        // concatenated into `tsconfig_content` by
                        // `lint_deps_config` the same way
                        // `typecheck_deps_config` does for js_typecheck.
                        "tsconfig_content",
                        Value::String(
                            r#"{"compilerOptions":{}}\n{"compilerOptions":{"strict":true}}"#
                                .to_string(),
                        ),
                    ),
                ]),
                &ct,
            )
            .await
            .unwrap();
        assert_ne!(
            a.target_def.hash, b.target_def.hash,
            "an eslint type-aware tsconfig extends-chain content change must bust the cache"
        );
    }

    /// `linter_bin` is an absolute host path — must NOT affect the cache key.
    #[tokio::test]
    async fn parse_hash_unaffected_by_linter_bin_path_difference() {
        let ct = ctoken();
        let a = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        let b = driver()
            .parse(
                make_parse_request(&[(
                    "linter_bin",
                    Value::String("/home/someone/project/node_modules/.bin/oxlint".to_string()),
                )]),
                &ct,
            )
            .await
            .unwrap();
        assert_eq!(
            a.target_def.hash, b.target_def.hash,
            "a different host linter path for the same effective toolchain must not bust the cache"
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
                    "js_lint".to_string(),
                    Default::default(),
                ),
                driver: "js_lint".to_string(),
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
                "eslint_plugins",
                vec!["//@heph/js/thirdparty/eslint-plugin-react@1.0.0:js_install"],
            ),
            ("", vec!["//@heph/fs:file@f=packages/a/src/index.ts"]),
            (
                "config",
                vec!["//@heph/fs:file@f=packages/a/.oxlintrc.json"],
            ),
        ]);
        let resp = driver().parse(req, &ct).await.unwrap();
        let origin_ids: Vec<&str> = resp
            .target_def
            .inputs
            .iter()
            .map(|i| i.origin_id.as_str())
            .collect();
        // Sorted group order: "" < "config" < "eslint_plugins".
        assert_eq!(
            origin_ids,
            vec!["dep||0", "dep|config|0", "dep|eslint_plugins|0"]
        );
    }

    /// Task requirement (feature-quality M5 review finding): `js_lint` is
    /// listed unconditionally for every package with a `package.json` — no
    /// discovery gating the way `js_test`'s per-file glob has (see
    /// `Provider::list`'s doc). A package with zero matched first-party
    /// source files (the workspace-root `package.json`, a types-only
    /// re-export package, a package mid-scaffolding) must not get a
    /// permanently-failing target with no way to opt out. Needs no real
    /// linter binary: with no staged `""`-group inputs, `run()` must take the
    /// early no-op return before ever touching `linter_bin`.
    #[tokio::test]
    async fn run_noops_successfully_when_no_source_files_staged() {
        let ct = ctoken();
        let parsed = driver().parse(make_parse_request(&[]), &ct).await.unwrap();

        let dir = tempfile::tempdir().expect("tempdir");
        let request_id = "test".to_string();
        let run_req = make_run_request(
            &parsed.target_def,
            &request_id,
            dir.path().to_path_buf(),
            dir.path().join("packages/a"),
            "deadbeef",
        );
        let resp = driver()
            .run(run_req, &ct)
            .await
            .expect("zero staged source files must no-op successfully, not hard-fail");
        assert!(resp.artifacts.is_empty());
    }

    // ---- run(): gated on a real oxlint/eslint binary being available in
    // this devenv ----
    //
    // Everything above tests cache-key/Input-declaration behavior and needs
    // no real linter. These two exercise the actual subprocess invocation and
    // its success/failure surfacing (task requirement 5) — they require a
    // real `oxlint` (checked via PATH) and skip cleanly, with a clear
    // message, when one isn't present, rather than failing the whole suite.

    fn find_real_oxlint() -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            let cand = dir.join("oxlint");
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

    #[tokio::test]
    #[ignore = "requires a real `oxlint` on PATH — devenv.nix provisions no Node/oxlint \
                toolchain (see toolchain.rs module docs); run explicitly with \
                `cargo test -- --ignored` on a host with oxlint installed"]
    async fn run_succeeds_on_a_clean_package() {
        let oxlint = find_real_oxlint().expect(
            "this test is #[ignore]d precisely because `oxlint` isn't guaranteed on PATH — it \
             was run explicitly, so a missing `oxlint` here is a real failure, not a skip",
        );

        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );

        let ct = ctoken();
        let req = make_parse_request(&[
            (
                "linter_bin",
                Value::String(oxlint.to_string_lossy().into_owned()),
            ),
            ("config_path", Value::String(String::new())),
            ("config_content", Value::String(String::new())),
        ]);
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
            .expect("a clean package must lint successfully");
    }

    #[tokio::test]
    #[ignore = "requires a real `oxlint` on PATH — devenv.nix provisions no Node/oxlint \
                toolchain (see toolchain.rs module docs); run explicitly with \
                `cargo test -- --ignored` on a host with oxlint installed"]
    async fn run_fails_and_surfaces_lint_output_on_a_real_violation() {
        let oxlint = find_real_oxlint().expect(
            "this test is #[ignore]d precisely because `oxlint` isn't guaranteed on PATH — it \
             was run explicitly, so a missing `oxlint` here is a real failure, not a skip",
        );

        let dir = tempfile::tempdir().expect("tempdir");
        // `no-debugger` is one of oxlint's default-enabled correctness rules.
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "debugger;\nexport const x = 1;\n",
        );

        let ct = ctoken();
        let req = make_parse_request(&[
            (
                "linter_bin",
                Value::String(oxlint.to_string_lossy().into_owned()),
            ),
            ("config_path", Value::String(String::new())),
            ("config_content", Value::String(String::new())),
        ]);
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
            .expect("a real lint violation must fail the driver, not succeed silently");
        let msg = format!("{err:#}");
        assert!(msg.contains("lint failed"), "{msg}");
        assert!(
            msg.contains("no-debugger") || msg.contains("debugger"),
            "the failure must name the violating rule/statement: {msg}"
        );
    }
}
