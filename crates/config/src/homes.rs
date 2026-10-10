//! The workspace's two heph homes, resolved together by [`Homes::resolve`] —
//! the one public resolver. Each home is built by [`HomeDir::resolve`] against
//! its *own* root, so the "not the root or above it" guard holds for both: the
//! shared home against the main checkout's root, the checkout home against
//! this checkout's.
//!
//! ## Two homes: shared, and the checkout's own
//!
//! In a linked git worktree the **shared** home is the main checkout's, so every
//! worktree of a repository hits one cache. The **checkout** home is this
//! checkout's own `<root>/<homeDir>`; it holds what must not be shared between
//! two working trees — sandboxes (and their FUSE mount and execute lock), the
//! filesystem-walk cache, and approval notices. Outside a linked worktree the
//! two are the same directory. See `docs/HOME_DIR.md`.

use std::fmt;
use std::ops::Deref;
use std::path::{Path, PathBuf};

use crate::git_checkout::{CheckoutKind, GitCheckout, linked_worktree_count};
use crate::home_dir::{DEFAULT_HOME_DIR, HomeDir};

/// This checkout's own home — a type of its own, not a [`HomeDir`], so a site
/// that needs one cannot be handed the other: everything that belongs to one
/// working tree (sandboxes, their FUSE mount and execute lock, the fswalk cache,
/// approval notices) takes a `CheckoutHome`, everything else the shared
/// [`HomeDir`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CheckoutHome(PathBuf);

impl CheckoutHome {
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl Deref for CheckoutHome {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for CheckoutHome {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// Whether, and with whom, the shared home is shared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HomeSharing {
    /// This checkout uses its own home and nobody else's; the string says why
    /// (not a git checkout, `shareHome: false`, an absolute `homeDir`, a
    /// fallback …).
    Own(String),
    /// A linked worktree using the main checkout's home.
    Linked {
        /// The heph root in the main checkout this worktree's root maps to.
        main_root: PathBuf,
        /// The worktree's name (`<common dir>/worktrees/<name>`).
        name: String,
        /// Linked worktrees of the repository, this one included.
        linked_worktrees: usize,
    },
    /// The main checkout of a repository that has linked worktrees, which may
    /// use this home.
    Main { linked_worktrees: usize },
}

impl HomeSharing {
    /// Whether another checkout may use the shared home — so a target that does
    /// not resolve *here* may still be someone else's.
    pub fn is_shared(&self) -> bool {
        match self {
            HomeSharing::Own(_) => false,
            HomeSharing::Linked { .. } => true,
            HomeSharing::Main { linked_worktrees } => *linked_worktrees > 0,
        }
    }
}

impl fmt::Display for HomeSharing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HomeSharing::Own(why) => write!(f, "own home ({why})"),
            HomeSharing::Linked {
                main_root,
                name,
                linked_worktrees,
            } => write!(
                f,
                "shared from {} (linked worktree {name}; {linked_worktrees} linked worktree(s))",
                main_root.display()
            ),
            HomeSharing::Main { linked_worktrees } => write!(
                f,
                "main checkout, shared with {linked_worktrees} linked worktree(s)"
            ),
        }
    }
}

/// The resolved homes of a workspace. See the module doc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Homes {
    shared: HomeDir,
    checkout: CheckoutHome,
    sharing: HomeSharing,
}

impl Homes {
    /// Resolve the homes of the workspace at `root`.
    ///
    /// `configured` is the config file's `homeDir`, resolved by
    /// [`HomeDir::resolve`] (which refuses a home at or above its root). An
    /// absolute one is used as written for both homes, with no worktree
    /// detection; `None` means [`DEFAULT_HOME_DIR`].
    ///
    /// `share_home` is `worktree.shareHome`. When on and `root` is in a linked
    /// git worktree whose root maps to an existing directory in the main
    /// checkout, the shared home is that directory's `homeDir`; otherwise it is
    /// the checkout's own. Detection never fails the resolve: every reason it
    /// gives up is logged at `debug` and lands in [`Homes::sharing`].
    pub fn resolve(
        root: &Path,
        configured: Option<&Path>,
        share_home: bool,
    ) -> anyhow::Result<Self> {
        let own = HomeDir::resolve(root, configured)?;
        let rel = configured.unwrap_or(Path::new(DEFAULT_HOME_DIR));

        let sharing = if rel.is_absolute() {
            HomeSharing::Own("homeDir is absolute".to_string())
        } else if !share_home {
            HomeSharing::Own("worktree.shareHome is false".to_string())
        } else {
            detect_sharing(root, rel)
        };
        let shared = match &sharing {
            // Against the main checkout's root, so the same guard holds there.
            HomeSharing::Linked { main_root, .. } => HomeDir::resolve(main_root, configured)?,
            HomeSharing::Own(_) | HomeSharing::Main { .. } => own.clone(),
        };
        let checkout = CheckoutHome(own.as_path().to_path_buf());
        tracing::debug!(
            shared = %shared.display(),
            checkout = %checkout.display(),
            "heph home: {sharing}"
        );
        Ok(Self {
            shared,
            checkout,
            sharing,
        })
    }

    /// The home every checkout of the repository shares: the cache, blobs,
    /// gateway and revision locks, staged inputs, credentials, scratch, diag.
    pub fn shared(&self) -> &HomeDir {
        &self.shared
    }

    /// This checkout's own home: sandboxes, the execute lock, the
    /// filesystem-walk cache, approval notices. The same directory as
    /// [`Homes::shared`] outside a linked worktree.
    pub fn checkout(&self) -> &CheckoutHome {
        &self.checkout
    }

    pub fn sharing(&self) -> &HomeSharing {
        &self.sharing
    }

    /// The default homes of a workspace rooted at `root` (a test's tempdir),
    /// with worktree detection on — what a config without `homeDir` resolves.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn for_tests(root: &Path) -> Self {
        Self::resolve(root, None, true).expect("resolving test homes")
    }
}

/// Decide whether the checkout at `root` shares a home; `home_rel` is the
/// (relative) `homeDir`.
///
/// On canonical paths: a root reached through a symlink must find the `.git`
/// of the tree it really is in, and "is the main checkout's root this one"
/// must compare real directories, not spellings of them (`/var` is
/// `/private/var` on macOS).
fn detect_sharing(root: &Path, home_rel: &Path) -> HomeSharing {
    let root = match root.canonicalize() {
        Ok(p) => p,
        Err(e) => return HomeSharing::Own(format!("canonicalizing {}: {e}", root.display())),
    };
    let root = root.as_path();
    let co = match GitCheckout::discover(root) {
        Ok(co) => co,
        Err(why) => return HomeSharing::Own(why),
    };
    match co.kind {
        CheckoutKind::Main { common_dir } => match linked_worktree_count(&common_dir) {
            0 => HomeSharing::Own("main checkout without linked worktrees".to_string()),
            n => HomeSharing::Main {
                linked_worktrees: n,
            },
        },
        CheckoutKind::Unshared(why) => HomeSharing::Own(why.to_string()),
        CheckoutKind::Linked {
            common_dir,
            main_top,
            name,
        } => {
            let Ok(rel) = root.strip_prefix(&co.top) else {
                return HomeSharing::Own(format!(
                    "root {} is not under worktree {}",
                    root.display(),
                    co.top.display()
                ));
            };
            let mapped = main_top.join(rel);
            let main_root = match mapped.canonicalize() {
                Ok(p) if p.is_dir() => p,
                _ => {
                    return HomeSharing::Own(format!(
                        "{} does not exist in the main checkout",
                        mapped.display()
                    ));
                }
            };
            if main_root == root {
                // A `gitdir` pointing back at this very tree: there is no
                // other checkout to share with.
                return HomeSharing::Own(format!(
                    "the main checkout's root is this root ({})",
                    root.display()
                ));
            }
            let would_share = HomeDir::resolve(&main_root, Some(home_rel));
            if would_share.is_ok_and(|h| root.starts_with(h.as_path())) {
                // A worktree placed inside the home it would share: the home
                // would hold its own sources, and walkers refuse anything
                // under a `.heph*` dir.
                return HomeSharing::Own(format!(
                    "{} is inside the main checkout's home",
                    root.display()
                ));
            }
            HomeSharing::Linked {
                main_root,
                name,
                linked_worktrees: linked_worktree_count(&common_dir),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_checkout::test_layout::{linked_worktree, main_checkout};

    fn resolve(root: &Path, configured: Option<&str>) -> Homes {
        Homes::resolve(root, configured.map(Path::new), true).expect("resolve")
    }

    /// `<tmp>/main` with a linked worktree `<tmp>/wt` named `wt`. The base is
    /// canonical: the shared home is, so expectations must be too.
    fn repo_with_worktree() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize");
        let main = base.join("main");
        let wt = base.join("wt");
        main_checkout(&main, "master");
        linked_worktree(&main, &wt, "wt", "feat");
        (tmp, main, wt)
    }

    fn assert_own(h: &Homes, root: &Path) {
        assert!(
            matches!(h.sharing(), HomeSharing::Own(_)),
            "{:?}",
            h.sharing()
        );
        assert_eq!(h.shared().as_path(), root.join(DEFAULT_HOME_DIR));
        assert_eq!(h.checkout().as_path(), h.shared().as_path());
    }

    /// Outside a git checkout both homes are the one `HomeDir::resolve` gives
    /// (its own tests cover the joining and normalizing).
    #[test]
    fn outside_a_checkout_both_homes_are_the_default() {
        let home = resolve(Path::new("/repo"), None);
        assert_eq!(home.shared().as_path(), Path::new("/repo/.heph"));
        assert_eq!(home.checkout().as_path(), Path::new("/repo/.heph"));
    }

    /// The root-or-above guard runs through `Homes::resolve` too.
    #[test]
    fn a_home_at_the_root_is_refused() {
        let err = Homes::resolve(Path::new("/repo"), Some(Path::new(".")), true)
            .expect_err("a home at the root must be refused");
        assert!(
            err.to_string().contains("workspace root or above"),
            "{err:#}"
        );
    }

    #[test]
    fn absolute_is_used_as_written_for_both() {
        let home = resolve(Path::new("/repo"), Some("/var/heph"));
        assert_eq!(home.shared().as_path(), Path::new("/var/heph"));
        assert_eq!(home.checkout().as_path(), Path::new("/var/heph"));
    }

    #[test]
    fn detect_normal_checkout() {
        let tmp = tempfile::tempdir().expect("tempdir");
        main_checkout(tmp.path(), "master");
        assert_own(&resolve(tmp.path(), None), tmp.path());
    }

    #[test]
    fn detect_linked_worktree() {
        let (_tmp, main, wt) = repo_with_worktree();
        let h = resolve(&wt, None);
        assert_eq!(h.shared().as_path(), main.join(DEFAULT_HOME_DIR));
        assert_eq!(h.checkout().as_path(), wt.join(DEFAULT_HOME_DIR));
        assert!(h.sharing().is_shared());
        assert_eq!(
            h.sharing(),
            &HomeSharing::Linked {
                main_root: main.clone(),
                name: "wt".to_string(),
                linked_worktrees: 1,
            }
        );
        // And the main checkout knows its home is shared.
        let m = resolve(&main, None);
        assert_eq!(m.shared().as_path(), m.checkout().as_path());
        assert_eq!(
            m.sharing(),
            &HomeSharing::Main {
                linked_worktrees: 1
            }
        );
        assert!(m.sharing().is_shared());
    }

    #[test]
    fn detect_worktree_subdir_root() {
        let (_tmp, main, wt) = repo_with_worktree();
        std::fs::create_dir_all(main.join("sub")).expect("mkdir");
        std::fs::create_dir_all(wt.join("sub")).expect("mkdir");
        let h = resolve(&wt.join("sub"), None);
        assert_eq!(
            h.shared().as_path(),
            main.join("sub").join(DEFAULT_HOME_DIR)
        );
        assert_eq!(
            h.checkout().as_path(),
            wt.join("sub").join(DEFAULT_HOME_DIR)
        );
    }

    #[test]
    fn detect_submodule_not_worktree() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sup = tmp.path();
        main_checkout(sup, "master");
        let modules = sup.join(".git").join("modules").join("x");
        std::fs::create_dir_all(&modules).expect("mkdir");
        std::fs::write(modules.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
        let sub = sup.join("x");
        std::fs::create_dir_all(&sub).expect("mkdir");
        std::fs::write(sub.join(".git"), "gitdir: ../.git/modules/x\n").expect(".git");
        let h = resolve(&sub, None);
        assert_own(&h, &sub);
        assert_eq!(
            h.sharing(),
            &HomeSharing::Own("submodule (no commondir)".to_string())
        );
    }

    #[test]
    fn detect_bare_repo_main() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bare = tmp.path().join("repo.git");
        let admin = bare.join("worktrees").join("w");
        std::fs::create_dir_all(&admin).expect("mkdir");
        std::fs::write(admin.join("commondir"), "../..\n").expect("commondir");
        std::fs::write(admin.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
        let wt = tmp.path().join("w");
        std::fs::create_dir_all(&wt).expect("mkdir");
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).expect(".git");
        let h = resolve(&wt, None);
        assert_own(&h, &wt);
        assert_eq!(
            h.sharing(),
            &HomeSharing::Own("worktree of a bare repository".to_string())
        );
    }

    #[test]
    fn detect_commondir_relative_and_absolute() {
        let (_tmp, main, wt) = repo_with_worktree();
        // git's own relative form.
        assert_eq!(
            resolve(&wt, None).shared().as_path(),
            main.join(DEFAULT_HOME_DIR)
        );
        // An absolute commondir, and a relative `gitdir:` in the `.git` file.
        let admin = main.join(".git").join("worktrees").join("wt");
        std::fs::write(
            admin.join("commondir"),
            format!("{}\n", main.join(".git").display()),
        )
        .expect("commondir");
        std::fs::write(wt.join(".git"), "gitdir: ../main/.git/worktrees/wt\n").expect(".git");
        assert_eq!(
            resolve(&wt, None).shared().as_path(),
            main.join(DEFAULT_HOME_DIR)
        );
    }

    #[test]
    fn detect_malformed_gitdir() {
        let (_tmp, main, wt) = repo_with_worktree();
        let admin = main.join(".git").join("worktrees").join("wt");
        let dot_git = wt.join(".git");

        // Trailing CRLF and padding still work.
        std::fs::write(&dot_git, format!("gitdir:  {}  \r\n", admin.display())).expect("write");
        assert_eq!(
            resolve(&wt, None).shared().as_path(),
            main.join(DEFAULT_HOME_DIR)
        );

        let cases: [(&str, &[u8]); 5] = [
            ("empty file", b""),
            ("no gitdir prefix", b"/somewhere/else\n"),
            ("empty path", b"gitdir:   \n"),
            ("non-UTF-8", b"gitdir: /tmp/\xff\xfe\n"),
            (
                "dangling",
                b"gitdir: /definitely/not/here/.git/worktrees/x\n",
            ),
        ];
        for (what, bytes) in cases {
            std::fs::write(&dot_git, bytes).expect("write");
            let h = resolve(&wt, None);
            assert!(
                matches!(h.sharing(), HomeSharing::Own(_)),
                "{what}: {:?}",
                h.sharing()
            );
            assert_own(&h, &wt);
        }

        // A dangling or malformed commondir is a fallback too.
        std::fs::write(&dot_git, format!("gitdir: {}\n", admin.display())).expect("write");
        for bytes in [&b""[..], b"/no/such/common\n", b"\xff\n"] {
            std::fs::write(admin.join("commondir"), bytes).expect("write");
            assert_own(&resolve(&wt, None), &wt);
        }
    }

    #[test]
    fn detect_mapped_root_missing() {
        let (_tmp, _main, wt) = repo_with_worktree();
        // `wt/only-here` has no counterpart in the main checkout.
        let root = wt.join("only-here");
        std::fs::create_dir_all(&root).expect("mkdir");
        let h = resolve(&root, None);
        assert_own(&h, &root);
        assert!(
            matches!(h.sharing(), HomeSharing::Own(why) if why.contains("does not exist in the main checkout")),
            "{:?}",
            h.sharing()
        );
    }

    #[test]
    fn share_home_disabled() {
        let (_tmp, _main, wt) = repo_with_worktree();
        let h = Homes::resolve(&wt, None, false).expect("resolve");
        assert_own(&h, &wt);
        assert!(!h.sharing().is_shared());
    }

    #[test]
    fn absolute_home_dir_wins() {
        let (tmp, _main, wt) = repo_with_worktree();
        let abs = tmp.path().join("abs-home");
        let h = Homes::resolve(&wt, Some(&abs), true).expect("resolve");
        assert_eq!(h.shared().as_path(), abs);
        assert_eq!(h.checkout().as_path(), abs);
        assert_eq!(
            h.sharing(),
            &HomeSharing::Own("homeDir is absolute".to_string())
        );
    }

    /// A root reached through a symlink finds the checkout it really is in,
    /// even when the `.git` is above the symlink's target and so never on the
    /// symlink's own ancestors. The checkout home stays as the root was
    /// spelled; the shared one is the main checkout's real directory.
    #[cfg(unix)]
    #[test]
    fn symlinked_root_is_detected_canonically() {
        let (tmp, main, wt) = repo_with_worktree();
        std::fs::create_dir_all(main.join("sub")).expect("mkdir");
        std::fs::create_dir_all(wt.join("sub")).expect("mkdir");
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("mkdir");
        let link = elsewhere.join("proj");
        std::os::unix::fs::symlink(wt.join("sub"), &link).expect("symlink");

        let h = resolve(&link, None);
        assert_eq!(
            h.shared().as_path(),
            main.join("sub").join(DEFAULT_HOME_DIR)
        );
        assert_eq!(h.checkout().as_path(), link.join(DEFAULT_HOME_DIR));

        // The main checkout's own root through a symlink is still the main
        // checkout, not a worktree of itself.
        let main_link = elsewhere.join("main");
        std::os::unix::fs::symlink(&main, &main_link).expect("symlink");
        let m = resolve(&main_link, None);
        assert_eq!(
            m.sharing(),
            &HomeSharing::Main {
                linked_worktrees: 1
            }
        );
    }

    /// A worktree that lives inside the home it would share must not share
    /// it: the home would contain its sources.
    #[test]
    fn worktree_inside_the_shared_home_keeps_its_own() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize");
        let main = base.join("main");
        main_checkout(&main, "master");
        let wt = main.join(DEFAULT_HOME_DIR).join("wt");
        linked_worktree(&main, &wt, "wt", "feat");
        let h = resolve(&wt, None);
        assert_own(&h, &wt);
    }

    #[test]
    fn relative_home_dir_is_detected() {
        let (_tmp, main, wt) = repo_with_worktree();
        let h = resolve(&wt, Some("state/h"));
        assert_eq!(h.shared().as_path(), main.join("state/h"));
        assert_eq!(h.checkout().as_path(), wt.join("state/h"));
    }
}
