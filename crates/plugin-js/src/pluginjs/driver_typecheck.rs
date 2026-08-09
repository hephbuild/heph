//! `js_typecheck` — runs `tsc --noEmit` over one package at a time.
//!
//! Per-package granularity is a deliberate design choice, not a shortcut:
//! TypeScript's global augmentation / `declare global` makes true per-file
//! soundness impossible — the exact same reasoning `crate::plugingo`'s own
//! per-package (never per-file) granularity rests on for Go. See
//! `ai-docs/js-plugin-plan.md`'s "Caching / incrementality" section.
//!
//! **M3 scope — check-only, no emit.** `tsc --noEmit` (or, lacking a
//! tsconfig anywhere on the ancestor chain, `tsc --noEmit <files...>`) is run
//! against the package's own `tsconfig.json`, or the nearest ancestor one
//! (`importgraph::find_nearest_tsconfig` — the same walk-up
//! `resolvers.rs`/`importgraph.rs` already use, so this driver's notion of
//! "which tsconfig applies" can never disagree with the import graph's).
//! Isolated-declarations `.d.ts` emission as a per-package API cut-point is
//! explicitly **not** built here — the design doc calls it out as a later
//! refinement, not M3 scope.
//!
//! ## Toolchain: `tstool = "host"`, a disclosed non-hermetic escape hatch
//!
//! No hermetic Node/TypeScript toolchain exists anywhere in this plugin yet.
//! `Provider::get` (see `provider.rs::typecheck_config`) resolves a `tsc`
//! binary from the host (`toolchain::resolve_host_tsc`) and threads its
//! absolute path plus its queried `tsc --version` string through this
//! target's config. This is exactly as disclosed a gap as
//! `driver_install.rs`'s `.npmrc`-auth gap or its best-effort lifecycle-script
//! execution: real, not hidden, and expected to be replaced once a pinned
//! hermetic TypeScript download exists (TODO M4+, see `toolchain.rs`).
//!
//! ## Inputs / cache key
//!
//! Every first-party source file in the package's own closure
//! (`importgraph::package_source_files`, filtered by the tsconfig's own
//! `include`/`exclude`/`files` when it declares any) plus every file
//! `ImportGraph::type_edges`/`runtime_edges` resolved to outside the package
//! (workspace-sibling or third-party, `.d.ts` or otherwise — a plain runtime
//! import needs its target's types for `tsc` just as much as an `import
//! type` does) are declared `Input`s, grouped `""` / `"types"` respectively,
//! plus the resolved tsconfig itself and its whole `extends` chain under
//! `"tsconfig"`. An import that names a third-party/workspace-sibling
//! package but never resolves on disk at all (no ambient `node_modules`
//! installed — the realistic steady state) still contributes to `"types"`,
//! via the same lockfile-driven addressing `provider.rs`'s `deps_config`
//! already uses for a declared dependency. This is the M3 scoping
//! requirement: the cache key must be sensitive to exactly what `tsc`
//! actually reads for *this* package, not the whole workspace — see
//! `provider.rs`'s `typecheck_deps_config` and its tests for the
//! input-scoping proof (the single most important test in this milestone,
//! per the task).
//!
//! A *shared* tsconfig (the package has none of its own and inherits an
//! ancestor's) is only trusted when that ancestor's own `include`/`files`
//! provably confine it to this package —
//! `importgraph::check_tsconfig_scope` rejects anything else with a loud
//! `Provider::get` error rather than silently building an Input set that
//! might not match the real, wider `tsc --project` program.
//!
//! **Known scope trims, disclosed rather than silent:** a tsconfig
//! `include`/`exclude`/`files` inherited purely through `extends` (not
//! redeclared by the leaf/ancestor itself) is not merged in — see
//! `importgraph::TsconfigFields`'s doc; and composite-project `references`
//! are not followed — see `resolvers.rs`'s `TsconfigReferences::Disabled`
//! note, which applies here too since this driver reuses the same
//! `find_nearest_tsconfig` resolution the import graph does. Two more, named
//! by M3 review rather than caught by a fixture yet:
//!
//! - **One-hop type edges.** `ImportGraph::type_edges` records a first-party
//!   file's own `import type`/`export type` sites, resolved one hop, but
//!   never recurses into the resolved `.d.ts`'s *own* imports/re-exports
//!   (common for real `@types/*` packages), never follows triple-slash
//!   `/// <reference types="..."/>`/`/// <reference path="..."/>`
//!   directives, and never models TypeScript's automatic ambient inclusion of
//!   every package under the resolved `typeRoots` (default
//!   `node_modules/@types/*`) when the tsconfig doesn't restrict `"types"` —
//!   files `tsc` reads unconditionally, with no corresponding `import`
//!   anywhere to detect. A change to one of these is invisible to this
//!   driver's Input set — a wrong cache hit, not merely a missed diagnostic.
//!   TODO M4+: recurse `.d.ts` re-exports, parse triple-slash directives, and
//!   declare `@types/*`/`typeRoots` as Inputs when `"types"` isn't
//!   restricted.
//! - **The `node` interpreter behind the `tsc` shebang is unhashed.**
//!   `tsc_version` hashes the TypeScript *package's* semver, but `run()`
//!   passes the host `PATH` through unhashed specifically so a
//!   `#!/usr/bin/env node`-shebang `tsc_bin` can find `node` (see the comment
//!   at that call site) — so which `node` binary/version actually executes
//!   the compiler is not part of the cache key. This is the same
//!   PATH-passthrough shape `go_compile`'s non-hermetic mode already accepts,
//!   but the interpreter here is the *primary* toolchain, not an optional
//!   `cgo` helper, so the divergence surface is larger. Accepted for now as
//!   part of the same `tstool = "host"` non-hermetic exemption above; TODO
//!   M4+: hash a queried `node --version` (or the resolved `node` binary's
//!   own identity) alongside `tsc_version` once a hermetic toolchain makes it
//!   worth the extra subprocess spawn.
//!
//! Beyond the declared `Input`s (whose *content* the engine hashes
//! automatically — see architecture.md's "Automatic hashing"),
//! [`JsTypecheckDef::hash`] additionally hashes the tsconfig's own content
//! (leaf plus every `extends`-chain ancestor's, concatenated) and the
//! queried `tsc_version` string directly, per this milestone's
//! `*Def::hash()` discipline: a compiler-flag or compiler-version change must
//! bust the cache even though the tsconfig content is *also* covered via its
//! declared Input (the two are deliberately redundant — the explicit hash
//! keeps this target's cache-sensitivity independently verifiable at the
//! `parse()` level, without needing a full engine round-trip to prove it; see
//! this module's tests). `tsc_bin` (an absolute host path) is deliberately
//! **excluded** from the hash — see the field's own doc comment.
//!
//! ## Failure reporting
//!
//! Unlike `go_lint`'s `-json`-always-exits-0 split (analyze vs. gate, so
//! facts still propagate to dependents even when a package has findings),
//! `js_typecheck` has no fact-propagation concern in this milestone — nothing
//! downstream reads a typecheck target's output. So a single driver suffices:
//! a real `tsc` type error surfaces as a plain, non-zero-exit driver failure,
//! with both stdout and stderr tails included (`tsc` writes diagnostics to
//! stdout, not stderr) — see [`tsc_failure_detail`].

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

/// Config for a `js_typecheck` target. Entirely engine-generated by the `js`
/// provider's `Provider::get` (see `pluginjs::provider::Provider::typecheck_config`)
/// — never authored by hand in a BUILD file.
#[derive(Spec)]
struct JsTypecheckSpec {
    /// Absolute host path to the resolved `tsc` binary
    /// (`toolchain::resolve_host_tsc`). Deliberately **not** part of
    /// `JsTypecheckDef`'s hash — see that struct's `tsc_bin` field doc.
    #[spec(required)]
    tsc_bin: String,
    /// `tsc --version`'s trimmed output, queried once by `Provider::get`.
    /// Hashed: a host tsc upgrade/downgrade must bust the cache.
    #[spec(required)]
    tsc_version: String,
    /// Workspace-root-relative path to the tsconfig actually in effect (the
    /// package's own, or the nearest ancestor's — see
    /// `importgraph::find_nearest_tsconfig`). Empty when no tsconfig exists
    /// anywhere on the ancestor chain up to the workspace root.
    tsconfig_path: String,
    /// The resolved tsconfig's own raw bytes, hashed directly — see module
    /// docs' "Inputs / cache key" section for why this is not purely
    /// redundant with the `"tsconfig"` dep group below.
    tsconfig_content: String,
    /// Dependencies, grouped by name → list of target addresses: `""` = the
    /// package's own first-party source files (tsconfig-`include`/`exclude`-
    /// filtered), `"types"` = every third-party/workspace-sibling file
    /// reached via `ImportGraph::type_edges`/`runtime_edges`, plus a
    /// whole-package `js_install`/sibling addr for any such import that never
    /// resolved on disk at all, `"tsconfig"` = the resolved tsconfig file
    /// plus its whole `extends` chain (0 or more entries) — see module docs'
    /// "Inputs / cache key" section.
    deps: HashMap<String, Vec<String>>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct JsTypecheckDef {
    tsc_version: String,
    tsconfig_path: String,
    tsconfig_content: String,
    /// Absolute host `tsc` path — carried through so `run()` can exec it, but
    /// see `Hash` impl below: deliberately excluded from the cache key.
    tsc_bin: String,
}

/// Bump to invalidate every cached `js_typecheck` result whenever the
/// invocation shape (flags, tsconfig-lookup rule, what counts as a hashed
/// config field) changes in a way the declared `Input` content hash alone
/// would not already capture.
const JS_TYPECHECK_FORMAT_VERSION: u32 = 1;

impl Hash for JsTypecheckDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        JS_TYPECHECK_FORMAT_VERSION.hash(state);
        self.tsc_version.hash(state);
        self.tsconfig_path.hash(state);
        self.tsconfig_content.hash(state);
        // `tsc_bin` is deliberately NOT hashed: it's an absolute host
        // filesystem path (`/home/alice/.../node_modules/.bin/tsc` vs.
        // `/usr/local/bin/tsc`) that differs across machines/checkouts for
        // the exact same effective toolchain — hashing it would needlessly
        // break cache portability (including remote-cache sharing) for a
        // toolchain that is otherwise identical. `tsc_version` is the actual
        // cache-relevant signal, matching how `plugin-go`'s toolchain
        // resolution never hashes a resolved host `go` binary path either.
        //
        // Source/type-dep file *content* arrives via declared `Input`s; the
        // engine hashes that separately (architecture.md's "Automatic
        // hashing"), so it is not duplicated here — same discipline as
        // `GoCompileDef`/`GoLintDef`.
    }
}

pub struct JsTypecheckDriver;

impl JsTypecheckDriver {
    pub fn new() -> Self {
        Self
    }
}

impl Default for JsTypecheckDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ManagedDriver for JsTypecheckDriver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "js_typecheck".to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        JsTypecheckSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let addr = &req.target_spec.addr;
        let pkg = addr.package.clone();
        let spec =
            JsTypecheckSpec::from(&req.target_spec.config).context("parse js_typecheck config")?;

        // Dep groups arrive from a HashMap — sort by group name so the
        // resulting `inputs` (and thus anything order-sensitive downstream)
        // is deterministic across parses, not HashMap-iteration order. Same
        // regression class as `js_package_info`'s `dep_inputs` /
        // `driver_golist.rs`'s `_golist`.
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

        let def = JsTypecheckDef {
            tsc_version: spec.tsc_version,
            tsconfig_path: spec.tsconfig_path,
            tsconfig_content: spec.tsconfig_content,
            tsc_bin: spec.tsc_bin,
        };

        let hash = {
            let mut h = DebugHasher::new(Xxh3Default::new(), || {
                format!("js_typecheck_{}", addr.format())
            });
            // `JS_TYPECHECK_FORMAT_VERSION` is hashed once, inside `Hash for
            // JsTypecheckDef` itself — matching `plugin-go`'s
            // `GoCompileDef`/`GO_COMPILE_FORMAT_VERSION` precedent (hashing it
            // here too would be a redundant no-op, not a second signal).
            def.hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs,
                // No output artifact: a `js_typecheck` target's only
                // observable effect is "did it succeed" — mirrors
                // `go_lint_gate`'s read-only, produces-nothing shape, but
                // without the separate analyze/gate split (see module docs'
                // "Failure reporting").
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
        let def = req.request.target.def_de::<JsTypecheckDef>();
        let tsc_bin = std::path::PathBuf::from(&def.tsc_bin);

        // `tstool = "host"` is explicitly non-hermetic (see module docs): the
        // resolved `tsc` binary is very likely a `#!/usr/bin/env node`
        // shebang script, so executing it needs `node` reachable via PATH —
        // pass the host's through, mirroring `go_compile`'s own PATH
        // passthrough for its non-hermetic toolchain modes. Only `PATH` is
        // forwarded — `HOME`/`TMPDIR`/locale/`NODE_OPTIONS`/etc. are not; a
        // `node`-invoked `tsc` occasionally does want one of those (npm
        // config lookups, some editor-integration cache paths). Accepted for
        // now as part of the same disclosed non-hermetic exemption above; a
        // real failure would be a two-line fix (add the var here), not a
        // design change.
        let mut env: HashMap<String, String> = HashMap::new();
        if let Ok(v) = std::env::var("PATH") {
            env.insert("PATH".to_string(), v);
        }

        let mut args: Vec<OsString> = vec![OsString::from("--noEmit")];
        if def.tsconfig_path.is_empty() {
            // No tsconfig anywhere on the ancestor chain: fall back to
            // checking exactly the declared first-party source files
            // directly. Rougher than a real tsconfig-driven check (default
            // compiler options apply), but keeps the driver usable for a
            // plain-JS/TS package with no tsconfig at all — see module docs.
            let srcs = self.group_staged_paths(&req, "");
            anyhow::ensure!(
                !srcs.is_empty(),
                "js_typecheck: no tsconfig.json found for {} and no first-party source files \
                 staged — nothing to typecheck",
                req.request.target.addr.format()
            );
            args.extend(srcs.into_iter().map(OsString::from));
        } else {
            let tsconfig_abs = req.sandbox_ws_dir.join(&def.tsconfig_path);
            args.push(OsString::from("--project"));
            args.push(tsconfig_abs.into_os_string());
        }

        // `cwd = sandbox_pkg_dir`, not `sandbox_ws_dir`: every path argument
        // above is already absolute (`tsconfig_abs`, and `srcs` — sourced
        // from `group_staged_paths`' list files, themselves always absolute
        // per `hartifactcontent::unpack`'s `dest = dst.join(...)`), so `cwd`
        // only matters for `tsc`'s own ambient, `process.cwd()`-relative
        // behavior (module/type resolution fallbacks) — the package's own
        // directory is what a real, non-heph `tsc` invocation runs with in
        // practice. See `driver_test.rs`'s module docs for the confirmed
        // live bug (`sandbox_ws_dir` broke a plugin's own ambient config
        // discovery) this mirrors the fix for.
        self.exec_tsc(&tsc_bin, args, &env, &req.sandbox_pkg_dir, ctoken)
            .await?;

        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

impl JsTypecheckDriver {
    /// All staged file paths for the inputs in dep `group` (origin
    /// `dep|group|*`). Mirrors `driver_compile.rs`/`driver_lint.rs`'s
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

    async fn exec_tsc(
        &self,
        tsc_bin: &std::path::Path,
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
            program: tsc_bin.to_path_buf(),
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
            .with_context(|| format!("wait for tsc ({tsc_bin:?})"))?;
        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "tsc failed ({}):\n{}",
                output.status,
                tsc_failure_detail(&stdout, &stderr)
            );
        }
        Ok(())
    }
}

/// Build the human-readable detail for a failed `tsc` invocation. `tsc`
/// writes its diagnostics (type errors) to **stdout**, not stderr — mirrors
/// `driver_compile.rs`'s `go_failure_detail`, which includes both streams for
/// the identical reason (a tool whose failure output lands on either stream
/// depending on the tool must never come back silently blank).
fn tsc_failure_detail(stdout: &str, stderr: &str) -> String {
    let stdout_tail = hplugin::error::head_and_tail_lines(stdout.trim(), 40);
    let stderr_tail = hplugin::error::head_and_tail_lines(stderr.trim(), 40);
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

    fn driver() -> JsTypecheckDriver {
        JsTypecheckDriver::new()
    }

    fn ctoken() -> StdCancellationToken {
        StdCancellationToken::new()
    }

    fn config(extra: &[(&str, Value)]) -> HashMap<String, Value> {
        let mut c: HashMap<String, Value> = HashMap::from([
            (
                "tsc_bin".to_string(),
                Value::String("/usr/bin/tsc".to_string()),
            ),
            (
                "tsc_version".to_string(),
                Value::String("Version 5.6.2".to_string()),
            ),
            (
                "tsconfig_path".to_string(),
                Value::String("packages/a/tsconfig.json".to_string()),
            ),
            (
                "tsconfig_content".to_string(),
                Value::String("{}".to_string()),
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
                    "js_typecheck".to_string(),
                    Default::default(),
                ),
                driver: "js_typecheck".to_string(),
                config: config(extra),
                ..Default::default()
            }),
        }
    }

    #[tokio::test]
    async fn driver_name_is_js_typecheck() {
        let resp = driver().config(ConfigRequest {}).unwrap();
        assert_eq!(resp.name, "js_typecheck");
    }

    #[tokio::test]
    async fn parse_missing_required_field_errors() {
        let ct = ctoken();
        let req = ParseRequest {
            request_id: "test".to_string(),
            target_spec: Arc::new(TargetSpec {
                addr: Addr::new(
                    PkgBuf::from("packages/a"),
                    "js_typecheck".to_string(),
                    Default::default(),
                ),
                driver: "js_typecheck".to_string(),
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

    /// Task requirement: a compiler-*version* change must bust the cache.
    /// This is the parse()-level proxy for "the host tsc was upgraded" —
    /// deliberately does not require a real tsc invocation (see module docs'
    /// "Toolchain" section for why the version is threaded through config
    /// rather than queried in `parse()` itself).
    #[tokio::test]
    async fn parse_hash_changes_when_tsc_version_changes() {
        let ct = ctoken();
        let a = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        let b = driver()
            .parse(
                make_parse_request(&[("tsc_version", Value::String("Version 5.7.0".to_string()))]),
                &ct,
            )
            .await
            .unwrap();
        assert_ne!(a.target_def.hash, b.target_def.hash);
    }

    /// Task requirement: a tsconfig content change must bust the cache, and
    /// be stable when it does not change.
    #[tokio::test]
    async fn parse_hash_changes_when_tsconfig_content_changes_and_stable_when_not() {
        let ct = ctoken();
        let a = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        let b = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        assert_eq!(
            a.target_def.hash, b.target_def.hash,
            "identical tsconfig content must hash identically"
        );

        let c = driver()
            .parse(
                make_parse_request(&[(
                    "tsconfig_content",
                    Value::String(r#"{"compilerOptions":{"strict":true}}"#.to_string()),
                )]),
                &ct,
            )
            .await
            .unwrap();
        assert_ne!(
            a.target_def.hash, c.target_def.hash,
            "a changed tsconfig content must change the cache key"
        );
    }

    /// `tsc_bin` is an absolute host path — must NOT affect the cache key, or
    /// the exact same effective toolchain would produce a different cache
    /// entry per machine/checkout, defeating remote-cache sharing.
    #[tokio::test]
    async fn parse_hash_unaffected_by_tsc_bin_path_difference() {
        let ct = ctoken();
        let a = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        let b = driver()
            .parse(
                make_parse_request(&[(
                    "tsc_bin",
                    Value::String("/home/someone/project/node_modules/.bin/tsc".to_string()),
                )]),
                &ct,
            )
            .await
            .unwrap();
        assert_eq!(
            a.target_def.hash, b.target_def.hash,
            "a different host tsc path for the same effective toolchain must not bust the cache"
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
        let mut config = self_config();
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
        config.insert("deps".to_string(), Value::Map(deps_map));
        ParseRequest {
            request_id: "test".to_string(),
            target_spec: Arc::new(TargetSpec {
                addr: Addr::new(
                    PkgBuf::from("packages/a"),
                    "js_typecheck".to_string(),
                    Default::default(),
                ),
                driver: "js_typecheck".to_string(),
                config,
                ..Default::default()
            }),
        }
    }

    fn self_config() -> HashMap<String, Value> {
        config(&[])
    }

    #[tokio::test]
    async fn parse_deps_become_target_dep_inputs_ordered_by_group() {
        let ct = ctoken();
        let req = make_parse_request_with_deps(vec![
            ("types", vec!["//@heph/fs:file@f=packages/b/index.d.ts"]),
            ("", vec!["//@heph/fs:file@f=packages/a/src/index.ts"]),
            (
                "tsconfig",
                vec!["//@heph/fs:file@f=packages/a/tsconfig.json"],
            ),
        ]);
        let resp = driver().parse(req, &ct).await.unwrap();
        let origin_ids: Vec<&str> = resp
            .target_def
            .inputs
            .iter()
            .map(|i| i.origin_id.as_str())
            .collect();
        // Sorted group order: "" < "tsconfig" < "types".
        assert_eq!(origin_ids, vec!["dep||0", "dep|tsconfig|0", "dep|types|0"]);
    }

    // ---- run(): gated on a real tsc binary being available in this devenv ----
    //
    // Everything above tests cache-key/Input-declaration behavior and needs no
    // real tsc. These two exercise the actual subprocess invocation and its
    // success/failure surfacing (task requirement 5) — they require a real
    // `tsc` (checked via PATH / `node_modules/.bin/tsc` convention) and skip
    // cleanly, with a clear message, when one isn't present, rather than
    // failing the whole suite.

    fn find_real_tsc() -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            let cand = dir.join("tsc");
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
    #[ignore = "requires a real `tsc` on PATH — devenv.nix provisions no Node/TypeScript \
                toolchain (see toolchain.rs module docs); run explicitly with \
                `cargo test -- --ignored` on a host with TypeScript installed"]
    async fn run_succeeds_on_a_clean_package() {
        let tsc = find_real_tsc().expect(
            "this test is #[ignore]d precisely because `tsc` isn't guaranteed on PATH — it was \
             run explicitly, so a missing `tsc` here is a real failure, not a skip",
        );

        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/tsconfig.json",
            r#"{"compilerOptions":{"strict":true,"skipLibCheck":true,"types":[]}}"#,
        );
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x: number = 1;\n",
        );

        let ct = ctoken();
        let req = make_parse_request(&[
            ("tsc_bin", Value::String(tsc.to_string_lossy().into_owned())),
            (
                "tsconfig_path",
                Value::String("packages/a/tsconfig.json".to_string()),
            ),
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
            .expect("a type-clean package must typecheck successfully");
    }

    #[tokio::test]
    #[ignore = "requires a real `tsc` on PATH — devenv.nix provisions no Node/TypeScript \
                toolchain (see toolchain.rs module docs); run explicitly with \
                `cargo test -- --ignored` on a host with TypeScript installed"]
    async fn run_fails_and_surfaces_tsc_output_on_a_real_type_error() {
        let tsc = find_real_tsc().expect(
            "this test is #[ignore]d precisely because `tsc` isn't guaranteed on PATH — it was \
             run explicitly, so a missing `tsc` here is a real failure, not a skip",
        );

        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/tsconfig.json",
            r#"{"compilerOptions":{"strict":true,"skipLibCheck":true,"types":[]}}"#,
        );
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x: number = 'not a number';\n",
        );

        let ct = ctoken();
        let req = make_parse_request(&[
            ("tsc_bin", Value::String(tsc.to_string_lossy().into_owned())),
            (
                "tsconfig_path",
                Value::String("packages/a/tsconfig.json".to_string()),
            ),
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
            .expect("a real type error must fail the driver, not succeed silently");
        let msg = format!("{err:#}");
        assert!(msg.contains("tsc failed"), "{msg}");
        // TS2322: Type 'string' is not assignable to type 'number'.
        assert!(
            msg.contains("TS2322") || msg.contains("not assignable"),
            "{msg}"
        );
    }
}
