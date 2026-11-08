//! Tier B: the real, prebuilt `heph` binary spawned as a child process,
//! dlopening the real `go` plugin cdylib — the seam `crates/bin-e2e` exists
//! to cover and an in-process test structurally cannot reach.
//!
//! Only prebuilt artifacts are used, located the same way
//! `crates/bin-e2e/tests/common/mod.rs`'s `Dist` does: a normalized
//! directory (`heph`, `heph-<name>-plugin.<ext>`), never rebuilt here.
//!
//! Unlike Tier A, the thing under test here (the real `heph` binary) is
//! already a prebuilt artifact on both sides, and the code driving it
//! (this module) is always the current checkout's own — there is no
//! per-commit "subject" binary to keep a stable contract with. `prepare`/
//! `measure_once` still split the same way as `inprocess`'s for a uniform
//! orchestrator shape, not because a compatibility seam requires it here.

use crate::inprocess::{cache_dir, wipe_cache};
use anyhow::{Context, Result, bail};
use bench_corpus::CorpusManifest;
use clap::ValueEnum;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

const DYLIB_EXT: &str = if cfg!(target_os = "macos") {
    "dylib"
} else {
    "so"
};

pub struct Dist {
    root: PathBuf,
}

impl Dist {
    pub fn locate(dir: &Path) -> Result<Self> {
        let heph = dir.join("heph");
        if !heph.is_file() {
            bail!("{} does not contain a `heph` binary", dir.display());
        }
        // Absolute: `build_go_tree` spawns `heph` with `current_dir(corpus)`,
        // so a relative `--dist` path (the common case — CI passes plain
        // `candidate-dist`) would silently resolve against the corpus dir
        // instead of the caller's cwd once that happens.
        let root = dir
            .canonicalize()
            .with_context(|| format!("canonicalize {}", dir.display()))?;
        Ok(Self { root })
    }

    fn heph(&self) -> PathBuf {
        self.root.join("heph")
    }

    fn plugin(&self, name: &str) -> PathBuf {
        self.root.join(format!("heph-{name}-plugin.{DYLIB_EXT}"))
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest as _, Sha256};
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(&bytes))))
}

fn host_os() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    }
}

fn host_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "amd64"
    }
}

/// Write the go-plugin manifest + a `.hephconfig` pointing at it, into
/// `corpus` — a real config a real `heph` invocation loads, forcing the
/// dlopen + ABI-negotiation + checksum-verify path. `corpus` must already be
/// absolute (see `Dist::locate`'s comment for why).
fn write_go_config(corpus: &Path, dist: &Dist) -> Result<()> {
    let dylib = dist.plugin("go");
    if !dylib.is_file() {
        bail!(
            "missing {} — the dist dir must contain the go plugin cdylib \
             (heph-go-plugin.{DYLIB_EXT}), not just the `heph` binary",
            dylib.display()
        );
    }
    let manifest_path = corpus.join("heph-go-plugin.json");
    let sum = sha256_file(&dylib)?;
    let doc = serde_json::json!({
        "name": "go",
        "version": "bench",
        "artifacts": [{
            "os": host_os(),
            "arch": host_arch(),
            "path": dylib,
            "checksum": sum,
        }],
    });
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&doc)?)
        .with_context(|| format!("write {}", manifest_path.display()))?;

    std::fs::write(corpus.join(".hephconfig"), go_config_yaml(&manifest_path))
        .context("write .hephconfig")
}

/// The `.hephconfig` Tier B runs under. Every `builtin:` here must be one the
/// shipped `heph` registers (`bootstrap::register_builtin_factories`) — an
/// unknown one fails `prepare` before a single target runs; the test below
/// holds that.
///
/// `gotool: host` — the go provider requires an explicit choice (host /
/// pinned version / a toolchain-producing target) and has no default.
/// `host` uses the Go `actions/setup-go` already installed for
/// `tools/gorepogen`, so this stays offline and pays no extra hermetic-
/// SDK download — same choice `bin-e2e`'s own go-plugin fixture makes,
/// and for the same reason.
///
/// `bash` is not optional: the go provider's generated targets (stdlib
/// `build_lib` included) are bash-driven.
fn go_config_yaml(manifest_path: &Path) -> String {
    format!(
        "plugins:\n  \
         - builtin: buildfile\n    options:\n      patterns:\n        - BUILD\n  \
         - builtin: exec\n  \
         - builtin: bash\n  \
         - path: {}\n    options:\n      gotool: \"host\"\n",
        manifest_path.display()
    )
}

/// The `.hephconfig` the codegen scenarios run under: the builtins the
/// corpus's bash and `copy` targets need, and no plugin cdylib, so either
/// side runs it whatever plugins its dist ships.
const CODEGEN_CONFIG: &str = "plugins:\n  \
     - builtin: buildfile\n    options:\n      patterns:\n        - BUILD\n  \
     - builtin: exec\n  \
     - builtin: bash\n";

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Scenario {
    Cold,
    FullHit,
    Incremental,
    /// Codegen corpus (`--codegen-files`): the root `glob("**")`
    /// (`//@heph/fs:glob@p=**`) with the walk cache (`fswalk.db`) removed
    /// before each rep. `prepare` has built every `copy` target, so the
    /// generated files are on disk and recorded (N-1: stamped).
    ///
    /// Bar (spec): cold <= 1.25 x the probe-free baseline re-measured on the
    /// codegen corpus, on the medians. That baseline is the candidate itself
    /// with the index inert, interleaved with its normal reps (`run dist
    /// --out-probe-free`, [`PROBE_FREE_GLOB`]): no "N-1 with the probe
    /// stubbed" build exists to fetch. The comparison with N-1 as released
    /// (with the xattr probe, while N-1 predates the index) is a reported
    /// row, not a gate.
    CodegenGlobCold,
    /// As `codegen-glob-cold`, keeping the walk cache across reps.
    CodegenGlobWarm,
    /// Codegen corpus: rebuild every `copy` target after `prepare`'s full
    /// build (all cache hits; each write-back finds nothing to do). Bar: <=
    /// 1.02 x N-1's warm build plus N-1's own spread. Its exact bars (0
    /// commits, 0 lock files) are `check-codegen`'s, on the candidate alone.
    CodegenWarmBuild,
    /// Codegen corpus: `heph tool codegen import` with the index db removed
    /// before each rep (the files keep the xattr every write-back stamps).
    /// Candidate only (`run dist --candidate-only`): N-1 has no import. Bar:
    /// <= 1.1 x N-1's `codegen-glob-cold`, i.e. its glob with the probe,
    /// through `compare`'s `against`. That baseline only has the probe while
    /// N-1 predates the index; against a later N-1 this compares the import
    /// with a probe-free glob, which the spec does not ask for.
    CodegenImport,
}

impl Scenario {
    pub fn name(self) -> &'static str {
        match self {
            Scenario::Cold => "cold",
            Scenario::FullHit => "full-hit",
            Scenario::Incremental => "incremental",
            Scenario::CodegenGlobCold => "codegen-glob-cold",
            Scenario::CodegenGlobWarm => "codegen-glob-warm",
            Scenario::CodegenWarmBuild => "codegen-warm-build",
            Scenario::CodegenImport => "codegen-import",
        }
    }

    /// Whether a candidate rep times index lookups, and so must start from a
    /// populated index ([`check_populated_index`]).
    pub fn times_index_lookups(self) -> bool {
        matches!(self, Scenario::CodegenGlobCold | Scenario::CodegenGlobWarm)
    }

    fn is_codegen(self) -> bool {
        match self {
            Scenario::Cold | Scenario::FullHit | Scenario::Incremental => false,
            Scenario::CodegenGlobCold
            | Scenario::CodegenGlobWarm
            | Scenario::CodegenWarmBuild
            | Scenario::CodegenImport => true,
        }
    }
}

/// The label carried by the go plugin's compile targets (`build_lib`,
/// `build_test`, `build_xtest`). NOT `build` — that is a *target name* in the go
/// provider, not a label, and the bare `//pkg:build` magic group only resolves
/// for a `package main`, which this corpus has none of.
const GO_BUILD_LABEL: &str = "go-build";

/// Number of targets `heph` reported matching, parsed from its `matched N
/// targets` line. `None` if no such line was emitted.
fn matched_targets(stderr: &str) -> Option<u64> {
    count_after(stderr, "matched ")
}

/// The number right after the last `word` in `output`.
fn count_after(output: &str, word: &str) -> Option<u64> {
    let rest = output.rsplit_once(word)?.1;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Run `heph <args>` in `corpus` with a throwaway `HOME`: the elapsed
/// milliseconds and the combined stdout + stderr. A non-zero exit is an error
/// carrying that output.
fn run_heph(dist: &Dist, corpus: &Path, args: &[&str]) -> Result<(f64, String)> {
    let home = tempfile::tempdir().context("create HOME tempdir")?;
    let what = args.join(" ");
    let start = Instant::now();
    let out = Command::new(dist.heph())
        .args(args)
        .current_dir(corpus)
        .env("HOME", home.path())
        .env("HEPH_CWD", corpus)
        .env("HEPH_NO_SELF_UPDATE", "1")
        .env("HEPH_DISABLE_TELEMETRY", "1")
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("spawn heph {what}"))?;
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    let output =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        bail!(
            "heph {what} failed: status {}\n--- output ---\n{output}",
            out.status,
        );
    }
    Ok((ms, output))
}

fn build_go_tree(dist: &Dist, corpus: &Path) -> Result<f64> {
    let (ms, stderr) = run_heph(dist, corpus, &["r", GO_BUILD_LABEL, "//go/..."])?;
    // A selection that matches nothing still exits 0, so without this a broken
    // corpus or a renamed label reads as a passing — and very fast — benchmark.
    // That is not hypothetical: this scenario spent its whole life matching zero
    // targets and timing only package discovery.
    match matched_targets(&stderr) {
        Some(0) | None => bail!(
            "heph r {GO_BUILD_LABEL} //go/... matched no targets — the corpus built nothing, so \
             this measures package discovery, not a build. Check that the corpus declares a go \
             variant and that `{GO_BUILD_LABEL}` is still the compile targets' label.\n\
             --- output ---\n{stderr}",
        ),
        Some(_) => {}
    }
    Ok(ms)
}

/// The root glob every codegen glob scenario resolves.
const ROOT_GLOB: &str = "//@heph/fs:glob@p=**";

/// Build every `copy` target of the codegen corpus (by its label), checking
/// the selection matched exactly the manifest's count.
fn build_codegen(dist: &Dist, corpus: &Path, expected: usize) -> Result<f64> {
    let (ms, output) = run_heph(dist, corpus, &["r", bench_corpus::CODEGEN_LABEL, "//..."])?;
    let matched = matched_targets(&output);
    if matched != u64::try_from(expected).ok() {
        bail!(
            "heph r {} //... matched {matched:?} targets, the corpus declares {expected} — \
             this would time something other than the copy targets\n--- output ---\n{output}",
            bench_corpus::CODEGEN_LABEL,
        );
    }
    Ok(ms)
}

fn glob_root(dist: &Dist, corpus: &Path) -> Result<f64> {
    let (ms, output) = run_heph(dist, corpus, &["r", ROOT_GLOB])?;
    if matched_targets(&output) != Some(1) {
        bail!("heph r {ROOT_GLOB} did not match the glob target\n--- output ---\n{output}");
    }
    Ok(ms)
}

/// Remove every file `prefix` names in `dir` (`fswalk.db`, its `-wal`,
/// `-shm` and lock siblings). A missing `dir` is nothing to remove.
fn remove_prefixed(dir: &Path, prefix: &str) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("list {}", dir.display())),
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("list {}", dir.display()))?;
        if entry.file_name().to_string_lossy().starts_with(prefix) {
            std::fs::remove_file(entry.path())
                .with_context(|| format!("remove {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// `heph tool codegen import` into a removed index; the count it recorded
/// must be every corpus output, or the rep timed a scan that found nothing
/// (a filesystem without xattrs, or a prepare that never built).
fn import_codegen(dist: &Dist, corpus: &Path, expected: usize) -> Result<f64> {
    let home = cache_dir(corpus)?;
    let codegen = heph::htwalk::codegen_index::codegen_dir(&home);
    if codegen.exists() {
        std::fs::remove_dir_all(&codegen)
            .with_context(|| format!("remove {}", codegen.display()))?;
    }
    let (ms, output) = run_heph(dist, corpus, &["tool", "codegen", "import"])?;
    let recorded = count_after(&output, "recorded ");
    if recorded != u64::try_from(expected).ok() {
        bail!(
            "heph tool codegen import recorded {recorded:?} files, the corpus has {expected} \
             stamped outputs\n--- output ---\n{output}"
        );
    }
    Ok(ms)
}

/// The scenario name [`measure_probe_free_glob`]'s reps are written under.
pub const PROBE_FREE_GLOB: &str = "codegen-glob-cold-probe-free";

/// The probe-free baseline of `codegen-glob-cold` (spec, Bench: "cold <=
/// 1.25 x the probe-free baseline re-measured on the codegen corpus"): the
/// same cold root glob by the same binary on the same corpus, with the index
/// inert — no `<home>/codegen` db and no managed section in `.gitignore` —
/// so nothing the glob does depends on codegen at all. `run dist
/// --out-probe-free` interleaves it with the candidate's normal reps.
pub fn measure_probe_free_glob(dist_dir: &Path, corpus: &Path) -> Result<f64> {
    // Same config and same cold start as `codegen-glob-cold`'s reps.
    let dist = Dist::locate(dist_dir)?;
    write_dist_codegen_config(&dist, corpus)?;
    let home = cache_dir(corpus)?;
    remove_prefixed(&home.join("cache"), "fswalk.db")?;
    let inert = InertIndex::hide(corpus, &home)?;
    let ms = glob_root(&dist, corpus);
    inert.restore()?;
    ms
}

/// The index's two inputs moved out of the way, restored by [`restore`].
///
/// [`restore`]: InertIndex::restore
struct InertIndex {
    gitignore: PathBuf,
    original: Option<Vec<u8>>,
    codegen: PathBuf,
    aside: PathBuf,
}

impl InertIndex {
    fn hide(corpus: &Path, home: &Path) -> Result<Self> {
        let codegen = heph::htwalk::codegen_index::codegen_dir(home);
        let aside = codegen.with_extension("probe-free-aside");
        if aside.exists() {
            bail!(
                "{} exists: an earlier probe-free rep was not restored",
                aside.display()
            );
        }
        if codegen.exists() {
            std::fs::rename(&codegen, &aside)
                .with_context(|| format!("move {} aside", codegen.display()))?;
        }
        let gitignore = corpus.join(".gitignore");
        let original = match std::fs::read(&gitignore) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("read {}", gitignore.display())),
        };
        if let Some(bytes) = &original {
            std::fs::write(&gitignore, strip_section(&String::from_utf8_lossy(bytes)))
                .with_context(|| format!("write {}", gitignore.display()))?;
        }
        Ok(InertIndex {
            gitignore,
            original,
            codegen,
            aside,
        })
    }

    /// Put back the `.gitignore` and the db as they were; a db the inert
    /// run created is dropped.
    fn restore(self) -> Result<()> {
        if let Some(bytes) = &self.original {
            std::fs::write(&self.gitignore, bytes)
                .with_context(|| format!("restore {}", self.gitignore.display()))?;
        }
        if self.codegen.exists() {
            std::fs::remove_dir_all(&self.codegen)
                .with_context(|| format!("remove {}", self.codegen.display()))?;
        }
        if self.aside.exists() {
            std::fs::rename(&self.aside, &self.codegen)
                .with_context(|| format!("restore {}", self.codegen.display()))?;
        }
        Ok(())
    }
}

/// `text` without its heph-managed section (BEGIN line through END line).
fn strip_section(text: &str) -> String {
    use heph::htwalk::gitignore_section::{BEGIN_MARKER_PREFIX, END_MARKER};
    let mut out = String::with_capacity(text.len());
    let mut inside = false;
    for line in text.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\n', '\r']);
        if !inside && bare.starts_with(BEGIN_MARKER_PREFIX) {
            inside = true;
        } else if inside {
            inside = bare != END_MARKER;
        } else {
            out.push_str(line);
        }
    }
    out
}

/// The codegen config with the default home (`.heph`): `check-codegen`'s,
/// which runs one engine and has no second binary to keep apart from.
pub(crate) fn write_codegen_config(corpus: &Path) -> Result<()> {
    std::fs::write(corpus.join(".hephconfig"), CODEGEN_CONFIG).context("write .hephconfig")
}

/// The heph home a codegen scenario gives `dist`'s binary: one per binary,
/// so the baseline's prepare (which wipes its home) cannot delete the
/// candidate's `<home>/codegen` db. In one shared home it did: the warmup
/// prepares the candidate, then the baseline, and N-1 predates the index,
/// so every candidate glob rep ran against the section with no rows. The
/// name is under `.heph*`, which every tree walk prunes, both binaries'
/// included.
fn codegen_home(dist: &Dist) -> String {
    use sha2::{Digest as _, Sha256};
    use std::os::unix::ffi::OsStrExt as _;
    let digest = Sha256::digest(dist.root.as_os_str().as_bytes());
    let id: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    format!(".heph-bench-{id}")
}

/// The codegen config for `dist`'s binary, with its own home
/// ([`codegen_home`]). Written before each of its prepares and reps, so
/// `cache_dir` resolves the home of the binary about to run.
fn write_dist_codegen_config(dist: &Dist, corpus: &Path) -> Result<()> {
    let config = format!("{CODEGEN_CONFIG}homeDir: {}\n", codegen_home(dist));
    std::fs::write(corpus.join(".hephconfig"), config).context("write .hephconfig")
}

/// Fail unless `dist`'s codegen index db exists and holds a settled row
/// (recorded, not pending) for every corpus output: the state every
/// candidate `codegen-glob-cold`/`-warm` rep must time its lookups against.
/// Run before each such rep, outside the timed region; a probe-free rep
/// that failed to put the db back fails the next one here.
pub fn check_populated_index(
    dist_dir: &Path,
    corpus: &Path,
    manifest: &CorpusManifest,
) -> Result<()> {
    let dist = Dist::locate(dist_dir)?;
    write_dist_codegen_config(&dist, corpus)?;
    let home = cache_dir(corpus)?;
    check_index_rows(corpus, &home, manifest.codegen_outputs.len())
}

fn check_index_rows(corpus: &Path, home: &Path, expected: usize) -> Result<()> {
    use heph::htwalk::codegen_index::{Store, StoreOptions, db_path};
    let db = db_path(corpus, home);
    if !db.is_file() {
        bail!(
            "no codegen index db at {}: the rep would time lookups against an empty index",
            db.display()
        );
    }
    let (_, rows) = Store::open(&db, &StoreOptions::default())
        .and_then(|store| store.load_all())
        .with_context(|| format!("read the codegen index {}", db.display()))?;
    let settled = rows
        .iter()
        .filter(|r| r.record.is_some() && r.pending_hash.is_none())
        .count();
    if rows.len() != expected || settled != expected {
        bail!(
            "codegen index {} has {} rows ({settled} settled), the corpus has {expected} \
             outputs: the rep would not time lookups against a populated index",
            db.display(),
            rows.len(),
        );
    }
    Ok(())
}

/// Codegen scenarios' prepare: a wiped home and one full build of every
/// `copy` target, which writes the files back and records (N-1: stamps) them.
fn prepare_codegen(dist: &Dist, corpus: &Path) -> Result<()> {
    write_dist_codegen_config(dist, corpus)?;
    let manifest = bench_corpus::load_manifest(corpus).context("load corpus manifest")?;
    if manifest.codegen_addrs.is_empty() {
        bail!("corpus has no copy targets (generate with --codegen-files > 0)");
    }
    wipe_cache(corpus)?;
    build_codegen(dist, corpus, manifest.codegen_addrs.len())?;
    Ok(())
}

fn measure_codegen(
    dist: &Dist,
    corpus: &Path,
    manifest: &CorpusManifest,
    scenario: Scenario,
) -> Result<f64> {
    if manifest.codegen_addrs.is_empty() {
        bail!("corpus has no copy targets (generate with --codegen-files > 0)");
    }
    write_dist_codegen_config(dist, corpus)?;
    match scenario {
        Scenario::CodegenGlobCold => {
            remove_prefixed(&cache_dir(corpus)?.join("cache"), "fswalk.db")?;
            glob_root(dist, corpus)
        }
        Scenario::CodegenGlobWarm => glob_root(dist, corpus),
        Scenario::CodegenWarmBuild => build_codegen(dist, corpus, manifest.codegen_addrs.len()),
        Scenario::CodegenImport => import_codegen(dist, corpus, manifest.codegen_outputs.len()),
        Scenario::Cold | Scenario::FullHit | Scenario::Incremental => {
            bail!("{} is not a codegen scenario", scenario.name())
        }
    }
}

/// `corpus` must already be absolute (canonicalize before calling — see
/// `write_go_config`'s comment on why a relative path breaks once `heph`
/// runs with `current_dir(corpus)`).
pub fn prepare(dist_dir: &Path, corpus: &Path, scenario: Scenario) -> Result<()> {
    let dist = Dist::locate(dist_dir)?;
    if scenario.is_codegen() {
        return prepare_codegen(&dist, corpus);
    }
    write_go_config(corpus, &dist)?;
    wipe_cache(corpus)?;
    build_go_tree(&dist, corpus)?;
    Ok(())
}

/// One measured rep. `mutate_seed` only matters for `Incremental` (ignored
/// otherwise) — the caller must vary it across repeated calls against the
/// same corpus, or every call mutates the same files again. `corpus` must
/// already be absolute, same as `prepare`.
pub fn measure_once(
    dist_dir: &Path,
    corpus: &Path,
    manifest: &CorpusManifest,
    scenario: Scenario,
    mutate_seed: u64,
) -> Result<f64> {
    if scenario.is_codegen() {
        return measure_codegen(&Dist::locate(dist_dir)?, corpus, manifest, scenario);
    }
    if manifest.go_package_count == 0 {
        bail!(
            "corpus has no go/ subtree (generate with --go-packages > 0) — nothing for Tier B to build"
        );
    }
    let dist = Dist::locate(dist_dir)?;
    write_go_config(corpus, &dist)?;

    if let Scenario::Cold = scenario {
        wipe_cache(corpus)?;
    }
    if let Scenario::Incremental = scenario {
        // Mutates the go/ subtree being built here, not the bash pkgN
        // packages `bench_corpus::incrementalize` touches — Tier B only
        // builds `//go/...`.
        bench_corpus::incrementalize_go(&corpus.join("go"), 0.01, 0xDEC1 ^ mutate_seed)
            .context("mutate go corpus for incremental scenario")?;
    }
    build_go_tree(&dist, corpus)
}

#[cfg(test)]
mod tests {
    use super::{InertIndex, matched_targets, strip_section};
    use heph::htwalk::gitignore_section::{BEGIN_MARKER, END_MARKER};

    /// The managed section goes, whatever surrounds it; a file without one,
    /// or with a BEGIN line of an older marker text, is handled the same.
    #[test]
    fn strip_section_drops_only_the_managed_section() {
        let text = format!("/target\n{BEGIN_MARKER}\n# //p:g\n/p/g.txt\n{END_MARKER}\n/out\n");
        assert_eq!(strip_section(&text), "/target\n/out\n");
        assert_eq!(strip_section("/target\n"), "/target\n");
        let old = format!("# BEGIN heph-generated (old text)\n/p/g.txt\n{END_MARKER}\n");
        assert_eq!(strip_section(&old), "");
        let crlf = format!("{BEGIN_MARKER}\r\n/p/g.txt\r\n{END_MARKER}\r\n/x\r\n");
        assert_eq!(strip_section(&crlf), "/x\r\n");
    }

    /// Hidden, the index has neither input; restored, both are back as they
    /// were, and a db the inert run created is gone.
    #[test]
    fn inert_index_hides_and_restores_both_inputs() {
        let read = |p: &std::path::Path| std::fs::read_to_string(p).expect("read");
        let corpus = tempfile::tempdir().expect("tempdir");
        let home = corpus.path().join(".heph");
        let codegen = heph::htwalk::codegen_index::codegen_dir(&home);
        let ignore = corpus.path().join(".gitignore");
        std::fs::create_dir_all(&codegen).expect("mkdir");
        std::fs::write(codegen.join("db"), "rows").expect("write db");
        let gitignore = format!("{BEGIN_MARKER}\n/p/g.txt\n{END_MARKER}\n");
        std::fs::write(&ignore, &gitignore).expect("write .gitignore");

        let inert = InertIndex::hide(corpus.path(), &home).expect("hide");
        assert!(!codegen.exists(), "the db is out of the way");
        assert_eq!(read(&ignore), "");
        std::fs::create_dir_all(&codegen).expect("mkdir");
        std::fs::write(codegen.join("db"), "made by the inert run").expect("write db");
        inert.restore().expect("restore");

        assert_eq!(read(&codegen.join("db")), "rows");
        assert_eq!(read(&ignore), gitignore);
    }

    /// Two binaries get two homes, each pruned from every walk by its
    /// `.heph` prefix, and the written config resolves `cache_dir` to the
    /// home of the binary it was written for: a baseline prepare's wipe
    /// cannot reach the candidate's db.
    #[test]
    fn each_binary_gets_its_own_home() {
        use super::{Dist, codegen_home, write_dist_codegen_config};
        let corpus = tempfile::tempdir().expect("tempdir");
        let candidate = Dist {
            root: "/dists/candidate".into(),
        };
        let baseline = Dist {
            root: "/dists/baseline".into(),
        };
        assert_ne!(codegen_home(&candidate), codegen_home(&baseline));
        for dist in [&candidate, &baseline] {
            let name = codegen_home(dist);
            assert!(name.starts_with(".heph"), "{name} is not pruned");
            write_dist_codegen_config(dist, corpus.path()).expect("config");
            let home = crate::inprocess::cache_dir(corpus.path()).expect("home");
            assert_eq!(&*home, corpus.path().join(&name));
        }
    }

    /// The check passes only on a db holding a settled row per output: no
    /// db, too few rows, and a row still pending each fail it.
    #[test]
    fn index_check_wants_a_settled_row_per_output() {
        use super::check_index_rows;
        use heph::htwalk::codegen_index::{
            ImportLockGuard, Record, Row, RowKind, db_path, store::create_with_rows,
        };
        let row = |path: &str, settled: bool| {
            let mut row = Row::new(path.as_bytes(), "//p:gen", RowKind::File);
            if settled {
                row.record = Some(Record {
                    hash: "h".into(),
                    size: 1,
                    mtime_ns: 1,
                    ino: 1,
                    dev: 1,
                    mode: 0o100644,
                    written_at_ns: 2,
                });
            } else {
                row.pending_hash = Some("h".into());
            }
            row
        };
        let corpus = tempfile::tempdir().expect("tempdir");
        let home = corpus.path().join(".heph-bench-x");
        let err = check_index_rows(corpus.path(), &home, 2).expect_err("no db");
        assert!(
            format!("{err:#}").contains("no codegen index db"),
            "{err:#}"
        );

        let db = db_path(corpus.path(), &home);
        let rows = [row("p/a.txt", true), row("p/b.txt", false)];
        create_with_rows(&db, &ImportLockGuard::new(Box::new(())), &rows).expect("create");
        let err = check_index_rows(corpus.path(), &home, 2).expect_err("one row pending");
        assert!(format!("{err:#}").contains("2 rows (1 settled)"), "{err:#}");
        let err = check_index_rows(corpus.path(), &home, 3).expect_err("a row short");
        assert!(format!("{err:#}").contains("3 outputs"), "{err:#}");

        let settled = tempfile::tempdir().expect("tempdir");
        let home = settled.path().join(".heph-bench-x");
        let db = db_path(settled.path(), &home);
        let rows = [row("p/a.txt", true), row("p/b.txt", true)];
        create_with_rows(&db, &ImportLockGuard::new(Box::new(())), &rows).expect("create");
        check_index_rows(settled.path(), &home, 2).expect("populated");
    }

    #[test]
    fn reads_the_match_count_heph_reports() {
        assert_eq!(
            matched_targets(" INFO matched 500 targets\n INFO matched 500 / 500, done 11713\n"),
            Some(500)
        );
    }

    #[test]
    fn a_zero_match_is_reported_as_zero_not_as_absent() {
        // The whole point of the guard: this run exits 0 and must still fail.
        assert_eq!(matched_targets(" INFO matched 0 targets\n"), Some(0));
    }

    #[test]
    fn absent_line_is_none() {
        assert_eq!(matched_targets("no such line here\n"), None);
    }

    /// Every `builtin:` in the generated config must be one `heph` registers.
    /// Removing the `sh` driver left `builtin: sh` here, and Tier B failed at
    /// `prepare` on every master push with `unknown builtin plugin 'sh'`.
    #[tokio::test]
    async fn generated_config_names_only_registered_builtins() {
        use heph::engine::config_yaml::{self, PluginIdentifier};
        use heph::engine::{Config, Engine};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".hephconfig");
        std::fs::write(
            &path,
            super::go_config_yaml(&dir.path().join("heph-go-plugin.json")),
        )
        .expect("write config");
        let file = config_yaml::load(&path).expect("parse generated config");

        let mut e = Engine::new(Config::new(
            dir.path().to_path_buf(),
            crate::inprocess::cache_dir(dir.path()).expect("home dir"),
        ))
        .expect("engine");
        heph::commands::bootstrap::register_builtin_factories(&mut e, dir.path())
            .expect("register");

        let mut builtins = 0;
        for spec in &file.plugins {
            if let PluginIdentifier::Builtin(name) = &spec.identifier {
                e.apply_builtin(name, &spec.options)
                    .unwrap_or_else(|err| panic!("builtin `{name}`: {err:#}"));
                builtins += 1;
            }
        }
        assert!(builtins > 0, "config lists no builtins: {:?}", file.plugins);
    }
}
