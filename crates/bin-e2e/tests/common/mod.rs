//! Harness for the binary end-to-end suite.
//!
//! Every test here drives the *shipped* artifacts — the `heph` binary CI
//! uploads and the plugin cdylibs published alongside it — as a child process.
//! Nothing in this crate links a workspace crate.
//!
//! Scope rule: a test belongs here only if an in-process test structurally
//! cannot cover it. Today that means three seams:
//!
//! - **the loader** — `dlopen` of a real cdylib, ABI negotiation across the
//!   stabby seam, manifest/checksum resolution. An in-process test constructs
//!   the plugin directly and never crosses the seam.
//! - **the terminal** — the interactive TUI only engages when stderr is a tty,
//!   and it takes over the alternate screen. Needs a real PTY.
//! - **the process** — exit status, and the fact that the binary launches at
//!   all on this host (dyld/glibc resolution of the shipped bytes).
//!
//! Engine semantics, caching, provider logic: `crates/e2e`, not here.

#![allow(
    dead_code,
    reason = "this module is compiled into every test binary; each uses a subset of it"
)]

use anyhow::{Context as _, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

/// Env var naming the staged artifact directory.
///
/// Set by the `e2e` devenv script, which is the only supported entrypoint —
/// locally and in CI alike. When it is missing the tests fail rather than skip:
/// a suite that never ran must not read as a suite that passed.
pub const DIST_ENV: &str = "HEPH_E2E_DIST";

/// Native shared-library extension for the host, matching the suffix the build
/// job gives the published plugin cdylibs.
pub const DYLIB_EXT: &str = if cfg!(target_os = "macos") {
    "dylib"
} else {
    "so"
};

/// The staged artifact directory: a normalized layout (`heph`,
/// `heph-<name>-plugin.<ext>`) that the `e2e` script produces identically from
/// a local release build and from CI's downloaded artifacts.
pub struct Dist {
    root: PathBuf,
}

impl Dist {
    pub fn locate() -> Self {
        let root = PathBuf::from(std::env::var_os(DIST_ENV).expect(
            "HEPH_E2E_DIST is not set — run this suite through the `e2e` devenv script \
             (it stages the artifacts and sets it), not `cargo test` directly",
        ));
        assert!(
            root.join("heph").is_file(),
            "{DIST_ENV}={} does not contain a `heph` binary",
            root.display()
        );
        Self { root }
    }

    pub fn heph(&self) -> PathBuf {
        self.root.join("heph")
    }

    /// Path to a published plugin cdylib, e.g. `plugin("go")` →
    /// `<dist>/heph-go-plugin.<ext>`.
    pub fn plugin(&self, name: &str) -> PathBuf {
        self.root.join(format!("heph-{name}-plugin.{DYLIB_EXT}"))
    }
}

/// A throwaway workspace: a temp tree with a `.hephconfig`, plus a temp `HOME`
/// so nothing the run does can touch the developer's real one.
pub struct Workspace {
    dir: TempDir,
    home: TempDir,
}

/// The plugin set every fixture starts from — enough to declare and run a
/// trivial target, nothing more.
pub const BASE_CONFIG: &str = "plugins:\n  - builtin: buildfile\n    options:\n      patterns:\n        - BUILD\n  - builtin: exec\n  - builtin: bash\n";

impl Workspace {
    pub fn new() -> Result<Self> {
        let ws = Self {
            dir: tempfile::tempdir().context("create workspace tempdir")?,
            home: tempfile::tempdir().context("create home tempdir")?,
        };
        ws.config(BASE_CONFIG)?;
        Ok(ws)
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Overwrite `.hephconfig`. Callers that need a plugin append to
    /// [`BASE_CONFIG`] rather than restating the builtins.
    pub fn config(&self, yaml: &str) -> Result<()> {
        std::fs::write(self.root().join(".hephconfig"), yaml).context("write .hephconfig")
    }

    pub fn write(&self, rel: impl AsRef<Path>, contents: &str) -> Result<()> {
        let path = self.root().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::write(&path, contents).with_context(|| format!("write {}", path.display()))
    }

    /// A `heph` invocation rooted at this workspace, with the ambient
    /// environment neutralized: self-update off (a run must not re-exec into
    /// some other binary mid-test), telemetry off, `HOME` redirected.
    pub fn cmd(&self, dist: &Dist, args: &[&str]) -> Command {
        let mut cmd = Command::new(dist.heph());
        cmd.args(args)
            .current_dir(self.root())
            .env("HOME", self.home.path())
            .env("HEPH_CWD", self.root())
            .env("HEPH_NO_SELF_UPDATE", "1")
            .env("HEPH_DISABLE_TELEMETRY", "1")
            .env("RUST_BACKTRACE", "1");
        cmd
    }

    /// Run to completion with both streams captured (so stderr is *not* a tty
    /// and the CI line renderer is selected).
    pub fn run(&self, dist: &Dist, args: &[&str]) -> Result<Output> {
        self.cmd(dist, args)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("spawn heph {}", args.join(" ")))
    }
}

/// Render a captured run for an assertion message. Test failures here are
/// investigated from CI logs, so the whole output goes in the panic.
pub fn describe(out: &Output) -> String {
    format!(
        "status: {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    )
}

/// Write a plugin distribution manifest (`*-plugin.json`) naming `dylib` as the
/// artifact for this host — the same shape `tools/pluginmanifest` emits, built
/// here so the suite needs no Go toolchain.
///
/// `checksum` is what heph verifies the artifact bytes against before loading;
/// [`sha256_file`] produces the honest one, and a wrong value is how the
/// rejection path gets tested.
pub fn write_manifest(path: &Path, name: &str, dylib: &Path, checksum: Option<&str>) -> Result<()> {
    let (os, arch) = host_os_arch();
    let mut artifact = serde_json::Map::new();
    artifact.insert("os".into(), os.into());
    artifact.insert("arch".into(), arch.into());
    artifact.insert("path".into(), dylib.to_string_lossy().into_owned().into());
    if let Some(sum) = checksum {
        artifact.insert("checksum".into(), sum.into());
    }
    let manifest = serde_json::json!({
        "name": name,
        "version": "e2e",
        "artifacts": [artifact],
    });
    let bytes = serde_json::to_vec_pretty(&manifest).context("encode plugin manifest")?;
    std::fs::write(path, bytes).with_context(|| format!("write {}", path.display()))
}

/// `sha256:<hex>` of a file, in the spelling the manifest's `checksum` field
/// takes.
pub fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest as _, Sha256};

    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(&bytes))))
}

/// Host os/arch in the published-artifact spelling heph's manifest resolver
/// matches on (`darwin`/`linux`, `amd64`/`arm64`).
pub fn host_os_arch() -> (&'static str, &'static str) {
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "amd64"
    };
    (os, arch)
}
