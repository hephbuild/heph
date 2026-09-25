//! Node-runtime-accurate module resolution via `oxc_resolver`, configured for
//! **two separate graph flavors** per `ai-docs/js-plugin-plan.md`'s
//! "Dependency graph" section:
//!
//! - **Runtime graph**: what `require(...)`/`import ... from`/`import(...)`
//!   actually load at runtime. Two condition sets, chosen by the *syntax*
//!   used at the call site (see `importparse.rs`'s [`ModuleContext`] doc),
//!   not by the file's own module type — Node's dynamic `import()` always
//!   resolves as ESM even from a CommonJS file, and `require()` always
//!   resolves as CJS even from an ESM file (e.g. via `createRequire`):
//!   - ESM context: `condition_names = ["node", "import"]`
//!   - CJS context: `condition_names = ["node", "require"]`
//!
//! - **Type graph**: `import type` / `export type` (see `importparse.rs`),
//!   resolved via `oxc_resolver`'s dedicated [`Resolver::resolve_dts`] —
//!   TypeScript's own `ts.resolveModuleName` algorithm (`"bundler"` module
//!   resolution), which independently already handles `.d.ts`/`.d.mts`/
//!   `.d.cts` extension priority and the `@types/<pkg>` scoped-name-mangling
//!   fallback (`@babel/core` → `@types/babel__core`) that plain Node
//!   resolution knows nothing about. `condition_names` still has to be set
//!   on the resolver instance passed to `resolve_dts` for conditional
//!   `exports` maps to pick the `"types"` branch (`resolve_dts`'s own doc
//!   says "`types` is always added", but the underlying
//!   `package_exports_resolve` reads `ResolveOptions::condition_names`
//!   verbatim with no ambient injection — verified by reading the
//!   `oxc_resolver` source rather than trusting the doc comment literally),
//!   so this crate builds it in explicitly:
//!   `condition_names = ["types", "node", "import"]` (type-only import
//!   syntax — `import type`/`export type ... from` — is exclusively an ESM
//!   construct; there is no CJS equivalent, so no CJS type resolver exists).
//!
//! A type-only import of the same specifier as a runtime import can
//! genuinely resolve to a *different* file or package (the flagship example
//! being a package with `"exports": {"types": "./dist/index.d.ts",
//! "import": "./dist/index.mjs"}}`, or the entire `@types/*` fallback for a
//! package that ships no types of its own) — this is exactly why
//! `importgraph.rs` keeps two separate edge lists rather than one graph with
//! a boolean flag.
//!
//! ## Extensions / extension_alias
//!
//! Runtime resolution here is intentionally more permissive than a strict
//! Node ESM loader (which requires a fully-specified extension for ESM
//! imports): first-party heph packages are TypeScript source, not the
//! post-build output Node would actually run, so `extensions` includes
//! `.ts`/`.tsx`/`.mts`/`.cts` and `extension_alias` maps an explicit `.js`
//! specifier extension to a same-named `.ts`/`.tsx` file — the well-known
//! "TypeScript lets you write `import './foo.js'` for a file that is
//! actually `./foo.ts`" convention (`allowImportingTsExtensions`-adjacent).
//! This is a deliberate relaxation from strict runtime semantics: heph's
//! resolver informs the dependency graph and cache key, not a guarantee of
//! what the eventual real toolchain (tsc/bundler/test runner) will accept —
//! see `ai-docs/js-plugin-plan.md`'s "Correctness safety valve".
//!
//! ## tsconfig
//!
//! `paths`/`baseUrl`/`extends` are handled by pointing `ResolveOptions::tsconfig`
//! at the nearest `tsconfig.json` found by walking up from the package
//! directory (see `importgraph.rs::find_nearest_tsconfig`) with
//! `TsconfigReferences::Disabled` — composite-project **references** (a
//! separate tsconfig pulling in another project's output) are not followed.
//! TODO M2+: wire `TsconfigReferences::Auto` once a concrete composite-project
//! fixture motivates it; scoped out here to keep the resolver's blast radius
//! bounded to what this milestone's tests actually exercise.
//!
//! ## Node builtins
//!
//! `builtin_modules: true` on every resolver: a specifier naming a Node
//! builtin (`"fs"`, `"node:fs"`, ...) resolves to [`ResolveOutcome::Builtin`]
//! rather than attempting (and failing, or worse, wrongly succeeding against
//! a same-named `node_modules` package) a filesystem resolution — builtins
//! are never a heph dependency edge and never a phantom-dependency
//! candidate.

use oxc_resolver::{
    ResolveError, ResolveOptions, Resolver, TsconfigDiscovery, TsconfigOptions, TsconfigReferences,
};
use std::path::{Path, PathBuf};

pub use crate::pluginjs::importparse::ModuleContext;

/// Outcome of resolving one specifier. Never an `Err`: a resolution failure
/// that isn't a recognized Node builtin is [`ResolveOutcome::Unresolved`],
/// not a propagated error — see `importgraph.rs` module docs for why an
/// unresolvable specifier is deliberately not a hard failure at this layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveOutcome {
    Resolved(PathBuf),
    /// A Node builtin module (`fs`, `node:fs`, ...) — never a heph edge.
    Builtin,
    /// Could not be resolved on disk. Logged, not propagated — see
    /// `importgraph.rs` module docs.
    Unresolved,
}

const EXTENSIONS: &[&str] = &[
    ".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs", ".json", ".node",
];

fn extension_alias() -> Vec<(String, Vec<String>)> {
    vec![
        (
            ".js".to_string(),
            vec![".ts".to_string(), ".tsx".to_string(), ".js".to_string()],
        ),
        (
            ".jsx".to_string(),
            vec![".tsx".to_string(), ".jsx".to_string()],
        ),
        (
            ".mjs".to_string(),
            vec![".mts".to_string(), ".mjs".to_string()],
        ),
        (
            ".cjs".to_string(),
            vec![".cts".to_string(), ".cjs".to_string()],
        ),
    ]
}

fn tsconfig_discovery(tsconfig: Option<&Path>) -> Option<TsconfigDiscovery> {
    tsconfig.map(|p| {
        TsconfigDiscovery::Manual(TsconfigOptions {
            config_file: p.to_path_buf(),
            references: TsconfigReferences::Disabled,
        })
    })
}

fn base_options(tsconfig: Option<&Path>) -> ResolveOptions {
    ResolveOptions {
        extensions: EXTENSIONS.iter().map(|s| (*s).to_string()).collect(),
        extension_alias: extension_alias(),
        builtin_modules: true,
        tsconfig: tsconfig_discovery(tsconfig),
        // `ResolveOptions::default()` sets `node_path: true`, which makes
        // `oxc_resolver` read the ambient `NODE_PATH` env var and append its
        // entries as extra module-search roots — an undeclared, unhashed
        // input that would make resolution (and therefore the
        // phantom-dependency check) depend on host environment state, not
        // just the declared source/lockfile/package.json. `NODE_PATH` is
        // also a legacy, non-ESM-standard Node feature not needed for this
        // milestone's scope, so it's turned off explicitly here rather than
        // inherited from the library default.
        node_path: false,
        ..ResolveOptions::default()
    }
}

/// The three resolvers this milestone needs, built once per (workspace
/// package, nearest tsconfig) pair — see [`GraphFlavor`]/module docs for why
/// three and not one or four.
pub struct Resolvers {
    runtime_esm: Resolver,
    runtime_cjs: Resolver,
    types: Resolver,
}

impl Resolvers {
    pub fn new(tsconfig: Option<&Path>) -> Self {
        let runtime_esm = Resolver::new(ResolveOptions {
            condition_names: vec!["node".to_string(), "import".to_string()],
            main_fields: vec!["main".to_string()],
            ..base_options(tsconfig)
        });
        let runtime_cjs = Resolver::new(ResolveOptions {
            condition_names: vec!["node".to_string(), "require".to_string()],
            main_fields: vec!["main".to_string()],
            ..base_options(tsconfig)
        });
        // Type-only import syntax (`import type` / `export type ... from`) is
        // exclusively an ESM construct, so one types resolver suffices — see
        // module docs.
        let types = Resolver::new(ResolveOptions {
            condition_names: vec![
                "types".to_string(),
                "node".to_string(),
                "import".to_string(),
            ],
            main_fields: vec![
                "types".to_string(),
                "typings".to_string(),
                "main".to_string(),
            ],
            ..base_options(tsconfig)
        });
        Self {
            runtime_esm,
            runtime_cjs,
            types,
        }
    }

    /// Resolve a runtime (value) specifier as seen from a file in directory
    /// `dir` (the file's own parent directory — Node's `__dirname`/
    /// `import.meta.url` directory).
    pub fn resolve_runtime(
        &self,
        context: ModuleContext,
        dir: &Path,
        specifier: &str,
    ) -> ResolveOutcome {
        let resolver = match context {
            ModuleContext::Esm => &self.runtime_esm,
            ModuleContext::Cjs => &self.runtime_cjs,
        };
        outcome_of(resolver.resolve(dir, specifier), dir, specifier)
    }

    /// Resolve a type-only specifier as seen from `file` (the importing file
    /// itself — `resolve_dts` takes the file, not its directory, since it
    /// needs the file's own path for tsconfig discovery bookkeeping upstream;
    /// this crate always passes an already-known tsconfig, so that part is a
    /// no-op, but the file path is still what the API expects).
    pub fn resolve_types(&self, file: &Path, specifier: &str) -> ResolveOutcome {
        outcome_of(self.types.resolve_dts(file, specifier), file, specifier)
    }
}

fn outcome_of(
    result: Result<oxc_resolver::Resolution, ResolveError>,
    from: &Path,
    specifier: &str,
) -> ResolveOutcome {
    match result {
        Ok(resolution) => ResolveOutcome::Resolved(resolution.path().to_path_buf()),
        Err(ResolveError::Builtin { .. }) => ResolveOutcome::Builtin,
        Err(err) => {
            tracing::debug!(
                from = %from.display(),
                specifier = specifier,
                error = %err,
                "js import resolve: unresolved (coarsened, not a hard error — see importgraph.rs)"
            );
            ResolveOutcome::Unresolved
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dir");
        }
        fs::write(path, contents).expect("write fixture file");
    }

    /// Canonicalize the tempdir root before using it as a resolution base or
    /// building an expected result path. Necessary on macOS, where `$TMPDIR`
    /// itself sits behind a symlink (`/tmp` -> `/private/tmp`) that
    /// `oxc_resolver`'s default `symlinks: true` (matching Node's own
    /// realpath-following behavior) resolves through -- without this, every
    /// expected path here would mismatch the resolver's actual (correct)
    /// output by exactly that symlink hop.
    fn root(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().canonicalize().expect("canonicalize tempdir")
    }

    #[test]
    fn resolves_relative_ts_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&root(&dir), "src/foo.ts", "export const x = 1;");
        let resolvers = Resolvers::new(None);
        let outcome =
            resolvers.resolve_runtime(ModuleContext::Esm, &root(&dir).join("src"), "./foo");
        assert_eq!(
            outcome,
            ResolveOutcome::Resolved(root(&dir).join("src/foo.ts"))
        );
    }

    #[test]
    fn extension_alias_resolves_js_specifier_to_ts_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&root(&dir), "src/foo.ts", "export const x = 1;");
        let resolvers = Resolvers::new(None);
        let outcome =
            resolvers.resolve_runtime(ModuleContext::Esm, &root(&dir).join("src"), "./foo.js");
        assert_eq!(
            outcome,
            ResolveOutcome::Resolved(root(&dir).join("src/foo.ts"))
        );
    }

    #[test]
    fn builtin_module_is_recognized() {
        let dir = tempfile::tempdir().expect("tempdir");
        let resolvers = Resolvers::new(None);
        let outcome = resolvers.resolve_runtime(ModuleContext::Cjs, &root(&dir), "fs");
        assert_eq!(outcome, ResolveOutcome::Builtin);
    }

    #[test]
    fn unresolvable_specifier_is_unresolved_not_a_panic_or_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let resolvers = Resolvers::new(None);
        let outcome = resolvers.resolve_runtime(ModuleContext::Esm, &root(&dir), "totally-missing");
        assert_eq!(outcome, ResolveOutcome::Unresolved);
    }

    /// A conditional `exports` map, resolved correctly for both ESM and CJS
    /// consuming contexts — required test per the task.
    #[test]
    fn conditional_exports_resolves_differently_for_esm_and_cjs() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &root(&dir),
            "node_modules/pkg/package.json",
            r#"{
                "name": "pkg",
                "exports": {
                    ".": {
                        "import": "./esm.mjs",
                        "require": "./cjs.cjs",
                        "default": "./esm.mjs"
                    }
                }
            }"#,
        );
        write(
            &root(&dir),
            "node_modules/pkg/esm.mjs",
            "export const x = 1;",
        );
        write(
            &root(&dir),
            "node_modules/pkg/cjs.cjs",
            "module.exports.x = 1;",
        );

        let resolvers = Resolvers::new(None);
        let esm = resolvers.resolve_runtime(ModuleContext::Esm, &root(&dir), "pkg");
        let cjs = resolvers.resolve_runtime(ModuleContext::Cjs, &root(&dir), "pkg");
        assert_eq!(
            esm,
            ResolveOutcome::Resolved(root(&dir).join("node_modules/pkg/esm.mjs"))
        );
        assert_eq!(
            cjs,
            ResolveOutcome::Resolved(root(&dir).join("node_modules/pkg/cjs.cjs"))
        );
    }

    #[test]
    fn types_resolver_prefers_declaration_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &root(&dir),
            "node_modules/pkg/package.json",
            r#"{"name": "pkg", "main": "index.js", "types": "index.d.ts"}"#,
        );
        write(
            &root(&dir),
            "node_modules/pkg/index.js",
            "module.exports.x = 1;",
        );
        write(
            &root(&dir),
            "node_modules/pkg/index.d.ts",
            "export declare const x: number;",
        );

        let resolvers = Resolvers::new(None);
        let outcome = resolvers.resolve_types(&root(&dir).join("src/consumer.ts"), "pkg");
        assert_eq!(
            outcome,
            ResolveOutcome::Resolved(root(&dir).join("node_modules/pkg/index.d.ts"))
        );
    }

    /// The `@types/<pkg>` DefinitelyTyped fallback for a package that ships
    /// no types of its own — `oxc_resolver`'s `resolve_dts` handles this
    /// natively (including `@scope/name` → `@types/scope__name` mangling).
    #[test]
    fn types_resolver_falls_back_to_at_types_package() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &root(&dir),
            "node_modules/untyped-pkg/package.json",
            r#"{"name": "untyped-pkg", "main": "index.js"}"#,
        );
        write(
            &root(&dir),
            "node_modules/untyped-pkg/index.js",
            "module.exports.x = 1;",
        );
        write(
            &root(&dir),
            "node_modules/@types/untyped-pkg/package.json",
            r#"{"name": "@types/untyped-pkg", "types": "index.d.ts"}"#,
        );
        write(
            &root(&dir),
            "node_modules/@types/untyped-pkg/index.d.ts",
            "export declare const x: number;",
        );

        let resolvers = Resolvers::new(None);
        let outcome = resolvers.resolve_types(&root(&dir).join("src/consumer.ts"), "untyped-pkg");
        assert_eq!(
            outcome,
            ResolveOutcome::Resolved(root(&dir).join("node_modules/@types/untyped-pkg/index.d.ts"))
        );
    }

    /// `ResolveOptions::default()` sets `node_path: true`; `base_options`
    /// must override it to `false` so the ambient `NODE_PATH` env var (an
    /// undeclared, unhashed input that would otherwise make resolution
    /// depend on host environment state) is never consulted. See
    /// `base_options`' doc comment.
    #[test]
    fn node_path_env_var_is_not_consulted() {
        let node_path_root = tempfile::tempdir().expect("tempdir");
        write(
            &root(&node_path_root),
            "leaked-pkg/package.json",
            r#"{"name": "leaked-pkg", "main": "index.js"}"#,
        );
        write(
            &root(&node_path_root),
            "leaked-pkg/index.js",
            "module.exports = {};",
        );
        let dir = tempfile::tempdir().expect("tempdir");

        let prior = std::env::var_os("NODE_PATH");
        // SAFETY: test-only, single-threaded within this process for the
        // duration of the mutation; restored immediately below regardless of
        // outcome via the `prior` save/restore.
        unsafe { std::env::set_var("NODE_PATH", root(&node_path_root)) };
        let resolvers = Resolvers::new(None);
        let outcome = resolvers.resolve_runtime(ModuleContext::Cjs, &root(&dir), "leaked-pkg");
        match &prior {
            // SAFETY: test-only, restoring the prior value we saved above.
            Some(v) => unsafe { std::env::set_var("NODE_PATH", v) },
            // SAFETY: test-only, restoring the prior (unset) state.
            None => unsafe { std::env::remove_var("NODE_PATH") },
        }

        assert_eq!(
            outcome,
            ResolveOutcome::Unresolved,
            "resolution must not depend on the ambient NODE_PATH env var"
        );
    }

    #[test]
    fn tsconfig_paths_are_respected() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &root(&dir),
            "tsconfig.json",
            r#"{
                "compilerOptions": {
                    "baseUrl": ".",
                    "paths": { "@app/*": ["src/*"] }
                }
            }"#,
        );
        write(&root(&dir), "src/widget.ts", "export const w = 1;");

        let resolvers = Resolvers::new(Some(&root(&dir).join("tsconfig.json")));
        let outcome = resolvers.resolve_runtime(ModuleContext::Esm, &root(&dir), "@app/widget");
        assert_eq!(
            outcome,
            ResolveOutcome::Resolved(root(&dir).join("src/widget.ts"))
        );
    }
}
