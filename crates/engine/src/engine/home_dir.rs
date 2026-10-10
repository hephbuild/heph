//! The workspace's heph home directory — where the local cache, sandboxes,
//! credentials, locks and diagnostics live.
//!
//! It is resolved **once**, by [`HomeDir::resolve`], into an absolute path, and
//! every other piece of code takes the resulting [`HomeDir`] rather than joining a
//! name onto the root itself. [`HomeDir`] has no public constructor besides the
//! resolver (and a test-only one), so a hand-built `root.join(".heph")` cannot
//! reach the engine: the place the home is decided is the only place it is.

use std::ops::Deref;
use std::path::{Path, PathBuf};

use anyhow::Context;

/// Name of the home directory under the workspace root when the config does not
/// set `homeDir`.
pub const DEFAULT_HOME_DIR: &str = ".heph";

/// The resolved, absolute heph home directory of a workspace.
///
/// Derefs to [`Path`], so `home.join("cache")` and passing `&home` where a
/// `&Path` is expected both work; there is deliberately no way to build one from
/// an arbitrary path outside [`HomeDir::resolve`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HomeDir(PathBuf);

impl HomeDir {
    /// Resolve the home directory of the workspace at `root`.
    ///
    /// `configured` is the config file's `homeDir`: a relative path is joined
    /// onto `root`, an absolute one is used as written, and `None` means
    /// [`DEFAULT_HOME_DIR`] under `root`.
    ///
    /// The result is made absolute with [`std::path::absolute`] (against the cwd,
    /// should `root` itself be relative), not canonicalized: the directory need
    /// not exist yet — the engine creates it — and canonicalizing would fail on a
    /// fresh workspace and resolve symlinks the user configured on purpose.
    pub fn resolve(root: &Path, configured: Option<&Path>) -> anyhow::Result<Self> {
        let rel = match configured {
            Some(p) if p.as_os_str().is_empty() => {
                // `root.join("")` is the root itself: the cache would be the
                // workspace, and `clean` would delete it.
                anyhow::bail!("homeDir must not be empty");
            }
            Some(p) => p,
            None => Path::new(DEFAULT_HOME_DIR),
        };
        let joined = root.join(rel);
        let abs = std::path::absolute(&joined)
            .with_context(|| format!("making home dir {} absolute", joined.display()))?;
        Ok(Self(abs))
    }

    /// The default home of a workspace rooted at `root` (a test's tempdir).
    ///
    /// Panics if the path cannot be made absolute, which for a tempdir root it
    /// always can.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn for_tests(root: &Path) -> Self {
        Self::resolve(root, None).expect("resolving a test home dir")
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl Deref for HomeDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for HomeDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_dot_heph_under_root() {
        let home = HomeDir::resolve(Path::new("/repo"), None).expect("resolve");
        assert_eq!(home.as_path(), Path::new("/repo/.heph"));
    }

    #[test]
    fn relative_joins_root() {
        let home =
            HomeDir::resolve(Path::new("/repo"), Some(Path::new("state/h"))).expect("resolve");
        assert_eq!(home.as_path(), Path::new("/repo/state/h"));
    }

    #[test]
    fn absolute_is_used_as_written() {
        let home =
            HomeDir::resolve(Path::new("/repo"), Some(Path::new("/var/heph"))).expect("resolve");
        assert_eq!(home.as_path(), Path::new("/var/heph"));
    }

    #[test]
    fn result_is_absolute_even_for_a_relative_root() {
        let home = HomeDir::resolve(Path::new("ws"), None).expect("resolve");
        assert!(home.is_absolute(), "{}", home.display());
        assert!(home.ends_with(Path::new("ws").join(DEFAULT_HOME_DIR)));
    }

    #[test]
    fn empty_configured_is_refused() {
        let err = HomeDir::resolve(Path::new("/repo"), Some(Path::new("")))
            .expect_err("an empty homeDir must be refused");
        assert!(err.to_string().contains("homeDir"), "{err:#}");
    }
}
