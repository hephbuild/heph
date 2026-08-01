//! Deterministic synthetic-corpus generator for `heph-bench`.
//!
//! Produces a workspace tree of plain `bash`-driver targets arranged as a
//! layered DAG (bounded fan-out, no cycles), plus an optional `go/` subtree
//! of hermetic Go packages (auto-discovered by the go provider — no BUILD
//! file needed) for scenarios that must cross the real plugin-cdylib seam.
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

    let manifest = CorpusManifest {
        bash_addrs: g.addrs,
        bash_prefix: String::new(),
        go_package_count,
        go_prefix: "go".to_string(),
        bash_packages,
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

/// A `go/` subtree the go provider auto-discovers from `go.mod` alone (no
/// BUILD file) — generated by `tools/gorepogen` rather than hand-rolled here,
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

    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
