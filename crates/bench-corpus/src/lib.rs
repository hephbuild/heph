//! Deterministic synthetic-corpus generator for `heph-bench`.
//!
//! Produces a workspace tree of plain `bash`-driver targets arranged as a
//! layered DAG (bounded fan-out, no cycles), plus optional `go/` and `js/`
//! subtrees for scenarios that must cross the real plugin-cdylib seam. The
//! go packages are discovered from `go.mod`; the only BUILD file written
//! there declares the build variant, without which the go provider lists no
//! build targets at all (see `write_go_variant_build`). The js packages are
//! a hermetic pnpm workspace, auto-discovered by the js provider with no
//! BUILD file needed at all.
//!
//! Same seed + same params => byte-identical tree. A CI run generates the
//! corpus once (from the PR-head generator) and points both the baseline and
//! candidate binaries at it, so the generator never needs to be stable across
//! commits — only within one comparison run.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Minimal deterministic PRNG (SplitMix64). The generator only needs
/// reproducible index/count selection, not cryptographic or statistical
/// quality, so a hand-rolled generator avoids pulling in `rand` for a new,
/// still-unreviewed crate.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..bound`. `bound == 0` always returns 0.
    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next_u64() % bound as u64) as usize
        }
    }
}

#[derive(Clone, Debug)]
pub struct CorpusParams {
    pub seed: u64,
    /// Total number of `bash` targets in the layered DAG.
    pub target_count: usize,
    /// Number of BUILD packages the targets are spread across.
    pub packages: usize,
    /// DAG depth: targets are split evenly across this many layers, each
    /// depending only on the layer below it.
    pub layers: usize,
    /// Max deps per target, drawn from the layer immediately below.
    pub fan_out: usize,
    /// Number of Go packages to generate under `go/` via `tools/gorepogen`
    /// (Tier B). 0 = no go subtree.
    pub go_packages: usize,
    /// `tools/gorepogen -max-depth`.
    pub go_max_depth: usize,
    /// Path to the `gorepogen` module (the `tools/gorepogen` directory
    /// containing its `go.mod`). Defaults to the copy in this workspace via
    /// [`default_gorepogen_dir`].
    pub gorepogen_dir: PathBuf,
    /// Number of JS/TS packages to generate under `js/` (Tier B, plugin-js).
    /// 0 = no js subtree. Unlike `go_packages`, generation never shells out
    /// to an external tool — see [`generate_js_tree`]'s doc comment for why
    /// there is nothing external to resolve.
    pub js_packages: usize,
    /// Max nesting depth, in directories below `js/packages/`, a generated
    /// package can sit at — mirrors `go_max_depth`'s role of bounding tree
    /// shape, translated into `pnpm-workspace.yaml` glob patterns (one
    /// literal `*` path segment per depth level: `packages/*`,
    /// `packages/*/*`, ...) since pnpm globs don't cross directory
    /// boundaries (see `crates/plugin-js/src/pluginjs/workspace.rs`'s
    /// `resolve_members` doc comment).
    pub js_max_depth: usize,
}

impl Default for CorpusParams {
    fn default() -> Self {
        Self {
            seed: 0,
            target_count: 1000,
            packages: 100,
            layers: 6,
            fan_out: 3,
            go_packages: 0,
            go_max_depth: 4,
            gorepogen_dir: default_gorepogen_dir(),
            js_packages: 0,
            js_max_depth: 4,
        }
    }
}

/// `tools/gorepogen`, resolved relative to this crate's location in the
/// workspace (`crates/bench-corpus/../../tools/gorepogen`) — not the
/// process's cwd, which a CLI invocation may have anywhere.
pub fn default_gorepogen_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/gorepogen")
}

/// File name `generate` persists the manifest under, inside the corpus root
/// — so a `run` invocation in a separate process (the normal case: corpus
/// generation and Tier A/B runs are separate `heph-bench` invocations) can
/// recover it with [`load_manifest`].
pub const MANIFEST_FILE: &str = ".bench-manifest.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CorpusManifest {
    /// Addrs of every generated bash target, in generation order.
    pub bash_addrs: Vec<String>,
    /// Package-path prefix (for `Matcher::PackagePrefix`) covering every
    /// generated bash package. Always `""` — bash packages sit at the corpus
    /// root, the go subtree under `go/`, so an empty prefix matches only the
    /// former today; kept explicit rather than assumed so callers don't have
    /// to know that layout.
    pub bash_prefix: String,
    /// Number of go packages generated under `go/` (0 if `go_packages == 0`).
    pub go_package_count: usize,
    /// Package-path prefix covering the generated go subtree (`"go"`), for a
    /// `//go/...`-shaped build against real Go packages, dlopening the real
    /// plugin cdylib.
    pub go_prefix: String,
    /// The BUILD package directories that hold bash targets — the mutation
    /// unit for [`incrementalize`].
    pub bash_packages: Vec<String>,
    /// Number of js packages generated under `js/` (0 if `js_packages == 0`).
    pub js_package_count: usize,
    /// Package-path prefix covering the generated js subtree (`"js"`), for a
    /// `//js/...`-shaped build against a real pnpm workspace, dlopening the
    /// real js plugin cdylib.
    pub js_prefix: String,
}

struct Layered {
    /// addr per target, indexed by global target index.
    addrs: Vec<String>,
    /// layer index per target, indexed by global target index.
    layer_of: Vec<usize>,
    /// target indices per layer.
    by_layer: Vec<Vec<usize>>,
}

fn layer_targets(target_count: usize, layers: usize, packages: usize) -> Layered {
    let layers = layers.max(1);
    let packages = packages.max(1);
    let mut addrs = Vec::with_capacity(target_count);
    let mut layer_of = Vec::with_capacity(target_count);
    let mut by_layer = vec![Vec::new(); layers];

    for i in 0..target_count {
        let layer = i % layers;
        let pkg = i % packages;
        addrs.push(format!("//pkg{pkg}:t{i}"));
        layer_of.push(layer);
        by_layer
            .get_mut(layer)
            .expect("layer = i % layers is always < by_layer.len() == layers")
            .push(i);
    }

    Layered {
        addrs,
        layer_of,
        by_layer,
    }
}

/// Write the layered bash-target DAG and (if requested) the go subtree under
/// `root`. `root` must already exist.
pub fn generate(params: &CorpusParams, root: &Path) -> Result<CorpusManifest> {
    let mut rng = Rng::new(params.seed);
    let g = layer_targets(params.target_count, params.layers, params.packages);

    // package index -> BUILD file source being accumulated.
    let mut pkg_src: Vec<String> = vec![String::new(); params.packages.max(1)];
    let mut bash_packages = Vec::new();

    for i in 0..params.target_count {
        let layer = *g
            .layer_of
            .get(i)
            .expect("i < target_count == layer_of.len() by loop bound");
        let pkg = i % params.packages.max(1);
        let deps = if layer == 0 {
            Vec::new()
        } else {
            let below = g
                .by_layer
                .get(layer - 1)
                .expect("layer > 0 here, so layer - 1 < layers == by_layer.len()");
            let n = params.fan_out.min(below.len());
            let mut picked = Vec::with_capacity(n);
            for _ in 0..n {
                let idx = *below
                    .get(rng.below(below.len()))
                    .expect("rng.below(below.len()) < below.len()");
                let addr = g
                    .addrs
                    .get(idx)
                    .expect("idx is a target index from by_layer, always < addrs.len()");
                if !picked.contains(addr) {
                    picked.push(addr.clone());
                }
            }
            picked
        };

        let src = pkg_src
            .get_mut(pkg)
            .expect("pkg = i % packages.max(1) < pkg_src.len() == packages.max(1)");
        write!(
            src,
            "target(\n    name = \"t{i}\",\n    driver = \"bash\",\n"
        )
        .context("format target")?;
        // The run body never reads dep content — dependency cost (DAG
        // wiring, hashing, cache-key propagation) comes entirely from the
        // `deps` field below; the script itself only needs to produce a
        // unique, cheap output.
        writeln!(src, "    run = \"echo {i} > $OUT\",").context("format run")?;
        if !deps.is_empty() {
            let dep_list = deps
                .iter()
                .map(|a| format!("\"{a}\""))
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(src, "    deps = [{dep_list}],").context("format deps")?;
        }
        // Unique per target, not a fixed "out": two sibling targets from the
        // same package can both be deps of one downstream consumer, and the
        // sandbox mounts each dep's output at ws/<pkg>/<out-path> — a shared
        // literal path collides the moment that happens (caught by an actual
        // `heph r build //...` run against the generated corpus).
        writeln!(src, "    out = \"t{i}.out\",\n)\n").context("format out")?;
    }

    for (pkg, src) in pkg_src.iter().enumerate() {
        if src.is_empty() {
            continue;
        }
        let pkg_dir = root.join(format!("pkg{pkg}"));
        std::fs::create_dir_all(&pkg_dir)
            .with_context(|| format!("create {}", pkg_dir.display()))?;
        std::fs::write(pkg_dir.join("BUILD"), src)
            .with_context(|| format!("write {}/BUILD", pkg_dir.display()))?;
        bash_packages.push(format!("pkg{pkg}"));
    }

    let go_package_count = if params.go_packages > 0 {
        generate_go_tree(params, root)?;
        params.go_packages
    } else {
        0
    };

    let js_package_count = if params.js_packages > 0 {
        generate_js_tree(params, root)?;
        params.js_packages
    } else {
        0
    };

    let manifest = CorpusManifest {
        bash_addrs: g.addrs,
        bash_prefix: String::new(),
        go_package_count,
        go_prefix: "go".to_string(),
        bash_packages,
        js_package_count,
        js_prefix: "js".to_string(),
    };
    save_manifest(&manifest, root)?;
    Ok(manifest)
}

/// Persist `manifest` as `<root>/.bench-manifest.json`. Called automatically
/// by [`generate`]; exposed for callers that build a [`CorpusManifest`] some
/// other way.
pub fn save_manifest(manifest: &CorpusManifest, root: &Path) -> Result<()> {
    let path = root.join(MANIFEST_FILE);
    let bytes = serde_json::to_vec_pretty(manifest).context("encode corpus manifest")?;
    std::fs::write(&path, bytes).with_context(|| format!("write {}", path.display()))
}

/// Load a manifest [`generate`] wrote for `root`.
pub fn load_manifest(root: &Path) -> Result<CorpusManifest> {
    let path = root.join(MANIFEST_FILE);
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

/// A `go/` subtree the go provider discovers from `go.mod` — generated by
/// `tools/gorepogen` rather than hand-rolled here,
/// so the corpus and any Go-side stress testing share one generator instead
/// of two drifting implementations. Note gorepogen's packages are libraries
/// (no `package main`); the go provider still builds/compiles every one,
/// which is what exercises the real go plugin cdylib.
fn generate_go_tree(params: &CorpusParams, root: &Path) -> Result<()> {
    let go_root = root.join("go");
    std::fs::create_dir_all(&go_root).context("create go/")?;
    // Absolute: the invocation below runs with cwd = gorepogen_dir (so `go
    // run` finds gorepogen's own go.mod, see the comment there), which would
    // otherwise resolve a relative `-out` against the wrong directory.
    let go_root_abs = go_root
        .canonicalize()
        .with_context(|| format!("canonicalize {}", go_root.display()))?;

    // `go run <dir>` does NOT make `<dir>` the module context — Go still
    // resolves the main module by walking up from the process's cwd, so
    // running this from the corpus root (no go.mod there) fails with "cannot
    // find main module". Run with cwd = gorepogen_dir instead, so `.`
    // resolves gorepogen's own go.mod.
    let status = std::process::Command::new("go")
        .current_dir(&params.gorepogen_dir)
        .arg("run")
        .arg(".")
        .arg("-seed")
        .arg(params.seed.to_string())
        .arg("-out")
        .arg(&go_root_abs)
        .arg("-module")
        .arg("heph.bench/corpus")
        .arg("-pkgs")
        .arg(params.go_packages.to_string())
        .arg("-max-depth")
        .arg(params.go_max_depth.to_string())
        .status()
        .context("spawn `go run tools/gorepogen`")?;
    if !status.success() {
        anyhow::bail!("gorepogen exited with {status}");
    }

    // Resolve third-party deps now, at generation time — not during a
    // measured Tier B rep, and not deferred to the hermetic go toolchain's
    // first build (which would fold network/module-cache latency into
    // whatever scenario happens to run first).
    let status = std::process::Command::new("go")
        .arg("mod")
        .arg("tidy")
        .current_dir(&go_root)
        .status()
        .context("spawn `go mod tidy` on generated corpus")?;
    if !status.success() {
        anyhow::bail!("go mod tidy exited with {status}");
    }

    write_go_variant_build(&go_root)?;

    Ok(())
}

/// The BUILD file [`write_go_variant_build`] installs, kept as real Starlark in
/// this crate's source rather than a Rust string literal: it is constant, and a
/// `.BUILD` file gets syntax highlighting, formatting and review as the language
/// it actually is instead of one escaped `\"` at a time.
const GO_VARIANT_BUILD: &str = include_str!("./go_variant.BUILD");

/// Declare the `host` Go build variant at the corpus module root.
///
/// The go provider has **no implicit default variant**: `ProviderInner::list`
/// emits no build targets for a package with no variant in ancestry. Without
/// this file the whole `go/` subtree lists nothing, so a Tier B scenario
/// matched zero targets, built nothing, and still exited 0 — a perf gate that
/// reads as passing while timing only package discovery.
///
/// The platform is resolved by `heph.core.os()` / `heph.core.arch()` at BUILD
/// evaluation time rather than baked in here. Two reasons: this crate promises
/// `same seed + same params => byte-identical tree`, which a host-dependent
/// literal would break; and the same generated corpus can then be measured on
/// any of the three supported targets without regenerating it. Both builtins
/// already return canonical Go naming (`darwin`/`linux`, `arm64`/`amd64`), so
/// they drop straight into the variant's factors.
///
/// Written after `go mod tidy` so a stray BUILD file is never in the tree while
/// the go tooling walks it.
fn write_go_variant_build(go_root: &Path) -> Result<()> {
    let build = go_root.join("BUILD");
    std::fs::write(&build, GO_VARIANT_BUILD)
        .with_context(|| format!("write {}", build.display()))?;
    Ok(())
}

/// A `js/` subtree the js provider auto-discovers via `pnpm-workspace.yaml` +
/// each package's own `package.json` (no BUILD file) — generated directly in
/// Rust rather than shelled out to an external tool, unlike
/// [`generate_go_tree`]. There is nothing to resolve at generation (or
/// measurement) time: the generated tree declares zero third-party npm
/// dependencies — no `dependencies`/`devDependencies` entry names a real npm
/// package at all — so every Tier B JS scenario stays fully offline (no
/// `js_install` network fetch, no lockfile needed), the same "resolve once,
/// up front" principle `generate_go_tree`'s `go mod tidy` establishes, just
/// simpler here since there is nothing external at all. Cross-package edges
/// are relative TypeScript `import` statements instead, so the module DAG is
/// real without a package manager ever needing to resolve anything.
///
/// Package layout mirrors the bash-target DAG's own shape
/// ([`layer_targets`]/[`generate`]): `params.layers` layers, each package
/// depending on up to `params.fan_out` packages in the layer below (picked
/// with the same [`Rng`] machinery), so js and bash corpora are structurally
/// comparable at the same params. The RNG here is seeded independently of
/// the bash-target loop's — `params.seed` XORed with a distinguishing
/// constant, same convention [`incrementalize_go`] uses — so enabling or
/// disabling `js_packages` never perturbs the bash (or go) output, and vice
/// versa.
///
/// Chose `pnpm-workspace.yaml` over npm's `package.json` `"workspaces"`
/// array (`crates/plugin-js/src/pluginjs/workspace.rs` supports both): pnpm
/// needs no root `package.json` at all — the workspace-member glob list
/// lives in its own file — one fewer file to generate, while still
/// exercising the identical glob-based discovery path the npm branch would.
fn generate_js_tree(params: &CorpusParams, root: &Path) -> Result<()> {
    let js_root = root.join("js");
    std::fs::create_dir_all(&js_root).context("create js/")?;

    let layers = params.layers.max(1);
    let max_depth = params.js_max_depth.max(1);
    let n = params.js_packages;

    // Independent of the bash-target loop's `rng` above — see doc comment.
    let mut rng = Rng::new(params.seed ^ 0xBA5E_1000_ABCD_EF01);

    // Layer and nesting depth are deterministic functions of the package
    // index (not rng-drawn), so the tree's shape never shifts under however
    // many `rng` draws the dep-picking loop below ends up making.
    let mut by_layer: Vec<Vec<usize>> = vec![Vec::new(); layers];
    let mut dirs: Vec<Vec<String>> = Vec::with_capacity(n);
    for i in 0..n {
        by_layer
            .get_mut(i % layers)
            .expect("i % layers is always < by_layer.len() == layers")
            .push(i);

        let depth = i % max_depth;
        let mut comps = vec!["packages".to_string()];
        for d in 0..depth {
            comps.push(format!("grp{d}"));
        }
        comps.push(format!("pkg{i}"));
        dirs.push(comps);
    }

    for i in 0..n {
        let layer = i % layers;
        let deps: Vec<usize> = if layer == 0 {
            Vec::new()
        } else {
            let below = by_layer
                .get(layer - 1)
                .expect("layer > 0 here, so layer - 1 < layers == by_layer.len()");
            let k = params.fan_out.min(below.len());
            let mut picked = Vec::with_capacity(k);
            for _ in 0..k {
                let idx = *below
                    .get(rng.below(below.len()))
                    .expect("rng.below(below.len()) < below.len()");
                if !picked.contains(&idx) {
                    picked.push(idx);
                }
            }
            picked
        };

        let dir_comps = dirs
            .get(i)
            .expect("i < n == dirs.len() by the loop bound above");
        let pkg_dir = js_root.join(dir_comps.join("/"));
        std::fs::create_dir_all(&pkg_dir)
            .with_context(|| format!("create {}", pkg_dir.display()))?;

        let package_json = serde_json::json!({
            "name": format!("corpus-js-pkg{i}"),
            "version": "0.0.0",
            "main": "./index.ts",
        });
        std::fs::write(
            pkg_dir.join("package.json"),
            serde_json::to_vec_pretty(&package_json).context("encode package.json")?,
        )
        .with_context(|| format!("write {}/package.json", pkg_dir.display()))?;

        let mut src = String::new();
        for (n_dep, &dep) in deps.iter().enumerate() {
            let dep_comps = dirs
                .get(dep)
                .expect("dep is a target index from by_layer, always < dirs.len()");
            let import_path = relative_ts_import(dir_comps, dep_comps);
            writeln!(
                src,
                "import {{ value as dep{n_dep} }} from \"{import_path}\";"
            )
            .context("format import")?;
        }
        // Same principle as the bash targets' `run` body: the module never
        // reads a dep's real content, only imports its exported binding —
        // dependency cost comes entirely from the import edges wiring the
        // module graph (parse, resolve, first-party closure walk), not from
        // any payload.
        let value_expr = if deps.is_empty() {
            format!("{i}")
        } else {
            let terms = (0..deps.len())
                .map(|n_dep| format!("dep{n_dep}"))
                .collect::<Vec<_>>()
                .join(" + ");
            format!("{i} + {terms}")
        };
        writeln!(src, "export const value: number = {value_expr};").context("format export")?;
        std::fs::write(pkg_dir.join("index.ts"), src)
            .with_context(|| format!("write {}/index.ts", pkg_dir.display()))?;
    }

    // One glob pattern per depth level actually reachable (0..max_depth): a
    // literal `*` path segment per nesting level, matching pnpm's own
    // non-crossing glob semantics (`packages/*` never matches
    // `packages/grp0/pkg5` — see `workspace.rs`'s `resolve_members` doc
    // comment) — every generated package is covered by exactly one line.
    let mut yaml = String::from("packages:\n");
    for d in 0..max_depth {
        let mut pattern = String::from("packages");
        for _ in 0..=d {
            pattern.push_str("/*");
        }
        writeln!(yaml, "  - \"{pattern}\"").context("format pnpm-workspace.yaml pattern")?;
    }
    std::fs::write(js_root.join("pnpm-workspace.yaml"), yaml)
        .context("write js/pnpm-workspace.yaml")?;

    Ok(())
}

/// Relative TS import specifier from the package at `from_dir` to the
/// package at `to_dir` (both directory-component lists relative to `js/`),
/// pointing at the target's `index.ts` module (extension omitted — standard
/// relative-import style). Always anchored (`./` or `../`), never a bare
/// specifier, so it is never mistaken for a package-name import.
fn relative_ts_import(from_dir: &[String], to_dir: &[String]) -> String {
    let common = from_dir
        .iter()
        .zip(to_dir.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut parts: Vec<String> = (common..from_dir.len()).map(|_| "..".to_string()).collect();
    parts.extend(to_dir.iter().skip(common).cloned());
    if parts.first().map(String::as_str) != Some("..") {
        parts.insert(0, ".".to_string());
    }
    parts.push("index".to_string());
    parts.join("/")
}

/// `ceil(len * fraction)`, clamped to `len`. The f64->usize cast is provably
/// non-negative (product of two non-negative factors, then `ceil`), but
/// clippy's `cast_sign_loss` can't see that from the call site.
#[expect(
    clippy::cast_sign_loss,
    reason = "ceil() of a non-negative product (len as f64 >= 0, fraction clamped to [0,1]) is always >= 0"
)]
fn fraction_count(len: usize, fraction: f64) -> usize {
    (((len as f64) * fraction.clamp(0.0, 1.0)).ceil() as usize).min(len)
}

/// Mutate a deterministic fraction of the generated bash packages (rewrite
/// each target's `run` line) so a re-run busts the cache for just that
/// fraction — the incremental scenario's "one file changed" step.
pub fn incrementalize(
    manifest: &CorpusManifest,
    root: &Path,
    fraction: f64,
    seed: u64,
) -> Result<usize> {
    let mut rng = Rng::new(seed ^ 0xACE1);
    let n = fraction_count(manifest.bash_packages.len(), fraction);
    let mut touched = std::collections::HashSet::new();
    while touched.len() < n {
        touched.insert(rng.below(manifest.bash_packages.len()));
    }

    for &idx in &touched {
        let pkg = manifest
            .bash_packages
            .get(idx)
            .expect("idx drawn from below.len() == bash_packages.len()");
        let path: PathBuf = root.join(pkg).join("BUILD");
        let mut src =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        src.push_str(&format!("\n# bench-mutated seed={seed}\n"));
        std::fs::write(&path, src).with_context(|| format!("rewrite {}", path.display()))?;
    }

    Ok(touched.len())
}

fn collect_go_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))? {
        let entry = entry.context("read dir entry")?;
        let path = entry.path();
        if entry.file_type().context("get file type")?.is_dir() {
            collect_go_files(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("go")
            && !path.to_string_lossy().ends_with("_test.go")
        {
            out.push(path);
        }
    }
    Ok(())
}

/// Same idea as [`incrementalize`], for the go subtree: appends a trailing
/// comment (valid anywhere in a `.go` file, changes nothing semantically) to
/// a deterministic fraction of non-test `.go` files under `go_root`. Walks
/// the tree rather than tracking names at generation time, since
/// `tools/gorepogen` picks its own package layout.
pub fn incrementalize_go(go_root: &Path, fraction: f64, seed: u64) -> Result<usize> {
    let mut files = Vec::new();
    collect_go_files(go_root, &mut files)?;
    files.sort();

    let mut rng = Rng::new(seed ^ 0x0B0A_B0AB);
    let n = fraction_count(files.len(), fraction);
    let mut touched = std::collections::HashSet::new();
    while touched.len() < n {
        touched.insert(rng.below(files.len()));
    }

    for &idx in &touched {
        let path = files
            .get(idx)
            .expect("idx drawn from files.len() in the loop above");
        let mut src =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        src.push_str(&format!("\n// bench-mutated seed={seed}\n"));
        std::fs::write(path, src).with_context(|| format!("rewrite {}", path.display()))?;
    }

    Ok(touched.len())
}

fn collect_ts_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))? {
        let entry = entry.context("read dir entry")?;
        let path = entry.path();
        if entry.file_type().context("get file type")?.is_dir() {
            collect_ts_files(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("ts") {
            out.push(path);
        }
    }
    Ok(())
}

/// Same idea as [`incrementalize_go`], for the js subtree: appends a
/// trailing top-level statement (valid anywhere in a `.ts` module, changes
/// nothing about the exported API) to a deterministic fraction of `.ts`
/// files under `js_root`. Unlike Go there is no test-file convention to
/// exclude — [`generate_js_tree`] never writes one — so, unlike
/// [`incrementalize_go`], every `.ts` file found is eligible.
pub fn incrementalize_js(js_root: &Path, fraction: f64, seed: u64) -> Result<usize> {
    let mut files = Vec::new();
    collect_ts_files(js_root, &mut files)?;
    files.sort();

    let mut rng = Rng::new(seed ^ 0xF00D_F00D_ABCD_1234);
    let n = fraction_count(files.len(), fraction);
    let mut touched = std::collections::HashSet::new();
    while touched.len() < n {
        touched.insert(rng.below(files.len()));
    }

    for &idx in &touched {
        let path = files
            .get(idx)
            .expect("idx drawn from files.len() in the loop above");
        let mut src =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        src.push_str(&format!("\n// bench-mutated seed={seed}\n"));
        std::fs::write(path, src).with_context(|| format!("rewrite {}", path.display()))?;
    }

    Ok(touched.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_variant_build_declares_the_variant_the_provider_needs() {
        let dir = tempfile::tempdir().expect("tempdir");
        super::write_go_variant_build(dir.path()).expect("write BUILD");
        let got = std::fs::read_to_string(dir.path().join("BUILD")).expect("read BUILD");

        // These three are what make the subtree list any build target at all —
        // the go provider has no implicit default variant.
        assert!(got.contains("provider_state("), "{got}");
        assert!(got.contains(r#"provider = "go""#), "{got}");
        assert!(got.contains(r#""host""#), "{got}");
        assert!(got.contains(r#""goos": heph.core.os()"#), "{got}");
        assert!(got.contains(r#""goarch": heph.core.arch()"#), "{got}");
    }

    #[test]
    fn go_variant_build_bakes_in_no_platform() {
        let dir = tempfile::tempdir().expect("tempdir");
        super::write_go_variant_build(dir.path()).expect("write BUILD");
        let got = std::fs::read_to_string(dir.path().join("BUILD")).expect("read BUILD");

        // A literal platform would break this crate's "same seed + same params
        // => byte-identical tree" promise, and would pin a corpus to the host
        // that generated it. `heph.core.os()`/`arch()` resolve it at evaluation
        // time instead.
        for literal in [
            "darwin", "linux", "arm64", "amd64", "x86_64", "aarch64", "macos",
        ] {
            assert!(
                !got.contains(&format!(r#""{literal}""#)),
                "BUILD must name no concrete platform, found {literal:?}:\n{got}"
            );
        }
    }

    #[test]
    fn generate_is_deterministic() {
        let params = CorpusParams {
            seed: 42,
            target_count: 50,
            packages: 5,
            layers: 4,
            fan_out: 2,
            ..Default::default()
        };

        let a = tempfile::tempdir().expect("tempdir");
        let b = tempfile::tempdir().expect("tempdir");
        generate(&params, a.path()).expect("generate a");
        generate(&params, b.path()).expect("generate b");

        let read_all = |root: &Path| -> Vec<(String, String)> {
            let mut out = Vec::new();
            for entry in std::fs::read_dir(root).expect("read_dir") {
                let entry = entry.expect("dir entry");
                let build = entry.path().join("BUILD");
                if build.is_file() {
                    out.push((
                        entry.file_name().to_string_lossy().into_owned(),
                        std::fs::read_to_string(build).expect("read BUILD"),
                    ));
                }
            }
            out.sort();
            out
        };

        assert_eq!(read_all(a.path()), read_all(b.path()));
    }

    #[test]
    fn generate_writes_every_target_and_only_forward_deps() {
        let params = CorpusParams {
            seed: 7,
            target_count: 200,
            packages: 20,
            layers: 8,
            fan_out: 3,
            ..Default::default()
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = generate(&params, dir.path()).expect("generate");
        assert_eq!(manifest.bash_addrs.len(), 200);

        // Every referenced dep addr must name a target generated in an
        // earlier layer (i < its own index is not guaranteed by addr alone,
        // but every addr referenced must exist in the manifest — proves no
        // dangling deps).
        let known: std::collections::HashSet<_> = manifest.bash_addrs.iter().collect();
        for pkg in &manifest.bash_packages {
            let src =
                std::fs::read_to_string(dir.path().join(pkg).join("BUILD")).expect("read BUILD");
            for line in src.lines().filter(|l| l.trim_start().starts_with("deps")) {
                for addr in line.split('"').skip(1).step_by(2) {
                    assert!(known.contains(&addr.to_string()), "dangling dep {addr}");
                }
            }
        }
    }

    // Ignored like `cache_load`/FUSE e2e: `go mod tidy` over gorepogen's
    // output needs network (or a warm module cache) for its k8s.io/etc
    // third-party imports. Not run by `tst`; run manually with `--ignored`
    // when touching the go-subtree path.
    #[test]
    #[ignore = "needs network for `go mod tidy` on gorepogen's third-party imports"]
    fn go_tree_produces_requested_package_count() {
        let params = CorpusParams {
            seed: 3,
            target_count: 30,
            packages: 3,
            layers: 3,
            fan_out: 2,
            go_packages: 5,
            ..Default::default()
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = generate(&params, dir.path()).expect("generate");
        assert_eq!(manifest.go_package_count, 5);
        assert!(dir.path().join("go/go.mod").is_file());
    }

    /// The deep-graph shape the memoizer redesign's bench gate runs against:
    /// one target per layer, exactly one dep each — a 200-deep linear chain.
    /// Depth is the point (the stack-overflow and depth-serialization
    /// pathologies scale with it), so the chain being unbroken is asserted,
    /// not assumed: with `fan_out = 1` every non-root layer picks exactly one
    /// dep from the layer below.
    #[test]
    fn generate_supports_a_deep_thin_chain_corpus() {
        let params = CorpusParams {
            seed: 5,
            target_count: 200,
            packages: 4,
            layers: 200,
            fan_out: 1,
            ..Default::default()
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = generate(&params, dir.path()).expect("generate deep corpus");
        assert_eq!(manifest.bash_addrs.len(), 200);

        // 200 targets over 200 layers = 1 per layer; every target except the
        // single layer-0 root must carry a deps list.
        let mut with_deps = 0usize;
        for pkg in &manifest.bash_packages {
            let src =
                std::fs::read_to_string(dir.path().join(pkg).join("BUILD")).expect("read BUILD");
            with_deps += src
                .lines()
                .filter(|l| l.trim_start().starts_with("deps"))
                .count();
        }
        assert_eq!(
            with_deps, 199,
            "a fan_out=1, one-target-per-layer corpus must be an unbroken chain"
        );
    }

    #[test]
    fn incrementalize_touches_requested_fraction() {
        let params = CorpusParams {
            seed: 1,
            target_count: 100,
            packages: 10,
            layers: 5,
            fan_out: 2,
            ..Default::default()
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = generate(&params, dir.path()).expect("generate");
        let touched = incrementalize(&manifest, dir.path(), 0.2, 99).expect("incrementalize");
        assert_eq!(touched, 2); // ceil(10 * 0.2)
    }

    #[test]
    fn incrementalize_go_touches_requested_fraction_and_skips_tests() {
        let dir = tempfile::tempdir().expect("tempdir");
        let go_root = dir.path().join("go");
        std::fs::create_dir_all(go_root.join("pkg")).expect("create dir");
        for n in 0..10 {
            std::fs::write(
                go_root.join("pkg").join(format!("f{n}.go")),
                "package pkg\n",
            )
            .expect("write go file");
        }
        std::fs::write(go_root.join("pkg").join("f0_test.go"), "package pkg\n")
            .expect("write test file");

        let touched = incrementalize_go(&go_root, 0.3, 7).expect("incrementalize_go");
        assert_eq!(touched, 3); // ceil(10 * 0.3), test file excluded from the pool

        let test_src = std::fs::read_to_string(go_root.join("pkg").join("f0_test.go"))
            .expect("read test file");
        assert_eq!(test_src, "package pkg\n", "test file must not be mutated");
    }

    /// Recursively collect `(path relative to `root`, contents)` under
    /// `root`, sorted — used by the js-tree tests below the same way
    /// `generate_is_deterministic`'s local `read_all` is used for the bash
    /// tree, just recursive (js packages nest per `js_max_depth`).
    fn read_all_recursive(root: &Path) -> Vec<(String, String)> {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) {
            let entries = std::fs::read_dir(dir).expect("read_dir");
            for entry in entries {
                let entry = entry.expect("dir entry");
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else {
                    let rel = path.strip_prefix(root).unwrap_or(&path);
                    out.push((
                        rel.to_string_lossy().into_owned(),
                        std::fs::read_to_string(&path).expect("read file"),
                    ));
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }

    /// Recursively collect directories under `root` that contain their own
    /// `package.json`, as paths relative to `root` — a minimal stand-in for
    /// `collect_js_packages` (kept local to this test rather than depending
    /// on the `plugin-js` crate, per this crate's existing "no plugin-crate
    /// dependency" shape — see `generate_go_tree`'s doc comment).
    fn discover_package_dirs(dir: &Path, root: &Path, out: &mut Vec<PathBuf>) {
        if dir.join("package.json").is_file() {
            out.push(dir.strip_prefix(root).unwrap_or(dir).to_path_buf());
        }
        let entries = std::fs::read_dir(dir).expect("read_dir");
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                discover_package_dirs(&path, root, out);
            }
        }
    }

    #[test]
    fn js_only_corpus_matches_manifest_and_workspace_shape() {
        let params = CorpusParams {
            seed: 5,
            target_count: 20,
            packages: 4,
            layers: 3,
            fan_out: 2,
            js_packages: 12,
            js_max_depth: 2,
            ..Default::default()
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = generate(&params, dir.path()).expect("generate");

        assert_eq!(manifest.js_package_count, 12);
        assert_eq!(manifest.js_prefix, "js");
        // go stays untouched: js_packages > 0 alone must not synthesize a go
        // subtree or count.
        assert_eq!(manifest.go_package_count, 0);
        assert!(!dir.path().join("go").exists());

        let js_root = dir.path().join("js");

        // Shape assertion: parse the generated `pnpm-workspace.yaml` with the
        // exact struct shape `workspace.rs::read_pnpm_workspace_globs` reads
        // (a `packages: Vec<String>` list, see its test fixtures), then match
        // every discovered package dir against those globs with the same
        // `wax` crate/version the real provider matches with.
        #[derive(serde::Deserialize)]
        struct PnpmWorkspaceFile {
            #[serde(default)]
            packages: Vec<String>,
        }
        let raw = std::fs::read_to_string(js_root.join("pnpm-workspace.yaml"))
            .expect("read pnpm-workspace.yaml");
        let parsed: PnpmWorkspaceFile =
            serde_yaml::from_str(&raw).expect("parse pnpm-workspace.yaml");
        assert!(!parsed.packages.is_empty());

        use wax::Program as _;
        let globs: Vec<wax::Glob> = parsed
            .packages
            .iter()
            .map(|p| wax::Glob::new(p).expect("valid glob"))
            .collect();

        let mut pkg_dirs = Vec::new();
        discover_package_dirs(&js_root, &js_root, &mut pkg_dirs);
        assert_eq!(
            pkg_dirs.len(),
            12,
            "every generated package must be found on disk"
        );

        // js_max_depth == 2 must actually produce nesting beyond the flat
        // `packages/pkgN` layer — otherwise this test would pass trivially
        // with just one glob line.
        assert!(
            pkg_dirs.iter().any(|d| d.components().count() > 2),
            "expected at least one package nested below packages/<name>"
        );

        for rel in &pkg_dirs {
            assert!(
                globs.iter().any(|g| g.is_match(rel.as_path())),
                "{} matched by no pnpm-workspace.yaml glob",
                rel.display()
            );

            let pj_raw = std::fs::read_to_string(js_root.join(rel).join("package.json"))
                .expect("read package.json");
            let pj: serde_json::Value = serde_json::from_str(&pj_raw).expect("parse package.json");
            assert!(
                pj.get("name").and_then(serde_json::Value::as_str).is_some(),
                "{}: package.json missing name",
                rel.display()
            );
            assert!(
                pj.get("main").and_then(serde_json::Value::as_str).is_some(),
                "{}: package.json missing main",
                rel.display()
            );
            // Hermeticity: no entry may name a real npm package.
            assert!(pj.get("dependencies").is_none());
            assert!(pj.get("devDependencies").is_none());
        }
    }

    #[test]
    fn bash_and_go_output_unaffected_by_js_packages() {
        let base = CorpusParams {
            seed: 9,
            target_count: 30,
            packages: 6,
            layers: 3,
            fan_out: 2,
            ..Default::default()
        };
        let without_js = base.clone();
        let mut with_js = base;
        with_js.js_packages = 8;
        with_js.js_max_depth = 3;

        let a = tempfile::tempdir().expect("tempdir");
        let b = tempfile::tempdir().expect("tempdir");
        let manifest_a = generate(&without_js, a.path()).expect("generate a");
        let manifest_b = generate(&with_js, b.path()).expect("generate b");

        assert_eq!(manifest_a.bash_addrs, manifest_b.bash_addrs);
        assert_eq!(manifest_a.bash_packages, manifest_b.bash_packages);
        for pkg in &manifest_a.bash_packages {
            let src_a = std::fs::read_to_string(a.path().join(pkg).join("BUILD")).expect("read a");
            let src_b = std::fs::read_to_string(b.path().join(pkg).join("BUILD")).expect("read b");
            assert_eq!(
                src_a, src_b,
                "bash BUILD output for {pkg} must be unaffected by js_packages"
            );
        }

        assert_eq!(manifest_a.js_package_count, 0);
        assert_eq!(manifest_b.js_package_count, 8);
    }

    #[test]
    fn js_tree_generation_is_deterministic() {
        let params = CorpusParams {
            seed: 77,
            target_count: 10,
            packages: 2,
            layers: 2,
            fan_out: 1,
            js_packages: 15,
            js_max_depth: 3,
            ..Default::default()
        };
        let a = tempfile::tempdir().expect("tempdir");
        let b = tempfile::tempdir().expect("tempdir");
        generate(&params, a.path()).expect("generate a");
        generate(&params, b.path()).expect("generate b");

        let js_a = read_all_recursive(&a.path().join("js"));
        let js_b = read_all_recursive(&b.path().join("js"));
        assert!(!js_a.is_empty());
        assert_eq!(js_a, js_b);
    }

    #[test]
    fn incrementalize_js_touches_requested_fraction() {
        let params = CorpusParams {
            seed: 3,
            target_count: 10,
            packages: 2,
            layers: 2,
            fan_out: 1,
            js_packages: 10,
            js_max_depth: 2,
            ..Default::default()
        };
        let dir = tempfile::tempdir().expect("tempdir");
        generate(&params, dir.path()).expect("generate");
        let js_root = dir.path().join("js");

        let touched = incrementalize_js(&js_root, 0.3, 55).expect("incrementalize_js");
        assert_eq!(touched, 3); // ceil(10 * 0.3), one index.ts per package

        let mutated = read_all_recursive(&js_root)
            .into_iter()
            .filter(|(path, contents)| {
                path.ends_with(".ts") && contents.contains("bench-mutated seed=55")
            })
            .count();
        assert_eq!(mutated, 3);
    }

    // Ignored like `go_tree_produces_requested_package_count`: `go mod tidy`
    // over gorepogen's output needs network. Extends that scenario to prove
    // go and js subtrees coexist independently in one corpus.
    #[test]
    #[ignore = "needs network for `go mod tidy` on gorepogen's third-party imports"]
    fn go_and_js_together_produce_both_independently() {
        let params = CorpusParams {
            seed: 3,
            target_count: 30,
            packages: 3,
            layers: 3,
            fan_out: 2,
            go_packages: 5,
            js_packages: 6,
            js_max_depth: 2,
            ..Default::default()
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = generate(&params, dir.path()).expect("generate");
        assert_eq!(manifest.go_package_count, 5);
        assert_eq!(manifest.js_package_count, 6);
        assert!(dir.path().join("go/go.mod").is_file());
        assert!(dir.path().join("js/pnpm-workspace.yaml").is_file());
    }
}
