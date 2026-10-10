//! Reading the git checkout a workspace lives in — from files, never the `git`
//! binary.
//!
//! The one place heph looks at `.git`. Two things need it: the engine's home
//! resolver (`Homes::resolve`), which shares the main checkout's home from a
//! linked worktree, and `${git:branch}`, which reads the checkout's `HEAD`. Both
//! must agree on what "the checkout" is, so neither reads `.git` itself. Lives
//! next to [`get_root`](crate::get_root): both locate the workspace on disk, and
//! neither needs the engine.
//!
//! Layouts, from the first `.git` at or above the workspace root:
//!
//! - **directory** — a main checkout. Its git dir and common dir are that `.git`.
//! - **file** `gitdir: <path>` — the path is the checkout's git dir (relative to
//!   the file's directory, or absolute). With a `commondir` file in it, this is a
//!   linked worktree whose shared repository is that common dir (relative to the
//!   git dir, or absolute); the main checkout is the common dir's parent when the
//!   common dir is named `.git`, and there is none when it is not (a bare
//!   repository). Without `commondir` it is a submodule, which has no main
//!   checkout to share with.
//!
//! Anything unreadable, malformed, non-UTF-8 or dangling is not an error: the
//! caller gets the reason, logs it, and carries on as if there were no checkout.
//! A misread `.git` must never be why a build cannot start.

use std::path::{Component, Path, PathBuf};

/// The checkout a workspace root belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCheckout {
    /// The checkout's working-tree top: the directory holding `.git`.
    pub top: PathBuf,
    /// This checkout's own git dir — where its `HEAD` is.
    pub git_dir: PathBuf,
    pub kind: CheckoutKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutKind {
    /// `.git` is a directory. `common_dir` is that directory.
    Main { common_dir: PathBuf },
    /// A linked worktree of the repository at `common_dir`, whose main
    /// checkout's top is `main_top`. `name` is the worktree's admin dir name
    /// (`<common_dir>/worktrees/<name>`).
    Linked {
        common_dir: PathBuf,
        main_top: PathBuf,
        name: String,
    },
    /// A checkout with a git dir but no main checkout to share with: a
    /// submodule, or a worktree of a bare repository. The string says which.
    Unshared(&'static str),
}

impl GitCheckout {
    /// Find the checkout `root` is in. `Err` is the reason there is none (no
    /// `.git` above `root`, or a `.git` file that could not be followed).
    pub fn discover(root: &Path) -> Result<Self, String> {
        let (top, dot_git) = root
            .ancestors()
            .map(|dir| (dir, dir.join(".git")))
            .find(|(_, p)| p.exists())
            .ok_or_else(|| format!("no .git at or above {}", root.display()))?;
        let top = top.to_path_buf();

        if dot_git.is_dir() {
            return Ok(Self {
                top,
                git_dir: dot_git.clone(),
                kind: CheckoutKind::Main {
                    common_dir: dot_git,
                },
            });
        }

        let git_dir = read_pointer(&dot_git, "gitdir:")
            .map(|p| normalize(&top.join(p)))
            .map_err(|why| format!("{}: {why}", dot_git.display()))?;
        if !git_dir.is_dir() {
            return Err(format!(
                "{} points at {}, which is not a directory",
                dot_git.display(),
                git_dir.display()
            ));
        }

        let commondir_file = git_dir.join("commondir");
        if !commondir_file.exists() {
            return Ok(Self {
                top,
                git_dir,
                kind: CheckoutKind::Unshared("submodule (no commondir)"),
            });
        }
        let common_dir = read_pointer(&commondir_file, "")
            .map(|p| normalize(&git_dir.join(p)))
            .map_err(|why| format!("{}: {why}", commondir_file.display()))?;
        if !common_dir.is_dir() {
            return Err(format!(
                "{} points at {}, which is not a directory",
                commondir_file.display(),
                common_dir.display()
            ));
        }
        let main_top = match (common_dir.file_name(), common_dir.parent()) {
            (Some(n), Some(parent)) if n == ".git" => parent.to_path_buf(),
            _ => {
                return Ok(Self {
                    top,
                    git_dir,
                    kind: CheckoutKind::Unshared("worktree of a bare repository"),
                });
            }
        };
        let name = git_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(Self {
            top,
            git_dir,
            kind: CheckoutKind::Linked {
                common_dir,
                main_top,
                name,
            },
        })
    }

    /// The current branch name, or `None` on a detached HEAD or an unreadable
    /// one.
    pub fn branch(&self) -> Option<String> {
        let head = std::fs::read_to_string(self.git_dir.join("HEAD")).ok()?;
        // `ref: refs/heads/<branch>` on a branch; a bare sha when detached.
        let branch = head.trim().strip_prefix("ref: refs/heads/")?;
        (!branch.is_empty()).then(|| branch.to_string())
    }
}

/// Linked worktrees registered in the repository at `common_dir`: the entries
/// of `<common_dir>/worktrees/`. A worktree deleted without `git worktree
/// prune` still counts — the count only ever makes heph more careful.
pub fn linked_worktree_count(common_dir: &Path) -> usize {
    std::fs::read_dir(common_dir.join("worktrees"))
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .count()
        })
        .unwrap_or(0)
}

/// Read a one-line pointer file: `prefix` (if any) then a path. Whitespace and
/// a trailing CRLF are trimmed. The path is returned as written — relative or
/// absolute — for the caller to anchor.
fn read_pointer(file: &Path, prefix: &str) -> Result<PathBuf, String> {
    let bytes = std::fs::read(file).map_err(|e| format!("reading: {e}"))?;
    let text = std::str::from_utf8(&bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let rest = text
        .trim()
        .strip_prefix(prefix)
        .ok_or_else(|| format!("no `{prefix}` line"))?
        .trim();
    if rest.is_empty() {
        return Err("empty path".to_string());
    }
    Ok(PathBuf::from(rest))
}

/// Fold `.` and `..` out of `p` lexically. git writes `commondir` as `../..`
/// relative to `<common>/worktrees/<name>`; folding it is what lets the common
/// dir's name be checked and its parent taken. Lexical rather than
/// `canonicalize` so a symlinked path the user chose survives as written.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(c);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Hand-built git layouts for tests — no `git` binary.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_layout {
    use std::path::{Path, PathBuf};

    /// A main checkout at `main` (a `.git` directory with `HEAD` on `branch`).
    pub fn main_checkout(main: &Path, branch: &str) -> PathBuf {
        let git = main.join(".git");
        std::fs::create_dir_all(&git).expect("mkdir .git");
        std::fs::write(git.join("HEAD"), format!("ref: refs/heads/{branch}\n")).expect("HEAD");
        git
    }

    /// A linked worktree of `main` at `wt`, registered as `name`, on `branch` —
    /// the files `git worktree add` writes, with git's relative `commondir`.
    pub fn linked_worktree(main: &Path, wt: &Path, name: &str, branch: &str) {
        let admin = main.join(".git").join("worktrees").join(name);
        std::fs::create_dir_all(&admin).expect("mkdir admin dir");
        std::fs::create_dir_all(wt).expect("mkdir worktree");
        std::fs::write(admin.join("commondir"), "../..\n").expect("commondir");
        std::fs::write(
            admin.join("gitdir"),
            format!("{}\n", wt.join(".git").display()),
        )
        .expect("gitdir");
        std::fs::write(admin.join("HEAD"), format!("ref: refs/heads/{branch}\n")).expect("HEAD");
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display()))
            .expect(".git file");
    }
}

#[cfg(test)]
mod tests {
    use super::test_layout::*;
    use super::*;

    #[test]
    fn normalize_folds_dots() {
        assert_eq!(
            normalize(Path::new("/r/.git/worktrees/w/../..")),
            Path::new("/r/.git")
        );
        assert_eq!(normalize(Path::new("/a/./b")), Path::new("/a/b"));
    }

    #[test]
    fn a_linked_worktree_names_its_main_checkout() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let main = tmp.path().join("main");
        let wt = tmp.path().join("wt");
        main_checkout(&main, "master");
        linked_worktree(&main, &wt, "wt1", "feat");
        let co = GitCheckout::discover(&wt.join("sub")).expect("discover");
        assert_eq!(co.top, wt);
        assert_eq!(
            co.kind,
            CheckoutKind::Linked {
                common_dir: main.join(".git"),
                main_top: main.clone(),
                name: "wt1".to_string(),
            }
        );
        assert_eq!(linked_worktree_count(&main.join(".git")), 1);
    }
}
