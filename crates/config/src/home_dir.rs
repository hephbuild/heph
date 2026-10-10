//! The workspace's heph home directory — where the local cache, sandboxes,
//! credentials, locks and diagnostics live.
//!
//! It is resolved **once**, by [`HomeDir::resolve`], into an absolute path, and
//! every other piece of code takes the resulting [`HomeDir`] rather than joining a
//! name onto the root itself. [`HomeDir`] has no public constructor besides the
//! resolver (and a test-only one), so a hand-built `root.join(".heph")` cannot
//! reach the engine: the place the home is decided is the only place it is.
//!
//! Directory names starting with [`HEPH_DIR_PREFIX`] (`.heph*`) are reserved:
//! every tree walk prunes them by name, so they are never source, wherever they
//! sit in the tree. The default home is one of them; a configured `homeDir`
//! need not be, and is pruned by its exact path instead.
//!
//! Engine-free on purpose: it sits next to [`get_root`](crate::get_root), so
//! everything that decides *where* a workspace lives is in one crate.

use std::ops::Deref;
use std::path::{Component, Path, PathBuf};

use anyhow::Context;

/// Name of the home directory under the workspace root when the config does not
/// set `homeDir`.
pub const DEFAULT_HOME_DIR: &str = ".heph";

/// Name prefix of a heph-owned directory: the default home (`.heph`), a
/// leftover one from an older default (`.heph3`), and the tool caches beside it
/// (`.heph-gocache`, …). Not the home's name — the home can be configured to
/// anything — but "this directory belongs to heph, not to the source tree".
/// Every tree walk prunes a directory whose name starts with it.
pub const HEPH_DIR_PREFIX: &str = ".heph";

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
    /// [`DEFAULT_HOME_DIR`] under `root`. There is no `~` or `$VAR` expansion.
    ///
    /// The result is made absolute with [`std::path::absolute`] and then
    /// normalized lexically (`.` dropped, `..` pops a component) — never
    /// canonicalized: the directory need not exist yet (the engine creates it),
    /// and canonicalizing would fail on a fresh workspace and resolve symlinks the
    /// user configured on purpose.
    ///
    /// A home that is the root itself or one of its ancestors (`.`, `..`, `/`,
    /// `sub/..`, an empty value) is refused. The home is pruned from every tree
    /// walk by exact path, so such a home prunes nothing: the cache and the
    /// sandboxes would be walked as source, and `heph tool gc`'s sweeps would run
    /// over the workspace's own directories.
    pub fn resolve(root: &Path, configured: Option<&Path>) -> anyhow::Result<Self> {
        let rel = configured.unwrap_or(Path::new(DEFAULT_HOME_DIR));
        let joined = root.join(rel);
        let home = normalize(&joined)?;
        let root = normalize(root)?;
        if root.starts_with(&home) {
            anyhow::bail!(
                "homeDir `{}` resolves to {}, which is the workspace root or above it; \
                 remove the key to use {DEFAULT_HOME_DIR}, or set a subdirectory",
                rel.display(),
                home.display(),
            );
        }
        Ok(Self(home))
    }

    /// The default home of a workspace rooted at `root` (a test's tempdir).
    ///
    /// Panics if the path cannot be resolved, which for a tempdir root it always
    /// can.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn for_tests(root: &Path) -> Self {
        Self::resolve(root, None).expect("resolving a test home dir")
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

/// [`std::path::absolute`], then resolve `.` and `..` components lexically —
/// without touching the filesystem, so a path that does not exist yet works (and
/// a symlink before a `..` is not followed).
///
/// The workspace root and the home both go through this: the home is pruned
/// from walks by exact path against paths joined onto the root, so the two must
/// be spelled the same way.
pub fn normalize(path: &Path) -> anyhow::Result<PathBuf> {
    let abs =
        std::path::absolute(path).with_context(|| format!("making {} absolute", path.display()))?;
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::CurDir => {}
            // `pop` at the root is a no-op, which is `/..` == `/`.
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    Ok(out)
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

    fn refused(configured: &str) -> String {
        let err = HomeDir::resolve(Path::new("/repo/ws"), Some(Path::new(configured)))
            .expect_err("a home at or above the root must be refused");
        format!("{err:#}")
    }

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
    fn dot_components_are_normalized() {
        let home =
            HomeDir::resolve(Path::new("/repo"), Some(Path::new("./a/../b/./h"))).expect("resolve");
        assert_eq!(home.as_path(), Path::new("/repo/b/h"));
    }

    #[test]
    fn result_is_absolute_even_for_a_relative_root() {
        let home = HomeDir::resolve(Path::new("ws"), None).expect("resolve");
        assert!(home.is_absolute(), "{}", home.display());
        assert!(home.ends_with(Path::new("ws").join(DEFAULT_HOME_DIR)));
    }

    #[test]
    fn empty_configured_is_refused() {
        assert!(refused("").contains("workspace root or above it"));
    }

    #[test]
    fn dot_is_refused() {
        let msg = refused(".");
        assert!(msg.contains("homeDir `.` resolves to /repo/ws,"), "{msg}");
        assert!(msg.contains("remove the key to use .heph"), "{msg}");
    }

    #[test]
    fn dot_slash_is_refused() {
        assert!(refused("./").contains("resolves to /repo/ws,"));
    }

    #[test]
    fn parent_is_refused() {
        assert!(refused("..").contains("resolves to /repo,"));
    }

    #[test]
    fn slash_is_refused() {
        assert!(refused("/").contains("resolves to /,"));
    }

    #[test]
    fn sub_dotdot_resolving_to_root_is_refused() {
        assert!(refused("sub/..").contains("resolves to /repo/ws,"));
    }

    #[test]
    fn a_sibling_sharing_the_roots_name_prefix_is_allowed() {
        // Component-wise, not string-wise: `/repo/ws-home` is not above `/repo/ws`.
        let home = HomeDir::resolve(Path::new("/repo/ws"), Some(Path::new("../ws-home")))
            .expect("resolve");
        assert_eq!(home.as_path(), Path::new("/repo/ws-home"));
    }
}
