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
            make_dirs_read_write(dir);
            fs::remove_dir_all(dir)
        }
        other => other,
    }
}

/// Recursively make every directory under `dir` writable (`0777`) so its
/// contents can be removed. Errors walking the tree are ignored — this is
/// a best-effort prelude to a removal retry, mirroring Go's helper.
#[cfg(unix)]
fn make_dirs_read_write(dir: &Path) {
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
fn make_dirs_read_write(_dir: &Path) {}
