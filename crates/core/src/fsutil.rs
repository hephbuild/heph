//! Small filesystem helpers shared across crates.

use std::fs;
use std::io;
use std::path::Path;

/// `remove_dir_all` that recovers from `PermissionDenied`. Read-only dirs
/// (e.g. a 0555 codegen output tree) make the kernel refuse to unlink their
/// children until the dir is writable. On the first permission failure we
/// recursively `chmod 0777` every directory under `dir` and retry the removal
/// once.
///
/// Borrowed from the Go toolchain's `modfetch.MakeDirsReadWrite`:
/// https://github.com/golang/go/blob/3c72dd513c30df60c0624360e98a77c4ae7ca7c8/src/cmd/go/internal/modfetch/fetch.go
pub fn remove_dir_all(dir: &Path) -> io::Result<()> {
    match fs::remove_dir_all(dir) {
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            make_readwrite_tree(dir);
            fs::remove_dir_all(dir)
        }
        other => other,
    }
}

/// Recursively make every directory under `dir` writable (`0777`) so its
/// contents can be removed. Errors walking the tree are ignored — this is
/// a best-effort prelude to a removal retry, mirroring Go's helper.
///
/// Inverse of [`make_readonly_tree`]: that one strips write bits to publish a
/// read-only shared tree (à la the Go module cache); this one restores them so
/// the tree can be deleted. Kept side by side so the two halves of the
/// read-only lifecycle stay in sync.
#[cfg(unix)]
pub fn make_readwrite_tree(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;

    fn walk(path: &Path) {
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(_) => return,
        };
        if !meta.is_dir() {
            return;
        }
        drop(fs::set_permissions(path, fs::Permissions::from_mode(0o777)));
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            walk(&entry.path());
        }
    }

    walk(dir);
}

#[cfg(not(unix))]
pub fn make_readwrite_tree(_dir: &Path) {}

/// Recursively strip write bits from every file and directory under `root`
/// (dirs become `0o555`, files keep their mode minus `0o222`), publishing a
/// read-only tree the way the Go module cache marks downloaded modules
/// read-only. Children are processed before their parents so the traversal
/// permission needed to descend is never lost. Symlinks are left untouched
/// (`set_permissions` would follow them; the target's own mode governs).
///
/// Inverse of [`make_readwrite_tree`]; the two live together so a change to
/// one prompts a matching change to the other.
#[cfg(unix)]
pub fn make_readonly_tree(root: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fn walk(path: &Path) -> io::Result<()> {
        let md = fs::symlink_metadata(path)?;
        let ft = md.file_type();
        if ft.is_symlink() {
            return Ok(());
        }
        if ft.is_dir() {
            for entry in fs::read_dir(path)? {
                walk(&entry?.path())?;
            }
            fs::set_permissions(path, fs::Permissions::from_mode(0o555))?;
        } else {
            let mode = md.permissions().mode() & !0o222;
            fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
        }
        Ok(())
    }

    walk(root)
}

#[cfg(not(unix))]
pub fn make_readonly_tree(_root: &Path) -> io::Result<()> {
    Ok(())
}
