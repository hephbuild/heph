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

pub mod pty;

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

    /// The temp `HOME` every [`Self::cmd`] runs with.
    pub fn home(&self) -> &Path {
        self.home.path()
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
    ///
    /// Bounded by [`RUN_DEADLINE`]: a hung heph is killed and the test fails
    /// naming the command, instead of stalling the suite until CI's timeout.
    pub fn run(&self, dist: &Dist, args: &[&str]) -> Result<Output> {
        let child = self
            .cmd(dist, args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn heph {}", args.join(" ")))?;
        wait_output(child, RUN_DEADLINE).with_context(|| format!("heph {}", args.join(" ")))
    }
}

/// How long any one heph run may take before the suite kills it: generous for
/// a release binary building a dependency on a shared CI runner.
pub const RUN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(300);

/// `wait_with_output`, bounded: past `timeout` the child is SIGKILLed (its
/// supervisor then reaps whatever it ran) and this is an error carrying what
/// it printed so far.
pub fn wait_output(child: std::process::Child, timeout: std::time::Duration) -> Result<Output> {
    let pid = i32::try_from(child.id()).context("pid")?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // The receiver gone means the caller already gave up on it.
        drop(tx.send(child.wait_with_output()));
    });
    match rx.recv_timeout(timeout) {
        Ok(out) => out.context("wait for heph"),
        Err(_) => {
            // SAFETY: a plain syscall on integers; the waiter thread still
            // owns the child, so the pid cannot have been reaped and reused.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            let out = rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .map(|o| o.map(|o| describe(&o)).unwrap_or_default())
                .unwrap_or_default();
            anyhow::bail!("still running after {timeout:?}; killed\n{out}")
        }
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

/// CSI 6n — Device Status Report, "where is the cursor?".
///
/// A pty is a pipe: nothing on the master end answers queries. The inline
/// viewport asks this while anchoring itself and **blocks until the terminal
/// replies**, so a harness that only reads deadlocks the child. Every pty test
/// therefore has to answer, and answer *every* occurrence — one missed query is
/// a permanent hang of the run under test.
pub const DSR_CURSOR: &[u8] = b"\x1b[6n";

/// Reply a real terminal would give: cursor at row 1, column 1. The value only
/// positions the inline viewport.
pub const DSR_REPLY: &[u8] = b"\x1b[1;1R";

/// Count the cursor-position queries in `tail`, and leave behind only what could
/// still be the start of another.
///
/// `tail` carries bytes across pty reads because a query can straddle a chunk
/// boundary. The rule both halves of that have to respect: consume through the
/// end of the **last** complete query, then keep `DSR_CURSOR.len() - 1` trailing
/// bytes so a straddling one is still assembled by the next read.
///
/// The earlier version cleared the whole tail whenever it matched, which is
/// wrong precisely when two queries arrive close together — the viewport rebuild
/// at resume and the one at teardown do exactly that. A chunk ending
/// `…\x1b[6n…\x1b[6` answered the first query and then dropped the three bytes
/// that begin the second, so the `n` opening the next chunk matched nothing, the
/// second query was never answered, and heph blocked forever inside
/// `Terminal::with_options` waiting for a reply that no longer had a sender.
/// That surfaced as a `child did not exit within 180s` with the run's last byte
/// being an unanswered `\x1b[6n` — a harness bug wearing the costume of a
/// product hang.
pub fn take_dsr_queries(tail: &mut Vec<u8>) -> usize {
    let mut queries = 0;
    let mut consumed = 0;
    while let Some(pos) = tail
        .get(consumed..)
        .and_then(|rest| rest.windows(DSR_CURSOR.len()).position(|w| w == DSR_CURSOR))
    {
        consumed += pos + DSR_CURSOR.len();
        queries += 1;
    }
    tail.drain(..consumed);
    // Whatever is left held no complete query, so only a possible prefix of one
    // is worth carrying.
    let straddle = DSR_CURSOR.len() - 1;
    if tail.len() > straddle {
        let excess = tail.len() - straddle;
        tail.drain(..excess);
    }
    queries
}

/// Send `sig` to `pid`.
pub fn signal(pid: i32, sig: i32) {
    // SAFETY: a plain syscall on integers.
    let rc = unsafe { libc::kill(pid, sig) };
    assert_eq!(
        rc,
        0,
        "kill({pid}, {sig}): {}",
        std::io::Error::last_os_error()
    );
}

/// Whether `pid` still exists (a zombie counts).
pub fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the pid exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Poll `cond` until it holds, or panic naming `what`.
pub fn wait_until(what: &str, timeout: std::time::Duration, mut cond: impl FnMut() -> bool) {
    let started = std::time::Instant::now();
    while !cond() {
        assert!(
            started.elapsed() < timeout,
            "timed out after {timeout:?} waiting for {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
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

#[cfg(test)]
mod dsr_tests {
    use super::{DSR_CURSOR, take_dsr_queries};

    /// The regression: a query that begins in the same chunk that ended a
    /// previous one must survive into the next read.
    #[test]
    fn a_query_straddling_a_boundary_after_another_is_still_answered() {
        let mut tail = b"\x1b[6nxx\x1b[6".to_vec();
        assert_eq!(take_dsr_queries(&mut tail), 1, "the complete query");
        tail.extend_from_slice(b"n");
        assert_eq!(
            take_dsr_queries(&mut tail),
            1,
            "the straddling query must be answered on the next read"
        );
    }

    /// Several in one chunk are all counted, so all get answered.
    #[test]
    fn every_query_in_a_chunk_is_counted() {
        let mut tail = b"a\x1b[6nb\x1b[6nc".to_vec();
        assert_eq!(take_dsr_queries(&mut tail), 2);
    }

    /// A chunk with no query keeps only a possible prefix — the tail must not
    /// grow without bound across a large transfer.
    #[test]
    fn a_miss_keeps_only_the_straddle_overlap() {
        let mut tail = vec![b'x'; 100_000];
        assert_eq!(take_dsr_queries(&mut tail), 0);
        assert_eq!(tail.len(), DSR_CURSOR.len() - 1);
    }

    /// An answered query is never answered twice: replying to a stale one wakes
    /// nothing and desynchronizes the next real reply.
    #[test]
    fn an_answered_query_is_not_seen_again() {
        let mut tail = b"\x1b[6n".to_vec();
        assert_eq!(take_dsr_queries(&mut tail), 1);
        tail.extend_from_slice(b"plain output");
        assert_eq!(take_dsr_queries(&mut tail), 0);
    }
}
