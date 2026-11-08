//! Path forms the index uses: root-relative normalised bytes for rows, this
//! checkout's db file, and the lock files the engine takes.

use std::borrow::Cow;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path, PathBuf};
use xxhash_rust::xxh3::xxh3_64;

/// A root-relative path as a `&Path`.
pub fn rel_path(rel: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(rel))
}

/// Whether `rel` is already a normalised root-relative path: non-empty, no
/// leading `/`, and no empty, `.` or `..` component.
fn is_normal(rel: &[u8]) -> bool {
    !rel.is_empty()
        && rel
            .split(|&c| c == b'/')
            .all(|c| !c.is_empty() && c != b"." && c != b"..")
}

/// `path` (absolute under `root`, or relative to it) as a normalised
/// root-relative path: `/`-separated, no `.`, `..` or empty component. `None`
/// for a path outside the root, or the root itself — never territory.
///
/// Lexical only: a `..` through a symlink is resolved as text, as git does.
/// Borrows when `path` is already normal, which is every path a walk hands in.
pub fn normalize_rel<'a>(root: &Path, path: &'a Path) -> Option<Cow<'a, [u8]>> {
    let rel = if path.is_absolute() {
        path.strip_prefix(root).ok()?
    } else {
        path
    };
    let bytes = rel.as_os_str().as_bytes();
    if is_normal(bytes) {
        return Some(Cow::Borrowed(bytes));
    }
    let mut parts: Vec<&[u8]> = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => parts.push(s.as_bytes()),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(Cow::Owned(parts.join(&b'/')))
}

/// The directory holding the index db files: `<home>/codegen`, beside
/// `cache/` and never under it, so nothing that prunes caches reaches it.
pub fn codegen_dir(home: &Path) -> PathBuf {
    home.join("codegen")
}

/// This checkout's db file. A home inside the root moves and copies with its
/// checkout, so it keeps the plain `index.db`; a home outside the root can
/// serve several checkouts, so its file is keyed by the canonical root.
pub fn db_path(root: &Path, home: &Path) -> PathBuf {
    let canon_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let abs_home = if home.is_absolute() {
        home.to_path_buf()
    } else {
        root.join(home)
    };
    let canon_home = canonicalize_existing_prefix(&abs_home);
    let dir = codegen_dir(&abs_home);
    if canon_home.starts_with(&canon_root) {
        dir.join("index.db")
    } else {
        let key = xxh3_64(canon_root.as_os_str().as_bytes());
        dir.join(format!("index-{key:016x}.db"))
    }
}

/// Canonicalize the longest existing prefix of `p` and append the rest, so a
/// home that does not exist yet still compares against the canonical root.
fn canonicalize_existing_prefix(p: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut cur = p;
    loop {
        if let Ok(c) = std::fs::canonicalize(cur) {
            let mut out = c;
            for r in rest.iter().rev() {
                out.push(r);
            }
            return out;
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                cur = parent;
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// The lock files under `<home>/lock/codegen/`. The engine takes them with
/// `hlock` (an async, cancellable `flock`) and hands the guard to the index
/// wrapped in a [`super::TargetLockGuard`] or [`super::ImportLockGuard`].
#[derive(Debug, Clone)]
pub struct CodegenLockPaths {
    dir: PathBuf,
}

impl CodegenLockPaths {
    pub fn new(home: &Path) -> Self {
        Self {
            dir: home.join("lock").join("codegen"),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// One target's codegen lock: held for one write-back (steps 2–4) or one
    /// command's handling of that target's rows. Named by a hash of the addr,
    /// so it never collides with [`Self::section`] or [`Self::import`].
    pub fn target(&self, addr: &str) -> PathBuf {
        self.dir.join(format!("{:016x}", xxh3_64(addr.as_bytes())))
    }

    /// The section lock: one read, change and write of `.gitignore`.
    pub fn section(&self) -> PathBuf {
        self.dir.join("section")
    }

    /// The import lock: one import or one db creation. Outermost.
    pub fn import(&self) -> PathBuf {
        self.dir.join("import")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(p: &str) -> Option<String> {
        normalize_rel(Path::new("/w"), Path::new(p))
            .map(|c| String::from_utf8(c.into_owned()).unwrap())
    }

    #[test]
    fn non_normalised_paths_are_normalised_before_the_probe() {
        assert_eq!(norm("./a").as_deref(), Some("a"));
        assert_eq!(norm("a//b").as_deref(), Some("a/b"));
        assert_eq!(norm("a/../b").as_deref(), Some("b"));
        assert_eq!(norm("/w/a/./b").as_deref(), Some("a/b"));
        assert_eq!(norm("a/b").as_deref(), Some("a/b"));
        assert!(matches!(
            normalize_rel(Path::new("/w"), Path::new("a/b")),
            Some(Cow::Borrowed(_))
        ));
    }

    #[test]
    fn paths_outside_the_root_are_none() {
        assert_eq!(norm("../a"), None);
        assert_eq!(norm("/elsewhere/a"), None);
        assert_eq!(norm("/w"), None);
        assert_eq!(norm("a/../.."), None);
    }

    #[test]
    fn a_home_inside_the_root_keeps_the_plain_name_and_outside_is_keyed() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let inside = db_path(root.path(), &root.path().join(".heph"));
        assert_eq!(inside.file_name().unwrap(), "index.db");
        let rel = db_path(root.path(), Path::new(".heph"));
        assert_eq!(rel.file_name().unwrap(), "index.db");
        let outside = db_path(root.path(), other.path());
        let name = outside.file_name().unwrap().to_str().unwrap().to_string();
        assert!(
            name.starts_with("index-") && name.ends_with(".db"),
            "{name}"
        );
        let second = tempfile::tempdir().unwrap();
        assert_ne!(outside, db_path(second.path(), other.path()));
    }

    #[test]
    fn target_lock_names_never_collide_with_the_fixed_locks() {
        let l = CodegenLockPaths::new(Path::new("/h"));
        assert_ne!(l.target("section"), l.section());
        assert_ne!(l.target("import"), l.import());
        assert_ne!(l.target("//p:a"), l.target("//p:ab"));
    }
}
