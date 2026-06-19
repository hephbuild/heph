//! Workspace-pinned self-upgrade.
//!
//! At startup heph reads the `version` field from the workspace `.hephconfig`
//! (via the engine-free [`hconfig`] loader, so this runs *before* the engine
//! boots). When that pin differs from the running binary, [`maybe_self_upgrade`]
//! downloads the pinned release for the host os/arch — once, under a cross-process
//! file lock — into `~/.heph/versions/<tag>/` and re-execs into it, replacing the
//! current process so the rest of the run is served by the pinned version.
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
/// `heph_<os>_<arch>` binaries under `<base>/<tag>/`.
const ARTIFACTS_BASE: &str = "https://github.com/hephbuild/heph-artifacts-v1/releases/download";

/// The dev-build sentinel stamped when `HEPH_BUILD_VERSION` is unset. Never
/// self-upgrades — a local/dev binary stays in charge.
const DEV_VERSION: &str = "v0.0.0-dev";

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

/// Read the workspace pin and, when it calls for a different exact version,
/// download it and re-exec into it. Returns `Ok(())` when nothing needs to happen
/// (no workspace, no pin, already current, dev build, or opted out). On a
/// successful upgrade this **does not return** — the process image is replaced.
///
/// Errors are returned so the caller can log them, but a failed upgrade should
/// never be fatal: the caller is expected to warn and continue with the current
/// binary.
#[cfg(unix)]
pub fn maybe_self_upgrade() -> Result<(), SelfUpgradeError> {
    if env_opts_out() {
        return Ok(());
    }

    let current = version::VERSION;
    // Not inside a heph workspace: no config, nothing to pin against. Surfaced as
    // a distinct error so the caller can tolerate it for `heph version`.
    let root = hconfig::get_root().map_err(|_e| SelfUpgradeError::NoConfig)?;
    let cfg = hconfig::load_from_root(&root)?;
    let Some(pin) = cfg.version.as_deref() else {
        return Ok(());
    };

    match decide(current, pin) {
        Decision::UpToDate => Ok(()),
        Decision::Unsupported { reason } => {
            tracing::warn!(pin, current, %reason, "ignoring .hephconfig version pin");
            Ok(())
        }
        Decision::Upgrade { target } => {
            tracing::info!(from = current, to = %target, "self-upgrading heph");
            let binary = imp::ensure_binary(&target)?;
            // Replaces the process image; only returns on failure.
            imp::exec_into(&binary)?;
            Ok(())
        }
    }
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
    version::VERSION == DEV_VERSION
}

/// Decide what to do given the running version and the configured pin.
fn decide(current: &str, pin: &str) -> Decision {
    let pin = pin.trim();
    if pin.is_empty() {
        return Decision::UpToDate;
    }
    if is_constraint(pin) {
        return Decision::Unsupported {
            reason: "version constraints are not yet supported; pin an exact version".to_string(),
        };
    }
    let Some(target) = version::parse(pin) else {
        return Decision::Unsupported {
            reason: format!("`{pin}` is not a valid version"),
        };
    };
    let Some(running) = version::parse(current) else {
        return Decision::Unsupported {
            reason: format!("running version `{current}` is not a valid version"),
        };
    };
    // Build metadata is ignored when comparing (it is not part of version
    // identity); the core triple + pre-release decides equality.
    if running.major == target.major
        && running.minor == target.minor
        && running.patch == target.patch
        && running.pre_release == target.pre_release
    {
        Decision::UpToDate
    } else {
        Decision::Upgrade {
            target: pin.to_string(),
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

/// Release asset name for the host: `heph_<os>_<arch>`.
fn binary_name(os: &str, arch: &str) -> String {
    format!("heph_{os}_{arch}")
}

/// Download URL for `tag`'s host binary: `<base>/<tag>/heph_<os>_<arch>`.
fn download_url(tag: &str, os: &str, arch: &str) -> String {
    format!("{ARTIFACTS_BASE}/{tag}/{}", binary_name(os, arch))
}

#[cfg(unix)]
mod imp {
    use super::{UPGRADED_ENV, binary_name, download_url, host_os_arch};
    use anyhow::{Context, anyhow};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::io::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};

    /// `~/.heph/versions/<tag>/` — where downloaded release binaries are cached,
    /// shared across workspaces (a pinned version is workspace-independent).
    fn version_cache_dir(tag: &str) -> anyhow::Result<PathBuf> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("HOME is not set; cannot locate ~/.heph version cache"))?;
        Ok(home.join(".heph").join("versions").join(tag))
    }

    /// Ensure `tag`'s host binary is present in the cache, downloading it once
    /// under an exclusive cross-process lock, and return its path.
    pub(super) fn ensure_binary(tag: &str) -> anyhow::Result<PathBuf> {
        let (os, arch) = host_os_arch();
        let dir = version_cache_dir(tag)?;
        let dest = dir.join(binary_name(os, arch));
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

        let url = download_url(tag, os, arch);
        tracing::info!("downloading heph {tag}");
        let bytes = download(&url)?;
        install_atomic(&dir, &dest, &bytes)?;
        Ok(dest)
    }

    /// Fetch `url` fully into memory. `reqwest::blocking` spins up its own runtime,
    /// so run it on a dedicated thread to stay safe if ever called from within an
    /// async runtime (matches the engine's plugin downloader).
    fn download(url: &str) -> anyhow::Result<Vec<u8>> {
        let url = url.to_string();
        std::thread::spawn(move || -> anyhow::Result<Vec<u8>> {
            let resp = reqwest::blocking::get(&url)
                .with_context(|| format!("GET {url}"))?
                .error_for_status()
                .with_context(|| format!("GET {url}"))?;
            Ok(resp.bytes()?.to_vec())
        })
        .join()
        .map_err(|_e| anyhow!("self-upgrade download thread panicked"))?
    }

    /// Write `bytes` to a temp file, mark it executable, then rename into place so
    /// a partial download is never seen as a usable binary by a concurrent run.
    fn install_atomic(dir: &Path, dest: &Path, bytes: &[u8]) -> anyhow::Result<()> {
        use std::io::Write;
        let tmp = dir.join(".heph.download");
        {
            let mut f =
                std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
            f.write_all(bytes)?;
            f.flush()?;
        }
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod {}", tmp.display()))?;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn up_to_date_when_versions_match() {
        assert_eq!(decide("v1.2.3", "v1.2.3"), Decision::UpToDate);
        // Leading `v` is optional and build metadata is ignored.
        assert_eq!(decide("1.2.3", "v1.2.3"), Decision::UpToDate);
        assert_eq!(decide("v1.2.3+build.9", "v1.2.3"), Decision::UpToDate);
    }

    #[test]
    fn upgrade_when_versions_differ() {
        assert_eq!(
            decide("v1.2.3", "v1.3.0"),
            Decision::Upgrade {
                target: "v1.3.0".to_string()
            }
        );
        // Pre-release is part of identity: a release differs from its rc.
        assert_eq!(
            decide("v1.2.3-rc.1", "v1.2.3"),
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
            decide("v1.0.0", "  v2.0.0  "),
            Decision::Upgrade {
                target: "v2.0.0".to_string()
            }
        );
    }

    #[test]
    fn constraints_are_unsupported() {
        for pin in [">=1.2", "^1.0.0", "~1.2.3", "1.2.* ", "1.0, 2.0", ">1 <2"] {
            assert!(
                matches!(decide("v1.0.0", pin), Decision::Unsupported { .. }),
                "expected {pin:?} to be unsupported"
            );
        }
    }

    #[test]
    fn unparseable_pin_is_unsupported() {
        assert!(matches!(
            decide("v1.0.0", "banana"),
            Decision::Unsupported { .. }
        ));
    }

    #[test]
    fn empty_pin_is_noop() {
        assert_eq!(decide("v1.0.0", "   "), Decision::UpToDate);
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
            download_url("v1.2.3", "darwin", "arm64"),
            "https://github.com/hephbuild/heph-artifacts-v1/releases/download/v1.2.3/heph_darwin_arm64"
        );
    }

    #[test]
    fn binary_name_per_platform() {
        assert_eq!(binary_name("linux", "amd64"), "heph_linux_amd64");
    }
}
