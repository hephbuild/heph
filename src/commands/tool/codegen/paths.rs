//! Path arguments of the codegen commands: cwd-relative, root-relative
//! (`//p/foo.go`, as heph spells a package) or absolute; normalised
//! lexically; a path outside the workspace is refused.

use std::path::{Path, PathBuf};

use hwalk::codegen_index::paths::rel_path;

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

/// "did you mean //p/x?": for a bare relative `arg` (the spelling `status`
/// prints) typed in a subdirectory, when it names nothing from `cwd` but an
/// existing path from the root. `None` otherwise, and for `//` and absolute
/// arguments, which mean one path whatever the cwd.
pub fn root_hint(root: &Path, cwd: &Path, arg: &str) -> Option<String> {
    if arg.starts_with("//") || Path::new(arg).is_absolute() {
        return None;
    }
    let ResolvedPath::Inside(here) = resolve_path(root, cwd, arg) else {
        return None;
    };
    let ResolvedPath::Inside(there) = resolve_path(root, root, arg) else {
        return None;
    };
    let exists = |rel: &[u8]| root.join(rel_path(rel)).symlink_metadata().is_ok();
    (here != there && !exists(&here) && exists(&there))
        .then(|| format!("did you mean //{}?", String::from_utf8_lossy(&there)))
}

/// `reason`, followed by `hint` when there is one.
pub fn with_hint(reason: &str, hint: Option<&str>) -> String {
    match hint {
        Some(h) => format!("{reason}; {h}"),
        None => reason.to_owned(),
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

    #[test]
    fn root_hint_only_for_a_bare_path_missing_here_and_present_at_the_root() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        let sub = root.join("sub");
        std::fs::create_dir_all(root.join("p"))?;
        std::fs::create_dir_all(sub.join("p"))?;
        std::fs::write(root.join("p/x.go"), "")?;
        std::fs::write(root.join("p/both.go"), "")?;
        std::fs::write(sub.join("p/both.go"), "")?;
        assert_eq!(
            root_hint(root, &sub, "p/x.go").as_deref(),
            Some("did you mean //p/x.go?")
        );
        // Exists from the cwd: that is the path meant.
        assert_eq!(root_hint(root, &sub, "p/both.go"), None);
        // Missing from the root too: nothing to suggest.
        assert_eq!(root_hint(root, &sub, "p/none.go"), None);
        // At the root, the two resolutions are one path.
        assert_eq!(root_hint(root, root, "p/none.go"), None);
        // `//` and absolute spellings do not depend on the cwd.
        assert_eq!(root_hint(root, &sub, "//sub/p/x.go"), None);
        let abs = sub.join("p/x.go");
        assert_eq!(root_hint(root, &sub, &abs.to_string_lossy()), None);
        Ok(())
    }
}
