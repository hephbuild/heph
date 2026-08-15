use crate::config_yaml::{CONFIG_FILE_NAME, CONFIG_FILE_NAMES};
use anyhow::anyhow;
use memoize::memoize;
use std::env;
use std::path::{Path, PathBuf};

/// Current working directory heph operates from. Honors `HEPH_CWD` (used by tests
/// and tooling to point heph at another tree) before falling back to the real
/// process cwd.
///
/// Canonicalized before returning: `HEPH_CWD` is caller-supplied and may be
/// symlinked (e.g. a test harness pointing it at a raw `tempfile::tempdir()`
/// path — on macOS, `/tmp`/`/var` are themselves symlinks to
/// `/private/tmp`/`/private/var`). Every absolute path the engine later builds
/// off this root (`sandbox_dir` and everything a managed driver derives from
/// it) inherits whatever string this function returns; a real subprocess a
/// driver spawns that does its own filesystem walk (confirmed live: a real
/// `vitest` run) resolves symlinks on its own, so an uncanonicalized root here
/// silently produces two different path strings for the same file — see
/// `Engine::new`'s doc comment for the exact failure this caused
/// ("No test files found" against a file that plainly existed).
/// `env::current_dir()` is already effectively canonical (backed by
/// `getcwd(2)`, which resolves symlinks), so this is a defensive no-op on
/// that branch and load-bearing only for `HEPH_CWD`.
pub fn get_cwd() -> anyhow::Result<PathBuf> {
    get_cwd_inner().map_err(|e| anyhow!(e))
}

#[memoize]
fn get_cwd_inner() -> Result<PathBuf, String> {
    resolve_cwd(env::var_os("HEPH_CWD").as_deref())
}

/// The actual logic behind [`get_cwd`], parameterized on the `HEPH_CWD` value
/// rather than reading the environment itself — `get_cwd_inner` is `#[memoize]`d
/// with no arguments, so it caches its *first* answer for the rest of the
/// process; that makes it untestable directly in a shared test binary (whichever
/// test or test-ordering happens to call `get_cwd`/`get_root` first wins for
/// every test after it). This half, with no memoization and no hidden global
/// read, is what unit tests actually exercise.
fn resolve_cwd(heph_cwd: Option<&std::ffi::OsStr>) -> Result<PathBuf, String> {
    let cwd = match heph_cwd {
        Some(v) => Path::new(v).to_path_buf(),
        None => env::current_dir().map_err(|e| e.to_string())?,
    };
    std::fs::canonicalize(&cwd).map_err(|e| format!("canonicalize cwd {}: {e}", cwd.display()))
}

/// Walk up from [`get_cwd`] until a workspace config file is found, returning the
/// directory that holds it (the workspace root). Errors if none is found in any
/// parent.
pub fn get_root() -> anyhow::Result<PathBuf> {
    get_root_inner().map_err(|e| anyhow!(e))
}

#[memoize]
fn get_root_inner() -> Result<PathBuf, String> {
    let cwd = get_cwd().map_err(|e| e.to_string())?;
    let mut current = Path::new(&cwd).to_path_buf();

    loop {
        let found = CONFIG_FILE_NAMES
            .iter()
            .any(|name| current.join(name).exists());
        if found {
            return Ok(current);
        }

        match current.parent().map(|p| p.to_path_buf()) {
            Some(parent) => current = parent,
            None => break,
        }
    }

    Err(format!(
        "Could not find {CONFIG_FILE_NAME} file in any parent directory"
    ))
}

#[cfg(test)]
mod tests {
    use super::resolve_cwd;

    /// `HEPH_CWD` is exactly what `crates/bin-e2e`'s test harness sets it to
    /// (`.env("HEPH_CWD", <a raw tempfile::tempdir() path>)`) — a symlink on
    /// macOS is the realistic case (`/tmp`/`/var` -> `/private/tmp`/
    /// `/private/var`), reproduced portably here with an explicit symlink
    /// rather than relying on the host's own `TMPDIR` happening to be one.
    #[test]
    #[cfg(unix)]
    fn resolve_cwd_canonicalizes_a_symlinked_heph_cwd() {
        let real = tempfile::tempdir().expect("tempdir");
        let real_canonical = std::fs::canonicalize(real.path()).expect("canonicalize");

        let link = real.path().with_file_name(format!(
            "link-{}",
            real.path()
                .file_name()
                .expect("tempdir has a file name")
                .to_string_lossy()
        ));
        std::os::unix::fs::symlink(real.path(), &link).expect("symlink");
        struct RemoveOnDrop(std::path::PathBuf);
        impl Drop for RemoveOnDrop {
            fn drop(&mut self) {
                drop(std::fs::remove_file(&self.0));
            }
        }
        let _cleanup = RemoveOnDrop(link.clone());

        let resolved = resolve_cwd(Some(link.as_os_str())).expect("resolve_cwd");
        assert_eq!(
            resolved, real_canonical,
            "a symlinked HEPH_CWD must resolve to the real, canonical directory — otherwise \
             every absolute path the engine builds off it (sandbox_dir and everything a driver \
             derives from it) disagrees with what a real subprocess's own filesystem walk \
             resolves"
        );
    }

    #[test]
    fn resolve_cwd_falls_back_to_process_cwd_when_unset() {
        let expected =
            std::fs::canonicalize(std::env::current_dir().expect("current_dir")).expect("cwd");
        let resolved = resolve_cwd(None).expect("resolve_cwd");
        assert_eq!(resolved, expected);
    }
}
