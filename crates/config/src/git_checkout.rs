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
//! caller gets the reason ([`NoCheckout`]), logs it, and carries on as if there
//! were no checkout. A misread `.git` must never be why a build cannot start.
//!
//! The nearest `.git` wins, even a dangling symlink: a workspace nested in an
//! outer repository whose own `.git` is broken reports that, rather than
//! silently reading the outer repository.

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
    /// submodule, a worktree of a submodule, or a worktree of a bare
    /// repository. The string says which.
    Unshared(&'static str),
}

/// Why a root is in no checkout heph can read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoCheckout {
    pub reason: String,
    /// The `.git` (or the `commondir` it leads to) exists but could not be
    /// followed: malformed, non-UTF-8, empty, or dangling. Worth a warning — a
    /// checkout is there, heph just cannot read it — where "no `.git` at all"
    /// is ordinary.
    pub malformed: bool,
}

impl NoCheckout {
    fn malformed(reason: String) -> Self {
        Self {
            reason,
            malformed: true,
        }
    }
}

impl std::fmt::Display for NoCheckout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl GitCheckout {
    /// Find the checkout `root` is in. `Err` is the reason there is none (no
    /// `.git` above `root`, or a `.git` file that could not be followed).
    ///
    /// The nearest `.git` is found by `symlink_metadata`, so a dangling `.git`
    /// symlink is *this* tree's `.git` (and a malformed one), not a reason to
    /// keep walking up into an outer repository.
    pub fn discover(root: &Path) -> Result<Self, NoCheckout> {
        let (top, dot_git) = root
            .ancestors()
            .map(|dir| (dir, dir.join(".git")))
            .find(|(_, p)| p.symlink_metadata().is_ok())
            .ok_or_else(|| NoCheckout {
                reason: format!("no .git at or above {}", root.display()),
                malformed: false,
            })?;
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
            .map_err(|why| NoCheckout::malformed(format!("{}: {why}", dot_git.display())))?;
        if !git_dir.is_dir() {
            return Err(NoCheckout::malformed(format!(
                "{} points at {}, which is not a directory",
                dot_git.display(),
                git_dir.display()
            )));
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
            .map_err(|why| NoCheckout::malformed(format!("{}: {why}", commondir_file.display())))?;
        if !common_dir.is_dir() {
            return Err(NoCheckout::malformed(format!(
                "{} points at {}, which is not a directory",
                commondir_file.display(),
                common_dir.display()
            )));
        }
        let main_top = match (common_dir.file_name(), common_dir.parent()) {
            (Some(n), Some(parent)) if n == ".git" => parent.to_path_buf(),
            // A submodule's repository lives at `<super>/.git/modules/<name>`
            // (nested: `…/modules/<a>/modules/<b>`), so a worktree of one has
            // a common dir that is not named `.git` either — but it is not
            // bare, and saying so would send someone looking for a bare repo.
            _ if is_submodule_git_dir(&common_dir) => {
                return Ok(Self {
                    top,
                    git_dir,
                    kind: CheckoutKind::Unshared("worktree of a submodule"),
                });
            }
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

/// Whether `dir` is a submodule's git dir: under a `modules` directory of
/// some `.git`.
fn is_submodule_git_dir(dir: &Path) -> bool {
    dir.ancestors().any(|a| {
        a.file_name().is_some_and(|n| n == "modules")
            && a.parent()
                .and_then(Path::file_name)
                .is_some_and(|n| n == ".git")
    })
}

/// The linked worktrees registered in a repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LinkedWorktrees {
    /// Admin dirs under `<common_dir>/worktrees/`, stale ones included: a
    /// worktree deleted without `git worktree prune` still counts. The count
    /// only ever makes heph more careful.
    pub registered: usize,
    /// Of those, the ones whose working tree no longer exists (their
    /// `gitdir` file names a `.git` that is gone). `git worktree prune`
    /// removes them.
    pub missing: usize,
}

/// The linked worktrees registered in the repository at `common_dir`: the
/// directories under `<common_dir>/worktrees/`. Anything else there (a stray
/// file) is not a worktree.
pub fn linked_worktrees(common_dir: &Path) -> LinkedWorktrees {
    let Ok(rd) = std::fs::read_dir(common_dir.join("worktrees")) else {
        return LinkedWorktrees::default();
    };
    rd.filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .fold(LinkedWorktrees::default(), |mut acc, e| {
            acc.registered += 1;
            // Only a `gitdir` that reads and names a path that is gone is
            // missing; an unreadable one is not evidence either way.
            let gone = read_pointer(&e.path().join("gitdir"), "")
                .is_ok_and(|p| e.path().join(p).symlink_metadata().is_err());
            if gone {
                acc.missing += 1;
            }
            acc
        })
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

    /// A main checkout at `main` (a `.git` directory with `HEAD` on `branch`)
    /// that is a heph workspace: an empty `.hephconfig` at its top, which a
    /// linked worktree reads its shared home from.
    pub fn main_checkout(main: &Path, branch: &str) -> PathBuf {
        let git = main.join(".git");
        std::fs::create_dir_all(&git).expect("mkdir .git");
        std::fs::write(git.join("HEAD"), format!("ref: refs/heads/{branch}\n")).expect("HEAD");
        std::fs::write(main.join(crate::CONFIG_FILE_NAME), "").expect(".hephconfig");
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
        assert_eq!(
            linked_worktrees(&main.join(".git")),
            LinkedWorktrees {
                registered: 1,
                missing: 0
            }
        );
    }

    #[test]
    fn linked_worktrees_empty_dir_is_zero() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git = main_checkout(tmp.path(), "master");
        assert_eq!(linked_worktrees(&git), LinkedWorktrees::default());
        std::fs::create_dir_all(git.join("worktrees")).expect("mkdir");
        assert_eq!(linked_worktrees(&git), LinkedWorktrees::default());
    }

    #[test]
    fn linked_worktrees_ignores_a_stray_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let main = tmp.path().join("main");
        let git = main_checkout(&main, "master");
        linked_worktree(&main, &tmp.path().join("wt"), "wt", "feat");
        std::fs::write(git.join("worktrees").join("stray"), "x").expect("write");
        assert_eq!(linked_worktrees(&git).registered, 1);
    }

    /// A worktree deleted without `git worktree prune` still counts as
    /// registered, and is reported as missing.
    #[test]
    fn linked_worktrees_counts_a_stale_admin_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let main = tmp.path().join("main");
        let git = main_checkout(&main, "master");
        linked_worktree(&main, &tmp.path().join("alive"), "alive", "a");
        let gone = tmp.path().join("gone");
        linked_worktree(&main, &gone, "gone", "b");
        std::fs::remove_dir_all(&gone).expect("rm worktree");
        assert_eq!(
            linked_worktrees(&git),
            LinkedWorktrees {
                registered: 2,
                missing: 1
            }
        );
    }

    /// A worktree of a submodule has a common dir under `.git/modules/`, not
    /// named `.git`. It is not a bare repository and must not be called one.
    #[test]
    fn a_worktree_of_a_submodule_is_not_called_bare() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sup = tmp.path().join("super");
        main_checkout(&sup, "master");
        let modules = sup.join(".git").join("modules").join("x");
        let admin = modules.join("worktrees").join("w");
        std::fs::create_dir_all(&admin).expect("mkdir");
        std::fs::write(admin.join("commondir"), "../..\n").expect("commondir");
        std::fs::write(admin.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
        let wt = tmp.path().join("w");
        std::fs::create_dir_all(&wt).expect("mkdir");
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).expect(".git");
        let co = GitCheckout::discover(&wt).expect("discover");
        assert_eq!(co.kind, CheckoutKind::Unshared("worktree of a submodule"));
    }

    /// A dangling `.git` symlink is this tree's `.git`, broken — not a reason
    /// to read the repository the workspace happens to sit inside.
    #[cfg(unix)]
    #[test]
    fn a_dangling_dot_git_symlink_does_not_reach_the_outer_repo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let outer = tmp.path().join("outer");
        main_checkout(&outer, "outer-branch");
        let inner = outer.join("inner");
        std::fs::create_dir_all(&inner).expect("mkdir");
        std::os::unix::fs::symlink(tmp.path().join("nowhere"), inner.join(".git"))
            .expect("symlink");
        let err = GitCheckout::discover(&inner).expect_err("dangling .git");
        assert!(err.malformed, "{err:?}");
        assert!(err.reason.contains("reading"), "{err:?}");
    }
}
