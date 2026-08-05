//! Tier B: the real, prebuilt `heph` binary spawned as a child process,
//! dlopening the real per-language plugin cdylib(s) — the seam
//! `crates/bin-e2e` exists to cover and an in-process test structurally
//! cannot reach.
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
//!
//! **Languages**: [`GO`] and [`JS`] are the two [`Lang`]s this tier knows how
//! to build — one `//<name>/...` corpus subtree, one plugin cdylib, one
//! provider-level option a real workspace has no default for (mirrors the go
//! provider's required `gotool`; see `Lang`'s doc). A caller picks one or
//! both via the `langs` slice threaded through [`prepare`]/[`measure_once`];
//! `crates/bench/src/main.rs`'s `--lang` flag is what turns that into a CLI
//! selector. Each language selects its own targets its own way (see
//! `Lang::label`'s doc): go carries a `go-build` label the provider stamps
//! on every compile target; js has no such label mechanism, so it uses the
//! `-e '<query>'` form instead. Both go through the same `matched_targets`
//! safety net regardless of which form selected them.

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
        // Absolute: `build_lang_tree` spawns `heph` with `current_dir(corpus)`,
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

/// A language Tier B knows how to build: `name` doubles as the plugin
/// identity (`heph-<name>-plugin.<ext>`) *and* the corpus subtree / target
/// prefix (`//<name>/...`) — true for both `go/` and `js/`, the only two
/// today, and simpler than carrying two separate fields that would always
/// agree in practice.
///
/// Deliberately data, not a `match` on a `&str` language tag scattered
/// across this module — adding a third language means adding one more
/// `const`, not hunting down every place a go-vs-js branch was hand-written.
#[derive(Debug)]
pub struct Lang {
    pub name: &'static str,
    /// The provider's `options:` fragment this language's plugin requires
    /// with no default — mirrors the go provider's `gotool` option (see
    /// `write_dist_config`'s doc comment) and the js provider's `pkgmanager`
    /// option (`crates/plugin-js/src/pluginjs/provider.rs`: "required — set
    /// it to \"npm\" or \"pnpm\"", no genuine default to pick for a real
    /// workspace).
    provider_options: &'static str,
    /// The label this language's *compile* targets carry, if any — go's
    /// `go-build` label (`crates/plugin-go/src/plugingo/driver_compile.rs`
    /// et al.) lets target selection use the fast `heph r <label>
    /// //go/...` two-positional-arg form (`label(go-build) && //go/...`).
    /// `None` means the provider has no such label (the js provider stamps
    /// none), so `build_lang_tree` falls back to the `-e '<query>'` form
    /// instead — see that function's doc for why the *other* two-positional
    /// form (`heph r build //<lang>/...`) is never correct for either case.
    label: Option<&'static str>,
    /// Pulls this language's generated package count out of a
    /// [`CorpusManifest`] — `measure_once`'s "nothing for Tier B to build"
    /// bail needs this per language, same pattern as the original go-only
    /// `measure_once`'s `manifest.go_package_count == 0` check.
    package_count: fn(&CorpusManifest) -> usize,
    /// Mutates a fraction of this language's corpus subtree for the
    /// `Incremental` scenario — `bench_corpus::incrementalize_go` /
    /// `incrementalize_js`.
    incrementalize: fn(&Path, f64, u64) -> anyhow::Result<usize>,
}

pub const GO: Lang = Lang {
    name: "go",
    // `host` uses the Go `actions/setup-go` already installed for
    // `tools/gorepogen`, so this stays offline and pays no extra hermetic-
    // SDK download — same choice `bin-e2e`'s own go-plugin fixture makes,
    // and for the same reason.
    provider_options: "gotool: \"host\"",
    // NOT `build` — that is a *target name* in the go provider, not a
    // label, and the bare `//pkg:build` magic group only resolves for a
    // `package main`, which this corpus has none of. `go-build` is the real
    // label the go provider's compile targets (`build_lib`, `build_test`,
    // `build_xtest`, ...) carry.
    label: Some("go-build"),
    package_count: |m| m.go_package_count,
    incrementalize: bench_corpus::incrementalize_go,
};

pub const JS: Lang = Lang {
    name: "js",
    // pnpm is the package-manager convention `bench_corpus::generate_js_tree`
    // writes (`js/pnpm-workspace.yaml`) — must match, or the js provider
    // discovers no workspace members at all.
    provider_options: "pkgmanager: \"pnpm\"",
    // The js provider stamps no bench-specific label on its targets, so
    // there's nothing to select by — `build_lang_tree` uses the `-e` query
    // form for this language instead.
    label: None,
    package_count: |m| m.js_package_count,
    incrementalize: bench_corpus::incrementalize_js,
};

/// Write every one of `langs`' plugin manifest + a single `.hephconfig`
/// covering all of them, into `corpus` — a real config a real `heph`
/// invocation loads, forcing the dlopen + ABI-negotiation + checksum-verify
/// path for each. `corpus` must already be absolute (see `Dist::locate`'s
/// comment for why).
fn write_dist_config(corpus: &Path, dist: &Dist, langs: &[&Lang]) -> Result<()> {
    // `sh` is not optional: the go provider's stdlib `build_lib` targets are
    // sh-driven, so without it every first-party compile fails at
    // `//@heph/go/std/...: driver not found: sh`. It went unnoticed while
    // this scenario matched zero targets and therefore never resolved a std
    // dep. Registered unconditionally (js-only runs pay nothing for an
    // unused builtin) rather than only when `GO` is among `langs`, so this
    // preamble doesn't itself need a language branch.
    let mut config = String::from(
        "plugins:\n  \
         - builtin: buildfile\n    options:\n      patterns:\n        - BUILD\n  \
         - builtin: exec\n  \
         - builtin: bash\n  \
         - builtin: sh\n",
    );

    for lang in langs {
        let dylib = dist.plugin(lang.name);
        if !dylib.is_file() {
            bail!(
                "missing {} — the dist dir must contain the {n} plugin cdylib \
                 (heph-{n}-plugin.{DYLIB_EXT}), not just the `heph` binary",
                dylib.display(),
                n = lang.name,
            );
        }
        let manifest_path = corpus.join(format!("heph-{}-plugin.json", lang.name));
        let sum = sha256_file(&dylib)?;
        let doc = serde_json::json!({
            "name": lang.name,
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

        config.push_str(&format!(
            "  - path: {}\n    options:\n      {}\n",
            manifest_path.display(),
            lang.provider_options,
        ));
    }

    std::fs::write(corpus.join(".hephconfig"), config).context("write .hephconfig")
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Scenario {
    Cold,
    FullHit,
    Incremental,
}

impl Scenario {
    pub fn name(self) -> &'static str {
        match self {
            Scenario::Cold => "cold",
            Scenario::FullHit => "full-hit",
            Scenario::Incremental => "incremental",
        }
    }
}

fn cache_dir(corpus: &Path) -> PathBuf {
    corpus.join(".heph3")
}

fn wipe_cache(corpus: &Path) -> Result<()> {
    let dir = cache_dir(corpus);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
    }
    Ok(())
}

/// Number of targets `heph` reported matching, parsed from its `matched N
/// targets` line. `None` if no such line was emitted.
fn matched_targets(stderr: &str) -> Option<u64> {
    let rest = stderr.rsplit_once("matched ")?.1;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn build_lang_tree(dist: &Dist, corpus: &Path, lang: &Lang) -> Result<f64> {
    let home = tempfile::tempdir().context("create HOME tempdir")?;
    let target = format!("//{}/...", lang.name);
    let start = Instant::now();
    // NOT `["r", "build", &target]` for either selection form: with two
    // positional args, `heph r`'s arg1/arg2 form treats arg1 as a *label*,
    // ANDed with arg2's package matcher (`src/commands/utils.rs::
    // matcher_from_args`) — `label(build) && //<lang>/...` matches nothing
    // (no BUILD file/provider sets a `build` label) and exits 0 having built
    // zero targets. Two real forms select something: `lang`'s own label
    // (`label(go-build) && //go/...`) when one exists, or the `-e` query
    // form when it doesn't.
    let args: Vec<&str> = match lang.label {
        Some(label) => vec!["r", label, &target],
        None => vec!["r", "-e", &target],
    };
    let out = Command::new(dist.heph())
        .args(&args)
        .current_dir(corpus)
        .env("HOME", home.path())
        .env("HEPH_CWD", corpus)
        .env("HEPH_NO_SELF_UPDATE", "1")
        .env("HEPH_DISABLE_TELEMETRY", "1")
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("spawn heph {}", args.join(" ")))?;
    let stderr =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        bail!(
            "heph {} failed: status {}\n--- output ---\n{}",
            args.join(" "),
            out.status,
            stderr,
        );
    }
    // A selection that matches nothing still exits 0, so without this a
    // broken corpus, a renamed label, or a wrong query reads as a passing —
    // and very fast — benchmark. Not hypothetical: the go scenario spent
    // its whole life matching zero targets and timing only package
    // discovery before this check existed.
    match matched_targets(&stderr) {
        Some(0) | None => bail!(
            "heph {} matched no targets — the {} corpus subtree built nothing, so this \
             measures package discovery, not a build.\n--- output ---\n{stderr}",
            args.join(" "),
            lang.name,
        ),
        Some(_) => {}
    }
    Ok(start.elapsed().as_secs_f64() * 1000.0)
}

/// Bails loudly, naming the missing language, if any of `langs` has no
/// generated corpus subtree — shared by `prepare` and `measure_once` so
/// requesting an ungenerated language fails the same way (and this early)
/// regardless of which one is called first.
fn require_langs_generated(manifest: &CorpusManifest, langs: &[&Lang]) -> Result<()> {
    for lang in langs {
        if (lang.package_count)(manifest) == 0 {
            bail!(
                "corpus has no {n}/ subtree (generate with --{n}-packages > 0) — nothing for Tier B to build",
                n = lang.name
            );
        }
    }
    Ok(())
}

/// `corpus` must already be absolute (canonicalize before calling — see
/// `write_dist_config`'s comment on why a relative path breaks once `heph`
/// runs with `current_dir(corpus)`). `langs` is the full set of languages
/// this run measures — every one of them gets a plugin entry in the written
/// `.hephconfig` and a throwaway warmup build.
pub fn prepare(
    dist_dir: &Path,
    corpus: &Path,
    manifest: &CorpusManifest,
    scenario: Scenario,
    langs: &[&Lang],
) -> Result<()> {
    require_langs_generated(manifest, langs)?;
    let dist = Dist::locate(dist_dir)?;
    write_dist_config(corpus, &dist, langs)?;
    match scenario {
        Scenario::Cold | Scenario::FullHit | Scenario::Incremental => {
            wipe_cache(corpus)?;
            for lang in langs {
                build_lang_tree(&dist, corpus, lang)?;
            }
        }
    }
    Ok(())
}

/// One measured rep, across every language in `langs` — returned as
/// `(lang.name, elapsed_ms)` pairs, one per input language, **in the same
/// order `langs` was given** (callers rely on this to zip results back onto
/// their own per-language accumulators without a lookup). `mutate_seed` only
/// matters for `Incremental` (ignored otherwise) — the caller must vary it
/// across repeated calls against the same corpus, or every call mutates the
/// same files again. `corpus` must already be absolute, same as `prepare`.
pub fn measure_once(
    dist_dir: &Path,
    corpus: &Path,
    manifest: &CorpusManifest,
    scenario: Scenario,
    mutate_seed: u64,
    langs: &[&Lang],
) -> Result<Vec<(&'static str, f64)>> {
    require_langs_generated(manifest, langs)?;
    let dist = Dist::locate(dist_dir)?;
    write_dist_config(corpus, &dist, langs)?;

    if let Scenario::Cold = scenario {
        wipe_cache(corpus)?;
    }
    if let Scenario::Incremental = scenario {
        // Mutates each language's own subtree being built here, not the bash
        // pkgN packages `bench_corpus::incrementalize` touches — Tier B only
        // builds `//<lang>/...` trees.
        for lang in langs {
            (lang.incrementalize)(&corpus.join(lang.name), 0.01, 0xDEC1 ^ mutate_seed)
                .with_context(|| format!("mutate {} corpus for incremental scenario", lang.name))?;
        }
    }

    let mut results = Vec::with_capacity(langs.len());
    for lang in langs {
        results.push((lang.name, build_lang_tree(&dist, corpus, lang)?));
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// `write_dist_config` with a single language must produce the exact
    /// same base `plugins:` preamble the original go-only implementation
    /// did (plus `builtin: sh`, which every language now shares) — a
    /// regression here silently breaks `.hephconfig` for every existing
    /// `run dist` caller (bare `--lang go`, today's default).
    #[test]
    fn write_dist_config_go_only_matches_legacy_shape() {
        let dist_dir = tempfile::tempdir().expect("dist tempdir");
        let corpus_dir = tempfile::tempdir().expect("corpus tempdir");
        std::fs::write(dist_dir.path().join("heph"), b"fake").expect("write fake heph");
        std::fs::write(
            dist_dir.path().join(format!("heph-go-plugin.{DYLIB_EXT}")),
            b"fake dylib bytes",
        )
        .expect("write fake go dylib");

        let dist = Dist::locate(dist_dir.path()).expect("locate dist");
        write_dist_config(corpus_dir.path(), &dist, &[&GO]).expect("write config");

        let config = std::fs::read_to_string(corpus_dir.path().join(".hephconfig"))
            .expect("read .hephconfig");
        assert!(config.contains("builtin: buildfile"));
        assert!(config.contains("builtin: exec"));
        assert!(config.contains("builtin: bash"));
        assert!(config.contains("builtin: sh"));
        assert!(config.contains("heph-go-plugin.json"));
        assert!(config.contains("gotool: \"host\""));
        assert!(
            !config.contains("pkgmanager"),
            "go-only config must not mention js's pkgmanager option: {config}"
        );

        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(corpus_dir.path().join("heph-go-plugin.json"))
                .expect("read go manifest"),
        )
        .expect("parse go manifest");
        assert_eq!(manifest["name"], "go");
    }

    /// `--lang both`'s config must declare BOTH plugins in one file — this
    /// is what lets a single `heph r` process see both `//go/...` and
    /// `//js/...` targets without a second dlopen pass.
    #[test]
    fn write_dist_config_both_langs_declares_both_plugins() {
        let dist_dir = tempfile::tempdir().expect("dist tempdir");
        let corpus_dir = tempfile::tempdir().expect("corpus tempdir");
        std::fs::write(dist_dir.path().join("heph"), b"fake").expect("write fake heph");
        std::fs::write(
            dist_dir.path().join(format!("heph-go-plugin.{DYLIB_EXT}")),
            b"fake go dylib bytes",
        )
        .expect("write fake go dylib");
        std::fs::write(
            dist_dir.path().join(format!("heph-js-plugin.{DYLIB_EXT}")),
            b"fake js dylib bytes",
        )
        .expect("write fake js dylib");

        let dist = Dist::locate(dist_dir.path()).expect("locate dist");
        write_dist_config(corpus_dir.path(), &dist, &[&GO, &JS]).expect("write config");

        let config = std::fs::read_to_string(corpus_dir.path().join(".hephconfig"))
            .expect("read .hephconfig");
        assert!(config.contains("heph-go-plugin.json"));
        assert!(config.contains("heph-js-plugin.json"));
        assert!(config.contains("gotool: \"host\""));
        assert!(config.contains("pkgmanager: \"pnpm\""));
    }

    /// A missing js dylib must bail with a js-specific message, not a
    /// generic or go-flavored one — this is the loud-failure path an
    /// operator staging an incomplete dist directory hits.
    #[test]
    fn write_dist_config_missing_js_dylib_bails_naming_js() {
        let dist_dir = tempfile::tempdir().expect("dist tempdir");
        let corpus_dir = tempfile::tempdir().expect("corpus tempdir");
        std::fs::write(dist_dir.path().join("heph"), b"fake").expect("write fake heph");
        // No heph-js-plugin.<ext> written.

        let dist = Dist::locate(dist_dir.path()).expect("locate dist");
        let err = write_dist_config(corpus_dir.path(), &dist, &[&JS]).expect_err("must bail");
        let msg = format!("{err:#}");
        assert!(msg.contains("js plugin cdylib"), "{msg}");
        assert!(msg.contains("heph-js-plugin"), "{msg}");
    }

    /// `measure_once` must bail loudly, and name the missing language, when
    /// asked for a language whose corpus subtree was never generated —
    /// mirrors the original go-only "nothing for Tier B to build" bail.
    #[test]
    fn measure_once_bails_naming_missing_language_subtree() {
        let dist_dir = tempfile::tempdir().expect("dist tempdir");
        let corpus_dir = tempfile::tempdir().expect("corpus tempdir");
        std::fs::write(dist_dir.path().join("heph"), b"fake").expect("write fake heph");
        std::fs::write(
            dist_dir.path().join(format!("heph-js-plugin.{DYLIB_EXT}")),
            b"fake js dylib bytes",
        )
        .expect("write fake js dylib");

        let manifest = CorpusManifest {
            bash_addrs: Vec::new(),
            bash_prefix: String::new(),
            bash_packages: Vec::new(),
            go_package_count: 0,
            go_prefix: "go".to_string(),
            js_package_count: 0,
            js_prefix: "js".to_string(),
        };

        let err = measure_once(
            dist_dir.path(),
            corpus_dir.path(),
            &manifest,
            Scenario::Cold,
            0,
            &[&JS],
        )
        .expect_err("must bail — js_package_count is 0");
        let msg = format!("{err:#}");
        assert!(msg.contains("js/"), "{msg}");
        assert!(msg.contains("--js-packages"), "{msg}");
    }

    /// Locates the real `heph` binary this workspace produces for the
    /// current (non-release) profile, building it on demand if the profile
    /// directory this test binary itself was built into doesn't have one
    /// yet — e.g. a scoped `cargo test -p bench` run that never asked for
    /// the root package's bin target.
    ///
    /// `crates/bench` only depends on the `heph` *library* (for Tier A,
    /// in-process); nothing in a plain `cargo test -p bench` build graph
    /// forces the `heph` *binary* target to exist, so `build_lang_tree`
    /// (Tier B) has no artifact to spawn without this.
    fn locate_or_build_dev_heph() -> PathBuf {
        let mut profile_dir = std::env::current_exe().expect("current_exe (test binary path)");
        profile_dir.pop();
        if profile_dir.ends_with("deps") {
            profile_dir.pop();
        }
        let bin_name = if cfg!(windows) { "heph.exe" } else { "heph" };
        let candidate = profile_dir.join(bin_name);
        if !candidate.is_file() {
            let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .canonicalize()
                .expect("canonicalize workspace root (crates/bench/../..)");
            let mut cmd = Command::new(env!("CARGO"));
            cmd.args(["build", "--locked", "-p", "heph", "--bin", "heph"])
                .current_dir(&workspace_root);
            if profile_dir.ends_with("release") {
                cmd.arg("--release");
            }
            let status = cmd
                .status()
                .expect("spawn `cargo build -p heph --bin heph`");
            assert!(
                status.success(),
                "cargo build -p heph --bin heph failed with {status}"
            );
        }
        assert!(
            candidate.is_file(),
            "expected a `heph` binary at {} after building it — the assumption that this \
             test binary's profile dir ({}) matches where `cargo build -p heph --bin heph` \
             places its output may not hold here",
            candidate.display(),
            profile_dir.display(),
        );
        candidate
    }

    /// End-to-end proof that `build_lang_tree` actually selects and runs
    /// real targets against the real `heph` binary — not just that it
    /// constructs a config file and exits 0. This is the regression test
    /// for the "matched zero targets, exits 0 anyway" bug class this
    /// module's whole `matched_targets` mechanism exists to catch: covers
    /// the `label: None` (query-form) branch, since a fixture `bash` target
    /// carries no bench-specific label to select by.
    #[test]
    fn build_lang_tree_runs_real_targets_not_zero() {
        let heph_bin = locate_or_build_dev_heph();

        let dist_dir = tempfile::tempdir().expect("dist tempdir");
        std::fs::copy(&heph_bin, dist_dir.path().join("heph")).expect("copy heph binary");
        let dist = Dist::locate(dist_dir.path()).expect("locate dist");

        let corpus_dir = tempfile::tempdir().expect("corpus tempdir");
        let corpus = corpus_dir
            .path()
            .canonicalize()
            .expect("canonicalize corpus dir");
        std::fs::write(
            corpus.join(".hephconfig"),
            "plugins:\n  \
             - builtin: buildfile\n    options:\n      patterns:\n        - BUILD\n  \
             - builtin: bash\n",
        )
        .expect("write .hephconfig");

        // A fixture "language" subtree — same `//<name>/...` shape `Lang`
        // uses, just with a plain bash target instead of a real go/js
        // provider, so this test needs no plugin cdylib.
        let pkg_dir = corpus.join("testlang");
        std::fs::create_dir_all(&pkg_dir).expect("create testlang/ package dir");
        std::fs::write(
            pkg_dir.join("BUILD"),
            "target(\n    \
             name = \"t0\",\n    \
             driver = \"bash\",\n    \
             run = \"echo hi > $OUT\",\n    \
             out = \"t0.out\",\n\
             )\n",
        )
        .expect("write BUILD");

        let test_lang = Lang {
            name: "testlang",
            provider_options: "",
            label: None,
            package_count: |_| 1,
            incrementalize: |_, _, _| Ok(0),
        };

        let elapsed_ms = build_lang_tree(&dist, &corpus, &test_lang).expect("build_lang_tree");
        assert!(
            elapsed_ms >= 0.0,
            "elapsed_ms must be a real, non-negative measurement"
        );
        assert!(
            cache_dir(&corpus).is_dir(),
            "a real build must populate {} — the label-matcher bug this test guards against \
             matches zero targets and never touches the cache at all",
            cache_dir(&corpus).display(),
        );
    }
}
