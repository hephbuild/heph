//! Path arguments of the codegen commands: cwd-relative, root-relative
//! (`//p/foo.go`, as heph spells a package) or absolute; normalised
//! lexically; a path outside the workspace is refused.

use std::path::{Path, PathBuf};

/// One path argument, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedPath {
    /// Root-relative, normalised (`/`-separated, no `.`, `..` or empty part).
    Inside(Vec<u8>),
    /// Outside the workspace, or the root itself: refused.
    Outside,
}

/// Resolve `arg` against `root` and `cwd` (both absolute).
pub fn resolve_path(root: &Path, cwd: &Path, arg: &str) -> ResolvedPath {
    let abs: PathBuf = if let Some(rest) = arg.strip_prefix("//") {
        root.join(rest)
    } else if Path::new(arg).is_absolute() {
        PathBuf::from(arg)
    } else {
        cwd.join(arg)
    };
    // `normalize_rel` strips `root` before resolving `..`, so normalise the
    // whole path first: `<root>/a/../../x` must not read as inside.
    let abs = lexical(&abs);
    match hwalk::codegen_index::normalize_rel(root, &abs) {
        Some(rel) => ResolvedPath::Inside(rel.into_owned()),
        None => ResolvedPath::Outside,
    }
}

/// `p` with `.` and `..` resolved as text (as git does).
fn lexical(p: &Path) -> PathBuf {
    use std::path::Component;

    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inside(s: &str) -> ResolvedPath {
        ResolvedPath::Inside(s.as_bytes().to_vec())
    }

    #[test]
    fn resolves_every_argument_form() {
        let root = Path::new("/w");
        let cwd = Path::new("/w/sub");
        assert_eq!(resolve_path(root, cwd, "a.go"), inside("sub/a.go"));
        assert_eq!(resolve_path(root, cwd, "//p/a.go"), inside("p/a.go"));
        assert_eq!(resolve_path(root, cwd, "/w/p/a.go"), inside("p/a.go"));
        assert_eq!(resolve_path(root, cwd, "../p/./a.go"), inside("p/a.go"));
        assert_eq!(
            resolve_path(root, cwd, "x//y/../a.go"),
            inside("sub/x/a.go")
        );
    }

    #[test]
    fn refuses_paths_outside_the_workspace() {
        let root = Path::new("/w");
        let cwd = Path::new("/w/sub");
        assert_eq!(
            resolve_path(root, cwd, "../../etc/passwd"),
            ResolvedPath::Outside
        );
        assert_eq!(
            resolve_path(root, cwd, "/etc/passwd"),
            ResolvedPath::Outside
        );
        assert_eq!(resolve_path(root, cwd, "//../x"), ResolvedPath::Outside);
        assert_eq!(resolve_path(root, cwd, ".."), ResolvedPath::Outside);
        // A sibling whose name starts with the root's is not inside it.
        assert_eq!(resolve_path(root, cwd, "/wx/a"), ResolvedPath::Outside);
    }
}
