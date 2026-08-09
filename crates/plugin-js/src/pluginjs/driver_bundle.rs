//! `js_bundle` — runs the configured bundler (`esbuild` default, the only
//! value implemented this milestone) over **one package's entry point at a
//! time**, producing a bundled output directory.
//!
//! ## Variants: `format` (esm/cjs) and `target` (node/browser)
//!
//! Per `ai-docs/js-plugin-plan.md`'s Variants section, module format and
//! target environment are addr args scoped to `js_bundle` only —
//! `js_install`/`js_typecheck`/`js_test`/`js_lint` stay variant-free, never
//! see either arg, and are unaffected by this milestone.
//!
//! **Design decision, and where this disagrees with a literal reading of the
//! plan doc's "reuse the existing `provider_state(provider=…,
//! variants={…})` mechanism (ancestry-vs-universe resolution)":** this
//! driver does **not** port `plugin-go`'s `variant.rs` ancestry/universe
//! resolver. That machinery exists to solve two problems specific to Go's
//! model — arbitrarily-nested `go.mod` module trees (an "ancestry" to walk
//! at all) and a binary's variant choice needing to propagate consistently
//! to transitive library deps possibly living in a sibling subtree (the
//! `vp`-pinned "universe" lookup) — neither of which exists here:
//! `pluginjs::workspace` is a single flat workspace root (nested workspaces
//! are explicitly rejected, see `workspace.rs`), so there is no ancestor
//! chain to walk and at most one enclosing scope per package. And per the
//! plan doc itself, this axis is scoped to bundle targets only — it does not
//! need to propagate to or agree with any other driver's own resolution the
//! way a Go variant must propagate through `build_lib`/`_golist`. Building
//! the ancestry/universe/inheritance/cycle-detection machinery for an axis
//! with nothing to inherit from and nothing to propagate to would be exactly
//! the unused generality this crate's own review history repeatedly flags.
//!
//! Instead, `format`/`target` are plain addr args resolved with a flat
//! default when absent (`format` → `"esm"`, `target` → `"node"`) — the same
//! shape `js_test`'s `file` addr arg already uses in this crate (see
//! `provider.rs`'s `Provider::get` handling of [`crate::pluginjs::TEST_TARGET`]),
//! just with a default rather than a hard "must specify" requirement. A
//! default (rather than Go's mandatory-`@v=`-with-no-default) is deliberate:
//! Go's "no implicit default" rule exists because an ancestry walk can make
//! *which* declaration applies genuinely ambiguous; there is no equivalent
//! ambiguity here to protect against, and requiring every caller to spell
//! out `@format=esm,target=node` for the common case would be pure
//! ceremony.
//!
//! ## Toolchain: `bundler = "esbuild"`, a disclosed non-hermetic escape hatch
//!
//! Exactly the same shape as `js_typecheck`'s `tstool = "host"` /
//! `js_test`'s `testrunner` / `js_lint`'s `linter` (see `toolchain.rs` module
//! docs): no hermetic Node/bundler toolchain exists anywhere in this plugin
//! yet, so `Provider::get` (`provider.rs::bundle_config`) resolves the
//! configured bundler's binary from the host
//! (`toolchain::resolve_host_bundler`) and queries its `--version` once,
//! threading both through this target's config. `rollup`/`webpack`/`vite`
//! are a stated M6+ follow-up — `bundler` is a real, checked option (not an
//! assumption baked into the driver), so setting it to anything but
//! `"esbuild"` today fails loudly at `Provider::get`, never silently.
//!
//! ## Entry points
//!
//! One entry file per target: the package's own `package.json` `"main"`
//! field by default, or an explicit `entry=<workspace-relative-path>` addr
//! arg override. An override is validated against workspace/package
//! containment the same way `js_test`'s `file` addr arg is — see
//! `provider.rs`'s `reject_path_escape`/`path_under_package` and their
//! `js_bundle`-specific tests in this module and in `provider.rs`.
//!
//! **Known scope trim, disclosed rather than silent**: exactly one entry
//! point per target, not `package.json` `"exports"`-map-driven multi-entry
//! bundling. A package needing several bundled entry points today addresses
//! several `js_bundle@entry=…` targets explicitly (each independently
//! cached). TODO M6+: multi-entry-point single-invocation bundling (matching
//! esbuild's own native multi-entry support) once a real need for it shows
//! up — building it speculatively now would be exactly the unused
//! generality this crate's own review history flags.
//!
//! ## Inputs / cache key
//!
//! Per `ai-docs/js-plugin-plan.md`'s `js_bundle` bullet under Caching/
//! incrementality: *"whole-graph by construction — cache key is the full
//! transitive closure's content hash plus bundler config/version. This is
//! the one driver where per-file incrementality is not the goal;
//! correctness of the entry-point closure is."* Concretely
//! (`provider.rs`'s `Provider::bundle_closure`):
//!
//! - Every first-party file transitively reachable from the entry point via
//!   `ImportGraph::runtime_edges` (**not** `type_edges` — a bundler erases
//!   type-only imports, it never emits them, so following them would
//!   over-declare the Input set), **recursing across workspace-package
//!   boundaries** — unlike `js_test`/`js_typecheck`'s deliberate "one-hop
//!   external" trim (per-package granularity is *correct* for those, so a
//!   sibling package's own further imports are out of scope), a bundler
//!   genuinely inlines a sibling package's entire reachable source into the
//!   same output, so under-declaring past one hop here would be a real
//!   cache-correctness bug, not merely a narrower one. This is the
//!   structural reason `js_bundle`'s closure builder cannot reuse
//!   `importgraph::build_test_closure` as-is — see that function's own doc
//!   for the trim this driver deliberately does not inherit.
//! - Every third-party package the closure's own unresolved bare specifiers
//!   name, resolved via `deps::resolve_one_dependency` — the lockfile-driven
//!   mechanism, never by walking `oxc_resolver` paths against an ambient
//!   `node_modules` (the M3-review lesson recorded in `provider.rs`, applied
//!   again here). Third-party packages are **not** recursed into further —
//!   the whole `js_install`/thirdparty addr is depended on as one opaque
//!   unit, whose own declared `DirPath` output already content-hashes
//!   everything inside it, so there is nothing to gain by walking its
//!   internals (mirrors `js_typecheck`/`js_test`'s identical one-hop
//!   treatment of third-party edges). The bare specifier name each such
//!   package was actually imported by (not just its resolved addr) is
//!   carried alongside it and fed to esbuild's own `--external:<name>` flag
//!   (`run()`, below) — see `provider.rs`'s `BundleClosureResult::external_names`
//!   doc for the feature-quality M6 review BLOCKER this fixes: previously
//!   only a bundler-config-file's own opt-in `"external"` array reached that
//!   flag, so every real third-party import (never listed there) made a real
//!   `esbuild --bundle` hard-fail trying to inline it.
//! - The resolved bundler config file, if any (`esbuild.config.json`, walked
//!   up the ancestor chain the same way a tsconfig is — see
//!   `importgraph::find_nearest_bundler_config`), plus every file it itself
//!   references via a relative `import`/`require` (`importgraph::
//!   resolve_runner_config_referenced_files`, reused as-is — see that
//!   function's doc; it degrades to a no-op for a `.json` config, which
//!   cannot `import` anything, and exists to future-proof a later JS/TS
//!   config format without new plumbing).
//! - The entry package's own resolved tsconfig, if any, plus its whole
//!   `extends` chain — the same treatment `js_typecheck` gives its tsconfig
//!   (`provider.rs`'s `bundle_deps_config`, mirroring `typecheck_deps_config`).
//!   esbuild auto-discovers and applies a tsconfig's `compilerOptions`
//!   (`paths`/`baseUrl`/`jsx`/`target`/`experimentalDecorators`) the same way
//!   `tsc` does; `run()` passes it an explicit `--tsconfig=<path>` rather
//!   than relying on that auto-discovery (see `run()`'s own comment) —
//!   without this, a `paths`-aliased entry point (a mainstream TS-monorepo
//!   pattern) would fail at real `esbuild` execution because the sandbox
//!   never had a `tsconfig.json` at all (code-quality M6 review BLOCKER).
//!   Only the *entry package's* tsconfig is resolved — the same scope
//!   `esbuild`'s own single-directory-walk auto-discovery would apply from
//!   the entry file, so a sibling package crossed via the whole-graph
//!   closure never contributes a second, conflicting tsconfig.
//! - The queried `bundler_version`.
//!
//! Beyond the declared `Input`s (content-hashed automatically by the
//! engine), [`JsBundleDef::hash`] additionally hashes `entry_file`, `format`,
//! `target`, `outdir`, `bundler_config_path`, `bundler_config_content`,
//! `external`, `tsconfig_path`, `tsconfig_content` and `bundler_version`
//! directly — the same deliberate redundancy-with-declared-Inputs discipline
//! `js_typecheck`'s `tsconfig_content` / `js_test`'s `runner_config_content`
//! already have (an independently-verifiable-at-`parse()`-level
//! cache-sensitivity proof, see this module's tests). `bundler_bin` (an
//! absolute host path) is deliberately **excluded** — see that field's own
//! doc comment.
//!
//! **Native/optional-dependency platform axis**: not hashed directly on this
//! `Def` — a third-party dependency resolved through
//! `deps::resolve_one_dependency` already carries `os`/`arch` in its own
//! `js_install` addr args (see `thirdparty.rs`), so a platform-restricted
//! third-party dependency naturally produces a different declared `Input`
//! addr, and therefore a different `js_bundle` hash, per platform — mirrors
//! `js_typecheck`/`js_test`'s identical choice not to hash `os`/`arch`
//! a second time on top of that.
//!
//! **Known scope trims, disclosed rather than silent (hermeticity M6
//! review)**:
//! - **`target=node`/`target=browser` doesn't change what's resolved or
//!   declared.** `Resolvers`/`import_graph` are not parameterized by
//!   `target`, so a package with a `package.json` `"browser"` field/exports
//!   condition gets the identical declared closure for both variants, while
//!   `esbuild --platform=browser` (driven directly by `def.target`) *does*
//!   honor that field — either the browser variant fails loudly (the
//!   browser-specific file was never staged) or, if it happens to already be
//!   in the closure via an unrelated edge, its presence was never actually
//!   established for the browser-resolution reason. Not fixed this
//!   milestone: parameterizing `Resolvers`/`import_graph`/`graph_cache` by
//!   platform is a real design change (a resolver + per-package cache this
//!   crate shares with `js_typecheck`/`js_test`, neither of which has a
//!   platform axis to key on today) rather than a contained fix, and is left
//!   as a stated M6+ follow-up rather than rushed into this milestone's
//!   close-out.
//! - **An ambiently-`node_modules`-resolved third-party edge is hashed as a
//!   first-party file Input, not routed through the lockfile.** Only
//!   possible when `node_modules` happens to exist on the host at
//!   `Provider::get` time (see `provider.rs`'s `bundle_closure_step`) —
//!   mirrors `typecheck_deps_config`'s identical, already-reviewed (M3)
//!   treatment of the same case, not new to this milestone. Flagged again
//!   here only because `js_bundle` is the driver whose whole stated purpose
//!   is closure correctness, so the general "avoid ambient node_modules"
//!   discipline this module's Inputs section states above still has this one
//!   documented edge-case exception.
//!
//! ## Failure reporting
//!
//! A bundler error (syntax error, unresolvable import esbuild itself catches
//! that this crate's own resolver didn't) is a plain, non-zero-exit driver
//! failure with both stdout and stderr tails surfaced
//! (`bundle_failure_detail`, identical shape to
//! `driver_typecheck.rs`'s `tsc_failure_detail` / `driver_test.rs`'s
//! `test_failure_detail`) — esbuild's actual diagnostic output must reach
//! the user, never be swallowed into a bare "failed".
//!
//! ## Output
//!
//! `run()` invokes esbuild with `--outdir=<sandbox_ws_dir>/<outdir>`; the
//! `TargetDef` declares a single `Content::DirPath(outdir)` output, `collect:
//! true` — the same shape `js_install` uses for a driver that produces file
//! outputs (see `driver_install.rs`), chosen over a single `FilePath` output
//! so a later multi-entry-point milestone can widen `run()`'s invocation
//! without an output-declaration change. `outdir` includes `format`/`target`
//! (`"<pkg>/dist/<format>-<target>"`, `Provider::bundle_config`) — without
//! this, `js_bundle@format=esm` and `js_bundle@format=cjs` for the same
//! package declared the identical output directory, colliding whenever both
//! are built together, the milestone's own headline dual-format-publish
//! shape (feature-quality M6 review BLOCKER).

use anyhow::Context;
use async_trait::async_trait;
use hcore::debug_hash::DebugHasher;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path};
use hplugin::driver::targetdef::{CacheConfig, Input, InputMode, Output, TargetDef};
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

/// Config for a `js_bundle` target. Entirely engine-generated by the `js`
/// provider's `Provider::get` (see `pluginjs::provider::Provider::bundle_config`)
/// — never authored by hand in a BUILD file.
#[derive(Spec)]
struct JsBundleSpec {
    /// Which bundler this target invokes — only `"esbuild"` in this
    /// milestone (see `toolchain::is_supported_bundler`).
    #[spec(required)]
    bundler: String,
    /// Absolute host path to the resolved bundler binary
    /// (`toolchain::resolve_host_bundler`). Deliberately **not** part of
    /// `JsBundleDef`'s hash — see that struct's `bundler_bin` field doc.
    #[spec(required)]
    bundler_bin: String,
    /// The bundler's own `--version` output, trimmed, queried once by
    /// `Provider::get`. Hashed: a host bundler upgrade/downgrade must bust
    /// the cache.
    #[spec(required)]
    bundler_version: String,
    /// Workspace-root-relative path to the one entry file this target
    /// bundles.
    #[spec(required)]
    entry_file: String,
    /// Module format: `"esm"` or `"cjs"`.
    #[spec(required)]
    format: String,
    /// Target environment: `"node"` or `"browser"` — maps to esbuild's own
    /// `--platform` flag (not its separate `--target` syntax-level flag,
    /// which this milestone does not expose).
    #[spec(required)]
    target: String,
    /// Workspace-root-relative output directory this target writes into.
    #[spec(required)]
    outdir: String,
    /// Workspace-root-relative path to the resolved bundler config
    /// (`esbuild.config.json`), or empty when none exists on the ancestor
    /// chain.
    bundler_config_path: String,
    /// The resolved bundler config's own raw bytes, hashed directly — see
    /// module docs' "Inputs / cache key" section.
    bundler_config_content: String,
    /// Package names esbuild's own `--external:<name>` flag needs, one per
    /// entry: the union of the closure's own discovered third-party bare
    /// specifiers and the resolved bundler config's opt-in `"external"`
    /// array — computed once by `Provider::get`
    /// (`Provider::bundle_deps_config`) so `run()` never re-derives it.
    external: Vec<String>,
    /// Workspace-root-relative path to the entry package's resolved
    /// tsconfig (the package's own, or the nearest ancestor's — see
    /// `importgraph::find_nearest_tsconfig`). Empty when no tsconfig exists
    /// anywhere on the ancestor chain.
    tsconfig_path: String,
    /// The resolved tsconfig's own raw bytes (leaf plus its whole `extends`
    /// chain), hashed directly — see module docs' "Inputs / cache key"
    /// section.
    tsconfig_content: String,
    /// Dependencies, grouped by name → target addresses: `""` = every
    /// first-party file transitively reachable from the entry point
    /// (recursing across workspace-package boundaries — see module docs),
    /// `"external"` = every third-party package the closure's own
    /// unresolved bare specifiers name, `"bundler_config"` = the resolved
    /// bundler config file plus anything it references, if any, `"tsconfig"`
    /// = the resolved tsconfig plus its whole `extends` chain, if any.
    deps: HashMap<String, Vec<String>>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct JsBundleDef {
    bundler: String,
    bundler_version: String,
    entry_file: String,
    format: String,
    target: String,
    outdir: String,
    bundler_config_path: String,
    bundler_config_content: String,
    external: Vec<String>,
    tsconfig_path: String,
    tsconfig_content: String,
    /// Absolute host bundler path — carried through so `run()` can exec it,
    /// but see `Hash` impl below: deliberately excluded from the cache key.
    bundler_bin: String,
}

/// Bump to invalidate every cached `js_bundle` result whenever the
/// invocation shape (flags, config-lookup rule, what counts as a hashed
/// config field) changes in a way the declared `Input` content hash alone
/// would not already capture.
const JS_BUNDLE_FORMAT_VERSION: u32 = 1;

impl Hash for JsBundleDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        JS_BUNDLE_FORMAT_VERSION.hash(state);
        self.bundler.hash(state);
        self.bundler_version.hash(state);
        self.entry_file.hash(state);
        self.format.hash(state);
        self.target.hash(state);
        self.outdir.hash(state);
        self.bundler_config_path.hash(state);
        self.bundler_config_content.hash(state);
        self.external.hash(state);
        self.tsconfig_path.hash(state);
        self.tsconfig_content.hash(state);
        // `bundler_bin` is deliberately NOT hashed — an absolute host
        // filesystem path that differs across machines/checkouts for the
        // exact same effective toolchain; see `JsTypecheckDef::hash`'s
        // identical `tsc_bin` exclusion for the full rationale.
        //
        // Source/closure file *content* arrives via declared `Input`s; the
        // engine hashes that separately (architecture.md's "Automatic
        // hashing"), so it is not duplicated here.
    }
}

pub struct JsBundleDriver;

impl JsBundleDriver {
    pub fn new() -> Self {
        Self
    }
}

impl Default for JsBundleDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ManagedDriver for JsBundleDriver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "js_bundle".to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        JsBundleSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let addr = &req.target_spec.addr;
        let pkg = addr.package.clone();
        let spec = JsBundleSpec::from(&req.target_spec.config).context("parse js_bundle config")?;

        anyhow::ensure!(
            spec.format == "esm" || spec.format == "cjs",
            "js_bundle: unsupported format {:?} for {} — expected \"esm\" or \"cjs\"",
            spec.format,
            addr.format()
        );
        anyhow::ensure!(
            spec.target == "node" || spec.target == "browser",
            "js_bundle: unsupported target {:?} for {} — expected \"node\" or \"browser\"",
            spec.target,
            addr.format()
        );

        // Dep groups arrive from a HashMap — sort by group name so the
        // resulting `inputs` is deterministic across parses, not HashMap-
        // iteration order. Same regression class as `js_typecheck`/
        // `js_test`'s own dep-input ordering.
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

        let mut external = spec.external.clone();
        external.sort();

        let def = JsBundleDef {
            bundler: spec.bundler,
            bundler_version: spec.bundler_version,
            entry_file: spec.entry_file,
            format: spec.format,
            target: spec.target,
            outdir: spec.outdir,
            bundler_config_path: spec.bundler_config_path,
            bundler_config_content: spec.bundler_config_content,
            external,
            tsconfig_path: spec.tsconfig_path,
            tsconfig_content: spec.tsconfig_content,
            bundler_bin: spec.bundler_bin,
        };

        let hash = {
            let mut h = DebugHasher::new(Xxh3Default::new(), || {
                format!("js_bundle_{}", addr.format())
            });
            def.hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        let outdir = def.outdir.clone();

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs,
                outputs: vec![Output {
                    group: String::new(),
                    paths: vec![Path {
                        content: Content::DirPath(outdir),
                        codegen_tree: CodegenMode::None,
                        collect: true,
                    }],
                }],
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
        let def = req.request.target.def_de::<JsBundleDef>();
        // Defense in depth: `Provider::get` already rejects an absolute or
        // `..`-escaping `entry`/config-referenced path before ever building
        // this `TargetDef` (see `provider::reject_path_escape`'s doc — a
        // code-quality review BLOCKER), but a cached `JsBundleDef` read back
        // from disk is untrusted input to this driver too, so the same check
        // runs again here, before either ever reaches `sandbox_ws_dir.join`.
        crate::pluginjs::provider::reject_path_escape("entry_file", &def.entry_file)
            .context("validating cached js_bundle def before running")?;
        crate::pluginjs::provider::reject_path_escape("outdir", &def.outdir)
            .context("validating cached js_bundle def before running")?;
        if !def.tsconfig_path.is_empty() {
            crate::pluginjs::provider::reject_path_escape("tsconfig_path", &def.tsconfig_path)
                .context("validating cached js_bundle def before running")?;
        }

        let bundler_bin = std::path::PathBuf::from(&def.bundler_bin);
        let entry_abs = req.sandbox_ws_dir.join(&def.entry_file);
        let outdir_abs = req.sandbox_ws_dir.join(&def.outdir);

        // `bundler = "esbuild"` is explicitly non-hermetic (see module
        // docs): the resolved binary's own `node_modules/.bin/esbuild` shim
        // is very likely a `#!/usr/bin/env node` script, so executing it
        // needs `node` reachable via PATH — mirrors `js_typecheck::run`'s
        // identical PATH-only passthrough and its own doc comment for why
        // only `PATH` (not `HOME`/`TMPDIR`/etc.) is forwarded.
        let mut env: HashMap<String, String> = HashMap::new();
        if let Ok(v) = std::env::var("PATH") {
            env.insert("PATH".to_string(), v);
        }

        let mut outdir_arg = OsString::from("--outdir=");
        outdir_arg.push(outdir_abs.as_os_str());

        let mut args: Vec<OsString> = vec![
            entry_abs.into_os_string(),
            OsString::from("--bundle"),
            OsString::from(format!("--format={}", def.format)),
            OsString::from(format!("--platform={}", def.target)),
            outdir_arg,
        ];
        // Explicit `--tsconfig=<path>` rather than relying on esbuild's own
        // ancestor-directory auto-discovery — the same reasoning
        // `driver_typecheck.rs::run()` passes `tsc` an explicit `--project`
        // instead of letting it walk up on its own: the resolved tsconfig
        // (code-quality M6 review BLOCKER) is staged at exactly this path,
        // and an explicit flag can never disagree with what `Provider::get`
        // actually declared/hashed as an Input.
        if !def.tsconfig_path.is_empty() {
            let tsconfig_abs = req.sandbox_ws_dir.join(&def.tsconfig_path);
            let mut tsconfig_arg = OsString::from("--tsconfig=");
            tsconfig_arg.push(tsconfig_abs.as_os_str());
            args.push(tsconfig_arg);
        }
        for name in &def.external {
            args.push(OsString::from(format!("--external:{name}")));
        }

        // `cwd = sandbox_pkg_dir`, not `sandbox_ws_dir` — every path
        // argument above is already absolute (`entry_abs`/`outdir_abs`/
        // `tsconfig_abs`), so `cwd` only matters for esbuild's own ambient,
        // `process.cwd()`-relative behavior — the package's own directory is
        // what a real, non-heph invocation runs with in practice. See
        // `driver_test.rs`'s module docs for the confirmed live bug this
        // mirrors the fix for.
        self.exec_bundler(&bundler_bin, args, &env, &req.sandbox_pkg_dir, ctoken)
            .await?;

        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

impl JsBundleDriver {
    async fn exec_bundler(
        &self,
        bundler_bin: &std::path::Path,
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
            program: bundler_bin.to_path_buf(),
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
            .with_context(|| format!("wait for bundler ({bundler_bin:?})"))?;
        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "js_bundle failed ({}):\n{}",
                output.status,
                bundle_failure_detail(&stdout, &stderr)
            );
        }
        Ok(())
    }
}

/// Build the human-readable detail for a failed bundler invocation — both
/// stdout and stderr tails included, mirroring `driver_typecheck.rs`'s
/// `tsc_failure_detail` / `driver_test.rs`'s `test_failure_detail` for the
/// identical reason: a failing bundle's actual output (a syntax error, an
/// unresolvable import) must reach the user, never come back silently blank.
fn bundle_failure_detail(stdout: &str, stderr: &str) -> String {
    let stdout_tail = hplugin::error::last_n_lines(stdout.trim(), 60);
    let stderr_tail = hplugin::error::last_n_lines(stderr.trim(), 60);
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

    fn driver() -> JsBundleDriver {
        JsBundleDriver::new()
    }

    fn ctoken() -> StdCancellationToken {
        StdCancellationToken::new()
    }

    fn config(extra: &[(&str, Value)]) -> HashMap<String, Value> {
        let mut c: HashMap<String, Value> = HashMap::from([
            ("bundler".to_string(), Value::String("esbuild".to_string())),
            (
                "bundler_bin".to_string(),
                Value::String("/usr/bin/esbuild".to_string()),
            ),
            (
                "bundler_version".to_string(),
                Value::String("0.19.2".to_string()),
            ),
            (
                "entry_file".to_string(),
                Value::String("packages/a/src/index.ts".to_string()),
            ),
            ("format".to_string(), Value::String("esm".to_string())),
            ("target".to_string(), Value::String("node".to_string())),
            (
                "outdir".to_string(),
                Value::String("packages/a/dist".to_string()),
            ),
            (
                "bundler_config_path".to_string(),
                Value::String(String::new()),
            ),
            (
                "bundler_config_content".to_string(),
                Value::String(String::new()),
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
                    "js_bundle".to_string(),
                    Default::default(),
                ),
                driver: "js_bundle".to_string(),
                config: config(extra),
                ..Default::default()
            }),
        }
    }

    #[tokio::test]
    async fn driver_name_is_js_bundle() {
        let resp = driver().config(ConfigRequest {}).unwrap();
        assert_eq!(resp.name, "js_bundle");
    }

    #[tokio::test]
    async fn parse_missing_required_field_errors() {
        let ct = ctoken();
        let req = ParseRequest {
            request_id: "test".to_string(),
            target_spec: Arc::new(TargetSpec {
                addr: Addr::new(
                    PkgBuf::from("packages/a"),
                    "js_bundle".to_string(),
                    Default::default(),
                ),
                driver: "js_bundle".to_string(),
                ..Default::default()
            }),
        };
        assert!(driver().parse(req, &ct).await.is_err());
    }

    #[tokio::test]
    async fn parse_rejects_unsupported_format() {
        let ct = ctoken();
        let req = make_parse_request(&[("format", Value::String("umd".to_string()))]);
        let err = driver()
            .parse(req, &ct)
            .await
            .err()
            .expect("an unsupported format must fail parse");
        assert!(format!("{err:#}").contains("umd"));
    }

    #[tokio::test]
    async fn parse_rejects_unsupported_target() {
        let ct = ctoken();
        let req = make_parse_request(&[("target", Value::String("deno".to_string()))]);
        let err = driver()
            .parse(req, &ct)
            .await
            .err()
            .expect("an unsupported target must fail parse");
        assert!(format!("{err:#}").contains("deno"));
    }

    #[tokio::test]
    async fn parse_accepts_cjs_and_browser() {
        let ct = ctoken();
        let req = make_parse_request(&[
            ("format", Value::String("cjs".to_string())),
            ("target", Value::String("browser".to_string())),
        ]);
        driver()
            .parse(req, &ct)
            .await
            .expect("cjs/browser must be accepted");
    }

    #[tokio::test]
    async fn parse_hash_stable_across_identical_parses() {
        let ct = ctoken();
        let a = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        let b = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        assert_eq!(a.target_def.hash, b.target_def.hash);
    }

    /// Task requirement: a bundler-*version* change must bust the cache.
    #[tokio::test]
    async fn parse_hash_changes_when_bundler_version_changes() {
        let ct = ctoken();
        let a = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        let b = driver()
            .parse(
                make_parse_request(&[("bundler_version", Value::String("0.20.0".to_string()))]),
                &ct,
            )
            .await
            .unwrap();
        assert_ne!(a.target_def.hash, b.target_def.hash);
    }

    /// Task requirement: esm vs cjs must be genuinely different cache
    /// entries for the same source.
    #[tokio::test]
    async fn parse_hash_changes_between_esm_and_cjs() {
        let ct = ctoken();
        let a = driver()
            .parse(
                make_parse_request(&[("format", Value::String("esm".to_string()))]),
                &ct,
            )
            .await
            .unwrap();
        let b = driver()
            .parse(
                make_parse_request(&[("format", Value::String("cjs".to_string()))]),
                &ct,
            )
            .await
            .unwrap();
        assert_ne!(
            a.target_def.hash, b.target_def.hash,
            "esm and cjs must produce different cache entries"
        );
    }

    /// Same requirement, the node/browser axis.
    #[tokio::test]
    async fn parse_hash_changes_between_node_and_browser() {
        let ct = ctoken();
        let a = driver()
            .parse(
                make_parse_request(&[("target", Value::String("node".to_string()))]),
                &ct,
            )
            .await
            .unwrap();
        let b = driver()
            .parse(
                make_parse_request(&[("target", Value::String("browser".to_string()))]),
                &ct,
            )
            .await
            .unwrap();
        assert_ne!(
            a.target_def.hash, b.target_def.hash,
            "node and browser must produce different cache entries"
        );
    }

    /// Task requirement: a bundler-config content change must bust the
    /// cache.
    #[tokio::test]
    async fn parse_hash_changes_when_bundler_config_content_changes() {
        let ct = ctoken();
        let a = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        let b = driver()
            .parse(
                make_parse_request(&[(
                    "bundler_config_content",
                    Value::String(r#"{"external":["react"]}"#.to_string()),
                )]),
                &ct,
            )
            .await
            .unwrap();
        assert_ne!(a.target_def.hash, b.target_def.hash);
    }

    /// Code-quality M6 review BLOCKER: a tsconfig content change (a `paths`
    /// alias edit, a `jsx`/`target`/decorators option flip) must bust the
    /// cache the same way `js_typecheck`'s identical `tsconfig_content`
    /// field already does — esbuild reads `compilerOptions` from it too.
    #[tokio::test]
    async fn parse_hash_changes_when_tsconfig_content_changes() {
        let ct = ctoken();
        let a = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        let b = driver()
            .parse(
                make_parse_request(&[(
                    "tsconfig_content",
                    Value::String(
                        r#"{"compilerOptions":{"paths":{"@app/*":["src/*"]}}}"#.to_string(),
                    ),
                )]),
                &ct,
            )
            .await
            .unwrap();
        assert_ne!(a.target_def.hash, b.target_def.hash);
    }

    /// `bundler_bin` is an absolute host path — must NOT affect the cache
    /// key.
    #[tokio::test]
    async fn parse_hash_unaffected_by_bundler_bin_path_difference() {
        let ct = ctoken();
        let a = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        let b = driver()
            .parse(
                make_parse_request(&[(
                    "bundler_bin",
                    Value::String("/home/someone/project/node_modules/.bin/esbuild".to_string()),
                )]),
                &ct,
            )
            .await
            .unwrap();
        assert_eq!(
            a.target_def.hash, b.target_def.hash,
            "a different host bundler path for the same effective toolchain must not bust the cache"
        );
    }

    #[tokio::test]
    async fn parse_declares_one_dir_output_and_caches_locally_and_remotely() {
        let ct = ctoken();
        let resp = driver().parse(make_parse_request(&[]), &ct).await.unwrap();
        assert_eq!(resp.target_def.outputs.len(), 1);
        assert!(matches!(
            &resp.target_def.outputs[0].paths[0].content,
            Content::DirPath(p) if p == "packages/a/dist"
        ));
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
                    "js_bundle".to_string(),
                    Default::default(),
                ),
                driver: "js_bundle".to_string(),
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
                vec!["//@heph/js/thirdparty/lodash@4.17.21:js_install@os=linux,arch=amd64"],
            ),
            ("", vec!["//@heph/fs:file@f=packages/a/src/index.ts"]),
            (
                "bundler_config",
                vec!["//@heph/fs:file@f=esbuild.config.json"],
            ),
        ]);
        let resp = driver().parse(req, &ct).await.unwrap();
        let origin_ids: Vec<&str> = resp
            .target_def
            .inputs
            .iter()
            .map(|i| i.origin_id.as_str())
            .collect();
        // Sorted group order: "" < "bundler_config" < "external".
        assert_eq!(
            origin_ids,
            vec!["dep||0", "dep|bundler_config|0", "dep|external|0"]
        );
    }

    // ---- run(): gated on a real esbuild binary being available in this devenv ----
    //
    // Everything above tests cache-key/Input-declaration behavior and needs no
    // real bundler. These exercise the actual subprocess invocation and its
    // success/failure surfacing (task requirement 7) — they require a real
    // `esbuild` (checked via PATH / `node_modules/.bin` convention) and are
    // #[ignore]d, with an honest message, when it isn't present, rather than
    // silently skipping.

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

    #[tokio::test]
    #[ignore = "requires a real `esbuild` on PATH — devenv.nix provisions no Node/esbuild \
                toolchain (see toolchain.rs module docs); run explicitly with \
                `cargo test -- --ignored` on a host with esbuild installed"]
    async fn run_succeeds_and_produces_expected_output() {
        let esbuild = find_real_bin("esbuild").expect(
            "this test is #[ignore]d precisely because esbuild isn't guaranteed on PATH — it \
             was run explicitly, so a missing esbuild here is a real failure, not a skip",
        );

        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const greeting: string = 'hello';\nconsole.log(greeting);\n",
        );

        let ct = ctoken();
        let req = make_parse_request(&[(
            "bundler_bin",
            Value::String(esbuild.to_string_lossy().into_owned()),
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
            .expect("a well-formed entry point must bundle successfully");

        let output = std::fs::read_to_string(dir.path().join("packages/a/dist/index.js"))
            .expect("bundled output file must exist");
        assert!(
            output.contains("hello"),
            "bundled output must contain the entry point's own content: {output}"
        );
    }

    #[tokio::test]
    #[ignore = "requires a real `esbuild` on PATH — devenv.nix provisions no Node/esbuild \
                toolchain (see toolchain.rs module docs); run explicitly with \
                `cargo test -- --ignored` on a host with esbuild installed"]
    async fn run_fails_and_surfaces_bundler_output_on_an_unresolvable_import() {
        let esbuild = find_real_bin("esbuild").expect(
            "this test is #[ignore]d precisely because esbuild isn't guaranteed on PATH — it \
             was run explicitly, so a missing esbuild here is a real failure, not a skip",
        );

        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "import { missing } from './does-not-exist';\nconsole.log(missing);\n",
        );

        let ct = ctoken();
        let req = make_parse_request(&[(
            "bundler_bin",
            Value::String(esbuild.to_string_lossy().into_owned()),
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
            .expect("an unresolvable import must fail the driver, not succeed silently");
        let msg = format!("{err:#}");
        assert!(msg.contains("js_bundle failed"), "{msg}");
        assert!(
            msg.contains("does-not-exist") || msg.contains("Could not resolve"),
            "{msg}"
        );
    }

    /// Feature-quality M6 review BLOCKER: `--external:<name>` flags were
    /// never derived from the discovered third-party closure, so every real
    /// npm dependency (never on disk on a fresh checkout, and never
    /// hand-enumerated in an `esbuild.config.json`) made a real `esbuild
    /// --bundle` hard-fail with "Could not resolve". `lodash` here is
    /// deliberately never installed on disk — proving the entry bundles
    /// successfully with no `node_modules/lodash` present at all, and that
    /// the import survives un-inlined (esbuild never attempts to resolve an
    /// externalized specifier, so its absence from disk is exactly the
    /// point).
    #[tokio::test]
    #[ignore = "requires a real `esbuild` on PATH — devenv.nix provisions no Node/esbuild \
                toolchain (see toolchain.rs module docs); run explicitly with \
                `cargo test -- --ignored` on a host with esbuild installed"]
    async fn run_succeeds_with_a_real_third_party_import_marked_external() {
        let esbuild = find_real_bin("esbuild").expect(
            "this test is #[ignore]d precisely because esbuild isn't guaranteed on PATH — it \
             was run explicitly, so a missing esbuild here is a real failure, not a skip",
        );

        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "import _ from 'lodash';\nconsole.log(_);\n",
        );
        // Deliberately no `node_modules/lodash` anywhere on disk.

        let ct = ctoken();
        let req = make_parse_request(&[
            (
                "bundler_bin",
                Value::String(esbuild.to_string_lossy().into_owned()),
            ),
            (
                "external",
                Value::List(vec![Value::String("lodash".to_string())]),
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
        driver().run(run_req, &ct).await.expect(
            "a third-party import marked external must bundle successfully with no \
             node_modules present at all",
        );

        let output = std::fs::read_to_string(dir.path().join("packages/a/dist/index.js"))
            .expect("bundled output file must exist");
        assert!(
            output.contains("lodash"),
            "the externalized import must survive un-inlined in the output, not be silently \
             dropped or inlined as if it resolved: {output}"
        );
    }
}
