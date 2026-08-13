//! Workspace-pinned self-upgrade.
//!
//! At startup heph reads the `version` field from the workspace `.hephconfig`
//! (via the engine-free [`hconfig`] loader, so this runs *before* the engine
//! boots). When that pin differs from the running binary, [`maybe_self_upgrade`]
//! downloads the pinned release for the host os/arch — once, under a cross-process
//! file lock — into `~/.heph/versions/<tag>/` and re-execs into it, replacing the
//! current process so the rest of the run is served by the pinned version.
//!
//! `versionFlavour` (default: empty, the "std" build) selects which published
//! artifact is downloaded: empty picks `heph_<os>_<arch>`, a named flavour (e.g.
//! `debug`, a build kept unstripped for backtraces) picks
//! `heph_<flavour>_<os>_<arch>`.
//!
//! The pin is read through the *lenient* [`hconfig::VersionPin`] loader, not the
//! full config shape: the binary that reads the pin is by definition the one that
//! may not understand the config the pinned version was written for. See
//! [`read_pin`].
//!
//! Only **exact** version pins are acted on today; a constraint expression (e.g.
//! `>=1.2, <2`) is recognized and skipped with a warning until resolution against
//! the release index is implemented.
//!
//! Guards against surprises and loops:
//! - dev builds (`v0.0.0-dev`) never self-upgrade — local development is in control;
//! - [`DISABLE_ENV`] (`HEPH_NO_SELF_UPDATE`) opts a process tree out entirely;
//! - the re-exec sets [`UPGRADED_ENV`] so the upgraded binary never re-upgrades,
//!   bounding the chain to a single hop even if a download reports a stale version.
//!
//! A version-number match alone isn't "nothing to do": `.hephconfig` can pin a
//! `versionFlavour` without bumping `version`. `hcore::version::flavour()` —
//! a post-build patch, not a compile-time constant, since both flavours of a
//! release share one compile — is what lets the running binary answer "which
//! flavour am I" directly, so [`decide`] can catch a flavour-only change too.

// Test code uses panicking helpers and fixture asserts; exempt the test cfg from
// the workspace restriction lints rather than rewriting each test. `allow` (not
// `expect`) since not every listed lint fires across this crate's small suite.
#![cfg_attr(
    test,
    allow(
        clippy::panic_in_result_fn,
        clippy::unwrap_used,
        clippy::indexing_slicing,
        clippy::assertions_on_result_states,
        reason = "restriction/style lints scoped to production code; tests are exempt"
    )
)]

use hcore::version;

/// Why a self-upgrade attempt could not complete. Lets the caller treat a
/// missing workspace config differently from a genuine upgrade failure (the
/// `version` command tolerates [`NoConfig`](SelfUpgradeError::NoConfig) so it
/// still works outside a workspace).
#[derive(Debug, thiserror::Error)]
pub enum SelfUpgradeError {
    /// No `.hephconfig` was found in this or any parent directory — not in a
    /// heph workspace, so there is no version pin to act on.
    #[error("no .hephconfig found in this or any parent directory")]
    NoConfig,
    /// The pin was read but the upgrade itself failed (config parse, download,
    /// install, or exec).
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

/// Base URL of the published release artifacts. Each release tags a set of
/// `heph_<os>_<arch>` (std flavour) and `heph_<flavour>_<os>_<arch>` (named
/// flavour, e.g. `debug`) binaries under `<base>/<tag>/`.
const ARTIFACTS_BASE: &str = "https://github.com/hephbuild/heph-artifacts-v1/releases/download";

/// The dev-build sentinel an artifact CI never stamped reports. Never
/// self-upgrades — a local/dev binary stays in charge.
///
/// Re-exported from `hcore` rather than spelled again here: the same string is
/// what `version::current()` falls back to for an unpatched version slot, and
/// two independent copies would drift into a dev binary that self-upgrades.
use hcore::version::DEV_VERSION;

/// Set to any non-empty value to disable self-upgrade for the whole process tree.
pub const DISABLE_ENV: &str = "HEPH_NO_SELF_UPDATE";

/// Set on the re-exec'd child so it does not attempt to upgrade again. Bounds the
/// exec chain to a single hop even if the downloaded binary reports an unexpected
/// version.
pub const UPGRADED_ENV: &str = "HEPH_SELF_UPDATED";

/// Outcome of comparing the running version against the workspace pin.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Decision {
    /// Running version already satisfies the pin — nothing to do.
    UpToDate,
    /// Pin differs; upgrade to this exact target tag.
    Upgrade { target: String },
    /// Pin can't be acted on (constraint expression, or unparseable). Skip with
    /// the carried reason logged.
    Unsupported { reason: String },
}

/// Read the workspace pin and, when it calls for a different exact version or
/// flavour, download it and re-exec into it. Returns `Ok(())` when nothing needs
/// to happen (no workspace, no pin, already current, dev build, or opted out).
/// On a successful upgrade this **does not return** — the process image is
/// replaced.
///
/// Errors are returned so the caller can log them, but a failed upgrade should
/// never be fatal: the caller is expected to warn and continue with the current
/// binary.
#[cfg(unix)]
pub fn maybe_self_upgrade() -> Result<(), SelfUpgradeError> {
    if env_opts_out() {
        return Ok(());
    }

    let current = version::current();
    // Not inside a heph workspace: no config, nothing to pin against. Surfaced as
    // a distinct error so the caller can tolerate it for `heph version`.
    let root = hconfig::get_root().map_err(|_e| SelfUpgradeError::NoConfig)?;
    let pin = read_pin(&root, &hconfig::profiles_from_env())?;
    let current_flavour = version::flavour();

    match plan(&pin, current, &current_flavour) {
        Decision::UpToDate => Ok(()),
        Decision::Unsupported { reason } => {
            let desired_version = pin.version.as_deref().unwrap_or_default();
            tracing::warn!(desired_version, current, %reason, "ignoring .hephconfig version pin");
            Ok(())
        }
        Decision::Upgrade { target } => {
            let binary = imp::ensure_binary(&target, pin_flavour(&pin))?;
            // Replaces the process image; only returns on failure.
            imp::exec_into(&binary)?;
            Ok(())
        }
    }
}

/// Read the workspace's version pin, tolerating a config this binary cannot
/// fully parse.
///
/// The pin decides *which binary should be running*, so reading it must not
/// depend on the running binary understanding the rest of the file — which is
/// exactly what the full (`deny_unknown_fields`) config shape would require. A
/// workspace pinning version N while using a key N introduced is unparseable by
/// N-1, and N-1 is precisely the binary that has to read the pin to hand over to
/// N; a downgrade pinning N-1 while using a key N *dropped* is the mirror image.
/// `hconfig::load_version_pin` looks at `version`/`versionFlavour` and skips
/// everything else, so the handover happens either way.
///
/// The strictness is deferred, not dropped: once the right binary is running, any
/// command that builds the engine loads the config in full and reports what it
/// doesn't understand. Commands that never build one (`heph version`) now run
/// against an unparseable config instead of failing the whole process on it.
///
/// `profiles` is threaded in rather than read from the environment here so this
/// path stays testable — see `hconfig::profiles_from_env`.
#[cfg(any(unix, test))]
fn read_pin(root: &std::path::Path, profiles: &[String]) -> anyhow::Result<hconfig::VersionPin> {
    hconfig::load_version_pin(root, profiles)
}

/// The pinned flavour, defaulting to the std (empty) build when unset.
#[cfg(any(unix, test))]
fn pin_flavour(pin: &hconfig::VersionPin) -> &str {
    pin.version_flavour.as_deref().unwrap_or("")
}

/// Turn a pin plus the running version/flavour into a [`Decision`]. The glue
/// [`maybe_self_upgrade`] runs, minus the filesystem and the exec — an unset
/// `version` means nothing to do, and an unset flavour means the std build.
#[cfg(any(unix, test))]
fn plan(pin: &hconfig::VersionPin, current: &str, current_flavour: &str) -> Decision {
    let Some(desired_version) = pin.version.as_deref() else {
        return Decision::UpToDate;
    };
    decide(current, current_flavour, desired_version, pin_flavour(pin))
}

#[cfg(not(unix))]
pub fn maybe_self_upgrade() -> Result<(), SelfUpgradeError> {
    // Self-upgrade relies on `execv`/`flock`; heph only ships on unix.
    Ok(())
}

/// Whether the running binary should refuse to self-upgrade based purely on the
/// environment + its own version: opted out, already upgraded once this chain, or
/// a dev build.
fn env_opts_out() -> bool {
    if std::env::var_os(UPGRADED_ENV).is_some() {
        return true;
    }
    if std::env::var_os(DISABLE_ENV).is_some_and(|v| !v.is_empty()) {
        return true;
    }
    version::current() == DEV_VERSION
}

/// Decide what to do given the running version+flavour and the configured
/// pin. A flavour mismatch forces an upgrade even when the version tag alone
/// matches — a workspace can pin the same `version` but change
/// `versionFlavour`.
fn decide(
    current: &str,
    current_flavour: &str,
    desired_version: &str,
    desired_flavour: &str,
) -> Decision {
    let desired_version = desired_version.trim();
    if desired_version.is_empty() {
        return Decision::UpToDate;
    }
    if is_constraint(desired_version) {
        return Decision::Unsupported {
            reason: "version constraints are not yet supported; pin an exact version".to_string(),
        };
    }
    let Some(target) = version::parse(desired_version) else {
        return Decision::Unsupported {
            reason: format!("`{desired_version}` is not a valid version"),
        };
    };
    let Some(running) = version::parse(current) else {
        return Decision::Unsupported {
            reason: format!("running version `{current}` is not a valid version"),
        };
    };
    // Build metadata is ignored when comparing (it is not part of version
    // identity); the core triple + pre-release decides equality.
    let version_matches = running.major == target.major
        && running.minor == target.minor
        && running.patch == target.patch
        && running.pre_release == target.pre_release;

    if version_matches && current_flavour == desired_flavour {
        Decision::UpToDate
    } else {
        Decision::Upgrade {
            target: desired_version.to_string(),
        }
    }
}

/// Whether `s` looks like a version *constraint* rather than an exact version.
/// Exact versions are a single bare token like `v1.2.3` / `1.2.3-rc.1`; anything
/// with a comparator operator, a comma, or whitespace-separated terms is a
/// constraint.
fn is_constraint(s: &str) -> bool {
    s.starts_with(['^', '~', '>', '<', '=', '*'])
        || s.contains(',')
        || s.split_whitespace().count() > 1
}

/// Host os/arch in the published-artifact spelling (`darwin`/`linux`,
/// `amd64`/`arm64`), matching the release asset names.
fn host_os_arch() -> (&'static str, &'static str) {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    (os, arch)
}

/// Release asset name for the host: `heph_<os>_<arch>` for the std (empty)
/// flavour, `heph_<flavour>_<os>_<arch>` for a named one (e.g. `debug`).
fn binary_name(flavour: &str, os: &str, arch: &str) -> String {
    if flavour.is_empty() {
        format!("heph_{os}_{arch}")
    } else {
        format!("heph_{flavour}_{os}_{arch}")
    }
}

/// Download URL for `tag`'s host binary in `flavour`:
/// `<base>/<tag>/heph_<os>_<arch>` (std), or
/// `<base>/<tag>/heph_<flavour>_<os>_<arch>` (named flavour).
fn download_url(tag: &str, flavour: &str, os: &str, arch: &str) -> String {
    format!("{ARTIFACTS_BASE}/{tag}/{}", binary_name(flavour, os, arch))
}

#[cfg(unix)]
mod imp {
    use super::{UPGRADED_ENV, binary_name, download_url, host_os_arch};
    use anyhow::{Context, anyhow};
    use std::io::{IsTerminal, Read, Write};
    use std::os::unix::io::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::thread::JoinHandle;
    use std::time::Duration;

    /// `~/.heph/versions/<tag>/` — where downloaded release binaries are cached,
    /// shared across workspaces (a pinned version is workspace-independent).
    fn version_cache_dir(tag: &str) -> anyhow::Result<PathBuf> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("HOME is not set; cannot locate ~/.heph version cache"))?;
        Ok(home.join(".heph").join("versions").join(tag))
    }

    /// Ensure `tag`'s host binary in `flavour` is present in the cache,
    /// downloading it once under an exclusive cross-process lock, and return its
    /// path.
    pub(super) fn ensure_binary(tag: &str, flavour: &str) -> anyhow::Result<PathBuf> {
        let (os, arch) = host_os_arch();
        let dir = version_cache_dir(tag)?;
        let dest = dir.join(binary_name(flavour, os, arch));
        if dest.exists() {
            return Ok(dest);
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;

        // Serialize concurrent downloads of this version across heph processes,
        // then re-check — another run may have installed it while we waited.
        let _lock = lock_exclusive(&dir.join("download.lock"))?;
        if dest.exists() {
            return Ok(dest);
        }

        let url = download_url(tag, flavour, os, arch);
        let bytes = download_with_ui(&url, tag)?;
        install_atomic(&dir, &dest, &bytes)?;
        Ok(dest)
    }

    /// Download `tag`'s binary, surfacing progress in the mode that fits the caller.
    ///
    /// - **Interactive** (stderr is a tty): a live spinner + byte progress on a
    ///   single stderr line, cleared completely on completion so no trace of the
    ///   upgrade is left behind.
    /// - **Non-interactive** (piped/CI logs): a single `tracing::info!` line, as
    ///   before — no cursor tricks that would garble a log file.
    fn download_with_ui(url: &str, tag: &str) -> anyhow::Result<Vec<u8>> {
        if !std::io::stderr().is_terminal() {
            tracing::info!("downloading heph {tag}");
            return download(url, None);
        }
        let progress = Arc::new(Progress::default());
        let spinner = spawn_spinner(tag.to_string(), Arc::clone(&progress));
        let result = download(url, Some(Arc::clone(&progress)));
        // Stop the spinner and let it wipe its line before we hand control back.
        progress.done.store(true, Ordering::SeqCst);
        drop(spinner.join());
        result
    }

    /// Live download progress shared between the fetch thread (writer) and the
    /// spinner thread (reader). `total` is 0 until the response headers arrive.
    #[derive(Default)]
    struct Progress {
        downloaded: AtomicU64,
        total: AtomicU64,
        done: AtomicBool,
    }

    const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

    /// Redraw the spinner line on stderr until `progress.done`, then clear it
    /// entirely (leaving no "upgraded" residue). Runs on its own thread so the
    /// blocking download can stream on another.
    fn spawn_spinner(tag: String, progress: Arc<Progress>) -> JoinHandle<()> {
        std::thread::spawn(move || {
            let mut err = std::io::stderr();
            let mut frame = 0usize;
            while !progress.done.load(Ordering::SeqCst) {
                let glyph = SPINNER_FRAMES
                    .get(frame % SPINNER_FRAMES.len())
                    .unwrap_or(&"");
                frame = frame.wrapping_add(1);
                let line = progress_line(
                    glyph,
                    &tag,
                    progress.downloaded.load(Ordering::Relaxed),
                    progress.total.load(Ordering::Relaxed),
                );
                // `\r` + `\x1b[K` overwrites the current line each tick.
                drop(write!(err, "\r{line}\x1b[K"));
                drop(err.flush());
                std::thread::sleep(Duration::from_millis(80));
            }
            // Wipe the line so nothing about the upgrade remains on screen.
            drop(write!(err, "\r\x1b[K"));
            drop(err.flush());
        })
    }

    /// The spinner's text (no cursor codes): `⠹ downloading heph v1.2.3 3.1/8.0 MiB`,
    /// dropping the total while it is still unknown.
    fn progress_line(glyph: &str, tag: &str, downloaded: u64, total: u64) -> String {
        if total > 0 {
            format!(
                "{glyph} downloading heph {tag} {}/{}",
                fmt_bytes(downloaded),
                fmt_bytes(total)
            )
        } else {
            format!("{glyph} downloading heph {tag} {}", fmt_bytes(downloaded))
        }
    }

    /// Human-readable byte count for the progress line.
    fn fmt_bytes(n: u64) -> String {
        const KIB: f64 = 1024.0;
        const MIB: f64 = 1024.0 * 1024.0;
        let bytes = n as f64;
        if bytes >= MIB {
            format!("{:.1} MiB", bytes / MIB)
        } else if bytes >= KIB {
            format!("{:.0} KiB", bytes / KIB)
        } else {
            format!("{n} B")
        }
    }

    /// Fetch `url` fully into memory, streaming so `progress` (when present) tracks
    /// bytes as they arrive. `reqwest::blocking` spins up its own runtime, so run it
    /// on a dedicated thread to stay safe if ever called from within an async
    /// runtime (matches the engine's plugin downloader).
    fn download(url: &str, progress: Option<Arc<Progress>>) -> anyhow::Result<Vec<u8>> {
        let url = url.to_string();
        std::thread::spawn(move || -> anyhow::Result<Vec<u8>> {
            let mut resp = reqwest::blocking::get(&url)
                .with_context(|| format!("GET {url}"))?
                .error_for_status()
                .with_context(|| format!("GET {url}"))?;
            if let Some(p) = &progress
                && let Some(len) = resp.content_length()
            {
                p.total.store(len, Ordering::Relaxed);
            }
            let mut buf = Vec::with_capacity(resp.content_length().unwrap_or(0) as usize);
            let mut chunk = [0u8; 64 * 1024];
            loop {
                let n = resp
                    .read(&mut chunk)
                    .with_context(|| format!("reading response body from {url}"))?;
                if n == 0 {
                    break;
                }
                let Some(part) = chunk.get(..n) else { break };
                buf.extend_from_slice(part);
                if let Some(p) = &progress {
                    p.downloaded.fetch_add(n as u64, Ordering::Relaxed);
                }
            }
            Ok(buf)
        })
        .join()
        .map_err(|_e| anyhow!("self-upgrade download thread panicked"))?
    }

    /// Write `bytes` to a temp file, mark it executable, then rename into place so
    /// a partial download is never seen as a usable binary by a concurrent run.
    fn install_atomic(dir: &Path, dest: &Path, bytes: &[u8]) -> anyhow::Result<()> {
        let tmp = dir.join(".heph.download");
        // `exec_into` execve's this binary moments later, so it must be written
        // with the writable-fd barrier: a thread forking during the write leaves
        // an inherited writable fd in the child until it reaches its own execve,
        // and an exec of a binary with a live writable fd fails with ETXTBSY.
        hcore::fsutil::write_executable(&tmp, bytes)?;
        std::fs::rename(&tmp, dest)
            .with_context(|| format!("install heph to {}", dest.display()))?;
        Ok(())
    }

    /// Replace the current process with `binary`, forwarding the original CLI
    /// arguments and marking the child so it won't self-upgrade again. Returns
    /// only if `execv` fails.
    pub(super) fn exec_into(binary: &Path) -> anyhow::Result<()> {
        let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        let err = std::process::Command::new(binary)
            .args(&args)
            .env(UPGRADED_ENV, "1")
            .exec();
        Err(err).with_context(|| format!("exec into {}", binary.display()))
    }

    /// An exclusive, advisory, cross-process file lock (`flock(2)`), released on
    /// drop. Serializes concurrent downloads of the same version across processes.
    struct FileLock {
        file: std::fs::File,
    }

    impl Drop for FileLock {
        fn drop(&mut self) {
            // SAFETY: `self.file` owns a valid fd for the lifetime of this guard.
            unsafe {
                libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }

    fn lock_exclusive(path: &Path) -> anyhow::Result<FileLock> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("open lock file {}", path.display()))?;
        // Blocking exclusive acquire; advisory and tied to the open file description.
        // SAFETY: `file` owns the fd; `flock` is valid for the call's duration.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("flock LOCK_EX on {}", path.display()));
        }
        Ok(FileLock { file })
    }

    #[cfg(test)]
    mod tests {
        use super::{fmt_bytes, install_atomic, progress_line};

        /// `install_atomic` is the last step before `exec_into` replaces the
        /// process image, and its errors are fatal (`main` turns them into
        /// `ExitCode::FAILURE`), so every command in a pinned workspace depends
        /// on it. Pin the contract end to end: the bytes land, the exec bit is
        /// set, the installed file actually runs, and no partial download is
        /// left behind for a concurrent run to mistake for a usable binary.
        #[test]
        fn install_atomic_writes_a_runnable_binary_and_leaves_no_partial() {
            let dir = tempfile::tempdir().expect("tempdir");
            let dest = dir.path().join("heph");

            install_atomic(dir.path(), &dest, b"#!/bin/sh\necho ok\n").expect("install");

            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&dest).expect("stat").permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "installed binary must be executable");

            let out = std::process::Command::new(&dest).output().expect("exec");
            assert!(out.status.success(), "installed binary failed: {out:?}");
            assert_eq!(String::from_utf8_lossy(&out.stdout), "ok\n");

            assert!(
                !dir.path().join(".heph.download").exists(),
                "the temp download must be renamed away, not left beside the binary"
            );
        }

        /// A failed install must say which step failed and must not leave a
        /// half-installed binary at `dest` — the next run would exec it.
        #[test]
        fn install_atomic_reports_the_failing_step_and_installs_nothing() {
            let dir = tempfile::tempdir().expect("tempdir");
            // `dest` is a non-empty directory: the rename cannot succeed.
            let dest = dir.path().join("occupied");
            std::fs::create_dir(&dest).expect("mkdir dest");
            std::fs::write(dest.join("child"), b"x").expect("occupy");

            let err = install_atomic(dir.path(), &dest, b"#!/bin/sh\necho ok\n")
                .expect_err("rename over a non-empty dir must fail");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("install heph to"),
                "error must name the step that failed, got: {msg}"
            );
            assert!(
                dest.is_dir(),
                "a failed install must not replace what was there"
            );
        }

        #[test]
        fn fmt_bytes_scales_by_unit() {
            assert_eq!(fmt_bytes(512), "512 B");
            assert_eq!(fmt_bytes(2048), "2 KiB");
            assert_eq!(fmt_bytes(3 * 1024 * 1024 + 100 * 1024), "3.1 MiB");
        }

        #[test]
        fn progress_line_includes_total_when_known() {
            assert_eq!(
                progress_line("⠹", "v1.2.3", 1024 * 1024, 8 * 1024 * 1024),
                "⠹ downloading heph v1.2.3 1.0 MiB/8.0 MiB"
            );
        }

        #[test]
        fn progress_line_drops_total_while_unknown() {
            // total == 0 means Content-Length was absent; show only what arrived.
            assert_eq!(
                progress_line("⠋", "v2.0.0", 4096, 0),
                "⠋ downloading heph v2.0.0 4 KiB"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(version: Option<&str>, flavour: Option<&str>) -> hconfig::VersionPin {
        hconfig::VersionPin {
            version: version.map(str::to_string),
            version_flavour: flavour.map(str::to_string),
        }
    }

    /// A workspace whose config this binary cannot fully parse must still hand
    /// over to the version it pins — otherwise the upgrade that would *fix* the
    /// parse error can never run. This is the seam that regressed: reading the pin
    /// through the strict loader made the whole run fatal. Covers an unknown
    /// top-level key (from a newer heph), an unknown *nested* key, and an unknown
    /// plugin field — the last two are rejected by their own deserializers rather
    /// than by the top-level struct.
    #[test]
    fn reads_the_pin_through_a_config_this_binary_cannot_parse() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(
            root.join(".hephconfig"),
            "version: v9.9.9\n\
             versionFlavour: debug\n\
             fromTheFuture: { deeply: [nested, 1, true] }\n\
             cache:\n  aKeyThisBinaryDropped: 1\n\
             plugins:\n  - builtin: buildfile\n    futureKnob: yes\n",
        )
        .expect("write config");

        // Sanity: the strict loader is exactly what would have blocked the
        // upgrade — if this ever stops erroring, the lenient path lost its point.
        assert!(
            hconfig::load_from_root(root).is_err(),
            "fixture must be unparseable under the full config shape"
        );

        let pin = read_pin(root, &[]).expect("pin must be readable regardless");
        assert_eq!(pin.version.as_deref(), Some("v9.9.9"));
        assert_eq!(pin.version_flavour.as_deref(), Some("debug"));
    }

    /// The pin read from such a config drives the decision the same as any other —
    /// the point of reading it is to act on it.
    #[test]
    fn plan_upgrades_from_a_pin() {
        assert_eq!(
            plan(&pin(Some("v2.0.0"), None), "v1.0.0", ""),
            Decision::Upgrade {
                target: "v2.0.0".to_string()
            }
        );
    }

    /// No pin is still no pin: a config without `version` must not invent one —
    /// the run continues on the current binary.
    #[test]
    fn plan_without_a_pin_is_a_noop() {
        assert_eq!(plan(&pin(None, None), "v1.0.0", ""), Decision::UpToDate);
        // …even when a flavour is pinned on its own: with no version there is no
        // release to fetch it from.
        assert_eq!(
            plan(&pin(None, Some("debug")), "v1.0.0", ""),
            Decision::UpToDate
        );
    }

    /// An omitted `versionFlavour` means the std build, so a std binary on a
    /// matching version has nothing to do — and a `debug` one does.
    #[test]
    fn plan_treats_an_omitted_flavour_as_std() {
        assert_eq!(
            plan(&pin(Some("v1.2.3"), None), "v1.2.3", ""),
            Decision::UpToDate
        );
        assert_eq!(
            plan(&pin(Some("v1.2.3"), None), "v1.2.3", "debug"),
            Decision::Upgrade {
                target: "v1.2.3".to_string()
            }
        );
    }

    /// `decide` with flavour held constant (both empty) — for tests that only
    /// care about version-number comparison; flavour comparison is covered
    /// separately below.
    fn decide_version_only(current: &str, desired_version: &str) -> Decision {
        decide(current, "", desired_version, "")
    }

    #[test]
    fn up_to_date_when_versions_match() {
        assert_eq!(decide_version_only("v1.2.3", "v1.2.3"), Decision::UpToDate);
        // Leading `v` is optional and build metadata is ignored.
        assert_eq!(decide_version_only("1.2.3", "v1.2.3"), Decision::UpToDate);
        assert_eq!(
            decide_version_only("v1.2.3+build.9", "v1.2.3"),
            Decision::UpToDate
        );
    }

    // A flavour-only edit (`.hephconfig` gains/changes `versionFlavour` without
    // bumping `version`) must still trigger a re-fetch — a version match alone
    // isn't "nothing to do".

    #[test]
    fn upgrades_on_flavour_mismatch_even_when_version_matches() {
        // Running the std ("") flavour; `.hephconfig` now asks for `debug` with
        // the *same* version pin — must not be a no-op.
        assert_eq!(
            decide("v1.2.3", "", "v1.2.3", "debug"),
            Decision::Upgrade {
                target: "v1.2.3".to_string()
            }
        );
        assert_eq!(
            decide("v1.2.3", "debug", "v1.2.3", ""),
            Decision::Upgrade {
                target: "v1.2.3".to_string()
            }
        );
    }

    #[test]
    fn up_to_date_when_version_and_flavour_both_match() {
        assert_eq!(
            decide("v1.2.3", "debug", "v1.2.3", "debug"),
            Decision::UpToDate
        );
        assert_eq!(decide("v1.2.3", "", "v1.2.3", ""), Decision::UpToDate);
    }

    #[test]
    fn unsupported_pin_ignored_regardless_of_flavour() {
        assert!(matches!(
            decide("v1.0.0", "", ">=1.2", "debug"),
            Decision::Unsupported { .. }
        ));
    }

    #[test]
    fn upgrade_when_versions_differ() {
        assert_eq!(
            decide_version_only("v1.2.3", "v1.3.0"),
            Decision::Upgrade {
                target: "v1.3.0".to_string()
            }
        );
        // Pre-release is part of identity: a release differs from its rc.
        assert_eq!(
            decide_version_only("v1.2.3-rc.1", "v1.2.3"),
            Decision::Upgrade {
                target: "v1.2.3".to_string()
            }
        );
    }

    #[test]
    fn target_string_preserves_pin_for_download_tag() {
        // The target carries the pin verbatim (trimmed) so the download tag
        // matches the release tag the user wrote.
        assert_eq!(
            decide_version_only("v1.0.0", "  v2.0.0  "),
            Decision::Upgrade {
                target: "v2.0.0".to_string()
            }
        );
    }

    #[test]
    fn constraints_are_unsupported() {
        for pin in [">=1.2", "^1.0.0", "~1.2.3", "1.2.* ", "1.0, 2.0", ">1 <2"] {
            assert!(
                matches!(
                    decide_version_only("v1.0.0", pin),
                    Decision::Unsupported { .. }
                ),
                "expected {pin:?} to be unsupported"
            );
        }
    }

    #[test]
    fn unparseable_pin_is_unsupported() {
        assert!(matches!(
            decide_version_only("v1.0.0", "banana"),
            Decision::Unsupported { .. }
        ));
    }

    #[test]
    fn empty_pin_is_noop() {
        assert_eq!(decide_version_only("v1.0.0", "   "), Decision::UpToDate);
    }

    #[test]
    fn is_constraint_classifies() {
        assert!(is_constraint(">=1.2"));
        assert!(is_constraint("^1.0"));
        assert!(is_constraint("~1.0"));
        assert!(is_constraint("1.0,2.0"));
        assert!(is_constraint("1.0 2.0"));
        assert!(is_constraint("*"));
        assert!(!is_constraint("v1.2.3"));
        assert!(!is_constraint("1.2.3-rc.1"));
    }

    #[test]
    fn download_url_is_well_formed() {
        assert_eq!(
            download_url("v1.2.3", "", "darwin", "arm64"),
            "https://github.com/hephbuild/heph-artifacts-v1/releases/download/v1.2.3/heph_darwin_arm64"
        );
    }

    #[test]
    fn download_url_includes_flavour_when_set() {
        assert_eq!(
            download_url("v1.2.3", "debug", "darwin", "arm64"),
            "https://github.com/hephbuild/heph-artifacts-v1/releases/download/v1.2.3/heph_debug_darwin_arm64"
        );
    }

    #[test]
    fn binary_name_per_platform() {
        assert_eq!(binary_name("", "linux", "amd64"), "heph_linux_amd64");
    }

    #[test]
    fn binary_name_includes_flavour_when_set() {
        assert_eq!(
            binary_name("debug", "linux", "amd64"),
            "heph_debug_linux_amd64"
        );
    }
}
