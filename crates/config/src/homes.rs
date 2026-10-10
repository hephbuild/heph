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
//! filesystem-walk cache, approval notices, and the cache entries of targets
//! that never go to a remote cache. Outside a linked worktree the two are the
//! same directory. See `docs/HOME_DIR.md`.

use std::fmt;
use std::ops::Deref;
use std::path::{Path, PathBuf};

use crate::git_checkout::{CheckoutKind, GitCheckout, LinkedWorktrees, linked_worktrees};
use crate::home_dir::{DEFAULT_HOME_DIR, HomeDir};

/// This checkout's own home — a type of its own, not a [`HomeDir`], so a site
/// that needs one cannot be handed the other: everything that belongs to one
/// working tree (sandboxes, their FUSE mount and execute lock, the fswalk cache,
/// approval notices, local-only cache entries) takes a `CheckoutHome`,
/// everything else the shared [`HomeDir`].
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

/// The most checkouts the shared store's `cache.history` scales by
/// ([`HomeSharing::shared_history`]).
///
/// Sized for the common shape: the main checkout plus a few branches in
/// flight at once, each keeping its own revisions. Past that, scaling would
/// stop paying for itself: a repository with dozens of agent worktrees would
/// multiply every target's footprint in the shared cache by dozens, so disk
/// stays bounded at this many times `history`, and the worktrees beyond it
/// evict each other as they did before scaling.
pub const MAX_HISTORY_CHECKOUTS: u32 = 4;

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
        worktrees: LinkedWorktrees,
    },
    /// The main checkout of a repository that has linked worktrees, which may
    /// use this home. Decided whatever this checkout's own `shareHome` says:
    /// that flag only decides whether *this* checkout uses another's home,
    /// not whether others use this one.
    Main { worktrees: LinkedWorktrees },
}

impl HomeSharing {
    /// Whether another checkout may use the shared home — so a target that does
    /// not resolve *here* may still be someone else's.
    pub fn is_shared(&self) -> bool {
        match self {
            HomeSharing::Own(_) => false,
            HomeSharing::Linked { .. } => true,
            HomeSharing::Main { worktrees } => worktrees.registered > 0,
        }
    }

    /// The checkouts that may build into the shared home: the main checkout
    /// plus every linked worktree that still exists on disk. A registered
    /// worktree whose working tree is gone builds nothing, so it does not
    /// count. 1 when the home is this checkout's own.
    ///
    /// Read from the `LinkedWorktrees` detection counted at resolve time, so
    /// it is fixed for the process.
    pub fn checkouts(&self) -> u32 {
        match self {
            HomeSharing::Own(_) => 1,
            HomeSharing::Linked { worktrees, .. } | HomeSharing::Main { worktrees } => {
                let live = worktrees.registered.saturating_sub(worktrees.missing);
                u32::try_from(live).unwrap_or(u32::MAX).saturating_add(1)
            }
        }
    }

    /// The `cache.history` the **shared** store enforces for a target declaring
    /// `history`: `history × min(checkouts, MAX_HISTORY_CHECKOUTS)`. In the
    /// shared store a target's revisions are every checkout's, so a per-target
    /// budget of `history` would let two checkouts building different revisions
    /// evict each other; scaled, each keeps roughly its own `history`, up to
    /// the cap. Unchanged when the home is not shared. A checkout's own store
    /// keeps plain `history`.
    pub fn shared_history(&self, history: u32) -> u32 {
        history.saturating_mul(self.checkouts().min(MAX_HISTORY_CHECKOUTS))
    }

    /// "N registered worktree(s) no longer exist; run `git worktree prune`",
    /// when some do not. A stale registration keeps the home counted as shared.
    pub fn stale_worktrees_note(&self) -> Option<String> {
        let missing = match self {
            HomeSharing::Own(_) => 0,
            HomeSharing::Linked { worktrees, .. } | HomeSharing::Main { worktrees } => {
                worktrees.missing
            }
        };
        (missing > 0).then(|| {
            format!("{missing} registered worktree(s) no longer exist; run `git worktree prune`")
        })
    }
}

impl fmt::Display for HomeSharing {
    /// Reads as the end of "the home is …".
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HomeSharing::Own(why) => write!(f, "this checkout's own ({why})"),
            HomeSharing::Linked {
                main_root,
                name,
                worktrees,
            } => write!(
                f,
                "the main checkout's at {}, used by linked worktree {name} and shared with {} linked worktree(s)",
                main_root.display(),
                worktrees.registered
            ),
            HomeSharing::Main { worktrees } => {
                write!(f, "shared with {} linked worktree(s)", worktrees.registered)
            }
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

/// What detection decided, and whether the fallback is worth a warning.
struct Detected {
    sharing: HomeSharing,
    /// A `.git` or `commondir` is there but could not be followed.
    warn: bool,
}

impl Detected {
    fn quiet(sharing: HomeSharing) -> Self {
        Self {
            sharing,
            warn: false,
        }
    }

    fn own(why: impl Into<String>) -> Self {
        Self::quiet(HomeSharing::Own(why.into()))
    }
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
    /// git worktree whose root maps to an existing heph workspace in the main
    /// checkout, with the same `homeDir` and a writable home, the shared home
    /// is that workspace's home; otherwise it is the checkout's own. Detection
    /// never fails the resolve: every reason it gives up lands in
    /// [`Homes::sharing`] and is logged (at `warn` for a `.git` or `commondir`
    /// that is there but unreadable, at `debug` otherwise).
    pub fn resolve(
        root: &Path,
        configured: Option<&Path>,
        share_home: bool,
    ) -> anyhow::Result<Self> {
        Self::resolve_with(root, configured, Some(share_home))
    }

    /// `detect: None` turns worktree detection off entirely (tests only).
    fn resolve_with(
        root: &Path,
        configured: Option<&Path>,
        detect: Option<bool>,
    ) -> anyhow::Result<Self> {
        let own = HomeDir::resolve(root, configured)?;
        let rel = configured.unwrap_or(Path::new(DEFAULT_HOME_DIR));

        let detected = if rel.is_absolute() {
            Detected::own("homeDir is absolute")
        } else {
            match detect {
                None => Detected::own("worktree detection is off"),
                Some(share_home) => detect_sharing(root, rel, share_home),
            }
        };
        let sharing = detected.sharing;
        let shared = match &sharing {
            // Against the main checkout's root, so the same guard holds there.
            HomeSharing::Linked { main_root, .. } => HomeDir::resolve(main_root, configured)?,
            HomeSharing::Own(_) | HomeSharing::Main { .. } => own.clone(),
        };
        let checkout = CheckoutHome(own.as_path().to_path_buf());
        match &sharing {
            HomeSharing::Linked {
                main_root, name, ..
            } => tracing::info!(
                checkout = %checkout.display(),
                "using shared home {} (linked worktree {name} of {})",
                shared.display(),
                main_root.display()
            ),
            HomeSharing::Own(why) if detected.warn => tracing::warn!(
                home = %shared.display(),
                "using this checkout's own home: {why}"
            ),
            _ => tracing::debug!(
                shared = %shared.display(),
                checkout = %checkout.display(),
                "heph home: {sharing}"
            ),
        }
        Ok(Self {
            shared,
            checkout,
            sharing,
        })
    }

    /// The home every checkout of the repository shares: the cache, blobs,
    /// gateway and revision locks, credentials, scratch, diag.
    pub fn shared(&self) -> &HomeDir {
        &self.shared
    }

    /// This checkout's own home: sandboxes, the execute lock, staged inputs
    /// (linked into the sandboxes), the filesystem-walk cache, approval
    /// notices, the cache entries of targets that never go to a remote. The
    /// same directory as [`Homes::shared`] outside a linked worktree.
    pub fn checkout(&self) -> &CheckoutHome {
        &self.checkout
    }

    pub fn sharing(&self) -> &HomeSharing {
        &self.sharing
    }

    /// The default homes of a workspace rooted at `root` (a test's tempdir),
    /// with worktree detection **off**: a test's result must not depend on
    /// whatever git checkout happens to sit above the temp dir. A test that
    /// builds a git layout calls [`Homes::resolve`].
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn for_tests(root: &Path) -> Self {
        Self::resolve_with(root, None, None).expect("resolving test homes")
    }
}

/// Decide whether the checkout at `root` shares a home; `home_rel` is the
/// (relative) `homeDir`.
///
/// On canonical paths: a root reached through a symlink must find the `.git`
/// of the tree it really is in, and "is the main checkout's root this one"
/// must compare real directories, not spellings of them (`/var` is
/// `/private/var` on macOS).
fn detect_sharing(root: &Path, home_rel: &Path, share_home: bool) -> Detected {
    let root = match root.canonicalize() {
        Ok(p) => p,
        Err(e) => return Detected::own(format!("canonicalizing {}: {e}", root.display())),
    };
    let root = root.as_path();
    let co = match GitCheckout::discover(root) {
        Ok(co) => co,
        Err(no) => {
            return Detected {
                warn: no.malformed,
                sharing: HomeSharing::Own(no.reason),
            };
        }
    };
    match co.kind {
        // Counted whatever `share_home` says: it decides whether this
        // checkout uses another's home, and the main checkout uses its own
        // either way. Whether worktrees use *this* one is theirs to decide.
        CheckoutKind::Main { common_dir } => {
            let worktrees = linked_worktrees(&common_dir);
            if worktrees.registered == 0 {
                Detected::own("main checkout without linked worktrees")
            } else {
                Detected::quiet(HomeSharing::Main { worktrees })
            }
        }
        CheckoutKind::Unshared(why) => Detected::own(why),
        CheckoutKind::Linked { .. } if !share_home => Detected::own("worktree.shareHome is false"),
        CheckoutKind::Linked {
            common_dir,
            main_top,
            name,
        } => {
            let Ok(rel) = root.strip_prefix(&co.top) else {
                return Detected::own(format!(
                    "root {} is not under worktree {}",
                    root.display(),
                    co.top.display()
                ));
            };
            let mapped = main_top.join(rel);
            let main_root = match mapped.canonicalize() {
                Ok(p) if p.is_dir() => p,
                _ => {
                    return Detected::own(format!(
                        "{} does not exist in the main checkout",
                        mapped.display()
                    ));
                }
            };
            if main_root == root {
                // A `gitdir` pointing back at this very tree: there is no
                // other checkout to share with.
                return Detected::own(format!(
                    "the main checkout's root is this root ({})",
                    root.display()
                ));
            }
            if let Err(why) = main_home_matches(&main_root, home_rel) {
                return Detected::own(why);
            }
            let would_share = match HomeDir::resolve(&main_root, Some(home_rel)) {
                Ok(h) => h,
                Err(e) => {
                    return Detected::own(format!("resolving the main checkout's home: {e:#}"));
                }
            };
            if root.starts_with(would_share.as_path()) {
                // A worktree placed inside the home it would share: the home
                // would hold its own sources, and walkers refuse anything
                // under a `.heph*` dir.
                return Detected::own(format!(
                    "{} is inside the main checkout's home",
                    root.display()
                ));
            }
            if !writable(&would_share) {
                return Detected::own(format!(
                    "the main checkout's home {} is not writable",
                    would_share.display()
                ));
            }
            Detected::quiet(HomeSharing::Linked {
                main_root,
                name,
                worktrees: linked_worktrees(&common_dir),
            })
        }
    }
}

/// `Ok` when `main_root` is a heph workspace whose own config resolves the
/// same home as this checkout's `homeDir` does. The shared home is the main
/// checkout's, so it is the main checkout's config that says where it is; a
/// worktree on a branch that moved `homeDir` must not invent a home there.
fn main_home_matches(main_root: &Path, home_rel: &Path) -> Result<(), String> {
    if !crate::CONFIG_FILE_NAMES
        .iter()
        .any(|n| main_root.join(n).exists())
    {
        return Err(format!(
            "the main checkout's {} is not a heph workspace (no {})",
            main_root.display(),
            crate::CONFIG_FILE_NAME
        ));
    }
    let cfg = crate::load_from_root(main_root)
        .map_err(|e| format!("loading the main checkout's config: {e:#}"))?;
    let main_home = HomeDir::resolve(main_root, cfg.home_dir.as_deref())
        .map_err(|e| format!("resolving the main checkout's homeDir: {e:#}"))?;
    let ours = HomeDir::resolve(main_root, Some(home_rel))
        .map_err(|e| format!("resolving this checkout's homeDir in the main checkout: {e:#}"))?;
    if main_home != ours {
        return Err(format!(
            "the main checkout's homeDir ({}) differs from this checkout's ({})",
            cfg.home_dir
                .as_deref()
                .unwrap_or(Path::new(DEFAULT_HOME_DIR))
                .display(),
            home_rel.display()
        ));
    }
    Ok(())
}

/// Whether `path` — or, when it does not exist yet, its nearest existing
/// ancestor (where the engine would create it) — is writable by this process.
/// `access(W_OK)` on linux and macOS alike.
fn writable(path: &Path) -> bool {
    path.ancestors()
        .find(|a| a.symlink_metadata().is_ok())
        .is_some_and(|existing| rustix::fs::access(existing, rustix::fs::Access::WRITE_OK).is_ok())
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

    fn one_worktree() -> LinkedWorktrees {
        LinkedWorktrees {
            registered: 1,
            missing: 0,
        }
    }

    /// The shared store's history is `history × checkouts`: the main checkout
    /// plus the linked worktrees that still exist. A registered worktree whose
    /// tree is gone does not count, and an unshared home keeps plain `history`.
    #[test]
    fn shared_history_scales_with_live_checkouts() {
        let wts = |registered, missing| LinkedWorktrees {
            registered,
            missing,
        };
        let own = HomeSharing::Own("not a git checkout".to_string());
        assert_eq!(own.checkouts(), 1);
        assert_eq!(own.shared_history(1), 1);
        assert_eq!(own.shared_history(3), 3);

        let main = HomeSharing::Main {
            worktrees: wts(2, 0),
        };
        assert_eq!(main.checkouts(), 3);
        assert_eq!(main.shared_history(1), 3);
        assert_eq!(main.shared_history(2), 6);
        assert_eq!(main.shared_history(0), 0);

        let linked = HomeSharing::Linked {
            main_root: PathBuf::from("/m"),
            name: "wt".to_string(),
            worktrees: one_worktree(),
        };
        assert_eq!(linked.shared_history(1), 2, "main + this worktree");

        let stale = HomeSharing::Main {
            worktrees: wts(3, 1),
        };
        assert_eq!(stale.shared_history(1), 3, "the missing one is not counted");
        let all_gone = HomeSharing::Main {
            worktrees: wts(2, 2),
        };
        assert!(all_gone.is_shared());
        assert_eq!(
            all_gone.shared_history(1),
            1,
            "only the main checkout is left"
        );

        assert_eq!(main.shared_history(u32::MAX), u32::MAX, "saturates");

        // Capped at `MAX_HISTORY_CHECKOUTS`, however many worktrees there are.
        let many = HomeSharing::Main {
            worktrees: wts(10, 0),
        };
        assert_eq!(many.checkouts(), 11);
        assert_eq!(many.shared_history(1), MAX_HISTORY_CHECKOUTS);
        assert_eq!(many.shared_history(1), 4);
        assert_eq!(many.shared_history(2), 8);
        let at_cap = HomeSharing::Main {
            worktrees: wts(3, 0),
        };
        assert_eq!(at_cap.shared_history(1), 4, "main + 3 is exactly the cap");
    }

    /// A worktree removed without `git worktree prune` stops counting, as
    /// detection reads it.
    #[test]
    fn deleted_worktree_does_not_count_as_a_checkout() {
        let (_tmp, main, wt) = repo_with_worktree();
        assert_eq!(resolve(&main, None).sharing().checkouts(), 2);
        std::fs::remove_dir_all(&wt).expect("remove worktree");
        assert_eq!(resolve(&main, None).sharing().checkouts(), 1);
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

    /// Asserts the fallback, and that its reason says `needle`.
    fn assert_own_because(h: &Homes, root: &Path, needle: &str) {
        assert_own(h, root);
        assert!(
            matches!(h.sharing(), HomeSharing::Own(why) if why.contains(needle)),
            "expected a reason containing {needle:?}, got {:?}",
            h.sharing()
        );
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
                worktrees: one_worktree(),
            }
        );
        // And the main checkout knows its home is shared.
        let m = resolve(&main, None);
        assert_eq!(m.shared().as_path(), m.checkout().as_path());
        assert_eq!(
            m.sharing(),
            &HomeSharing::Main {
                worktrees: one_worktree()
            }
        );
        assert!(m.sharing().is_shared());
    }

    /// `shareHome: false` in the main checkout does not stop it counting its
    /// worktrees: they may still share its home, so gc must still know.
    #[test]
    fn main_counts_worktrees_whatever_its_share_home() {
        let (_tmp, main, _wt) = repo_with_worktree();
        let m = Homes::resolve(&main, None, false).expect("resolve");
        assert_eq!(
            m.sharing(),
            &HomeSharing::Main {
                worktrees: one_worktree()
            }
        );
        assert!(m.sharing().is_shared());
    }

    #[test]
    fn detect_worktree_subdir_root() {
        let (_tmp, main, wt) = repo_with_worktree();
        std::fs::create_dir_all(main.join("sub")).expect("mkdir");
        std::fs::write(main.join("sub").join(crate::CONFIG_FILE_NAME), "").expect("config");
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

    /// git's own form: an absolute `gitdir:` and a relative `commondir`.
    #[test]
    fn detect_relative_commondir() {
        let (_tmp, main, wt) = repo_with_worktree();
        assert_eq!(
            resolve(&wt, None).shared().as_path(),
            main.join(DEFAULT_HOME_DIR)
        );
    }

    /// A relative `gitdir:` in the `.git` file, anchored at the file's dir.
    #[test]
    fn detect_relative_gitdir() {
        let (_tmp, main, wt) = repo_with_worktree();
        std::fs::write(wt.join(".git"), "gitdir: ../main/.git/worktrees/wt\n").expect(".git");
        assert_eq!(
            resolve(&wt, None).shared().as_path(),
            main.join(DEFAULT_HOME_DIR)
        );
    }

    /// An absolute `commondir`.
    #[test]
    fn detect_absolute_commondir() {
        let (_tmp, main, wt) = repo_with_worktree();
        let admin = main.join(".git").join("worktrees").join("wt");
        std::fs::write(
            admin.join("commondir"),
            format!("{}\n", main.join(".git").display()),
        )
        .expect("commondir");
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

        let cases: [(&str, &[u8], &str); 5] = [
            ("empty file", b"", "no `gitdir:` line"),
            (
                "no gitdir prefix",
                b"/somewhere/else\n",
                "no `gitdir:` line",
            ),
            ("empty path", b"gitdir:   \n", "empty path"),
            ("non-UTF-8", b"gitdir: /tmp/\xff\xfe\n", "not UTF-8"),
            (
                "dangling",
                b"gitdir: /definitely/not/here/.git/worktrees/x\n",
                "which is not a directory",
            ),
        ];
        for (what, bytes, reason) in cases {
            std::fs::write(&dot_git, bytes).expect("write");
            let h = resolve(&wt, None);
            assert_own(&h, &wt);
            assert!(
                matches!(h.sharing(), HomeSharing::Own(why) if why.contains(reason)),
                "{what}: expected {reason:?} in {:?}",
                h.sharing()
            );
        }

        // A dangling or malformed commondir is a fallback too.
        std::fs::write(&dot_git, format!("gitdir: {}\n", admin.display())).expect("write");
        for (bytes, reason) in [
            (&b""[..], "empty path"),
            (b"/no/such/common\n", "which is not a directory"),
            (b"\xff\n", "not UTF-8"),
        ] {
            std::fs::write(admin.join("commondir"), bytes).expect("write");
            assert_own_because(&resolve(&wt, None), &wt, reason);
        }
    }

    /// A main checkout whose mapped root is, through a symlink, this very
    /// worktree's root: there is no other checkout to share with.
    #[cfg(unix)]
    #[test]
    fn detect_main_root_is_this_root() {
        let (_tmp, main, wt) = repo_with_worktree();
        std::fs::create_dir_all(wt.join("sub")).expect("mkdir");
        std::os::unix::fs::symlink(wt.join("sub"), main.join("sub")).expect("symlink");
        let root = wt.join("sub");
        assert_own_because(
            &resolve(&root, None),
            &root,
            "the main checkout's root is this root",
        );
    }

    #[test]
    fn detect_mapped_root_missing() {
        let (_tmp, _main, wt) = repo_with_worktree();
        // `wt/only-here` has no counterpart in the main checkout.
        let root = wt.join("only-here");
        std::fs::create_dir_all(&root).expect("mkdir");
        assert_own_because(
            &resolve(&root, None),
            &root,
            "does not exist in the main checkout",
        );
    }

    #[test]
    fn main_not_a_heph_workspace_keeps_own() {
        let (_tmp, main, wt) = repo_with_worktree();
        std::fs::remove_file(main.join(crate::CONFIG_FILE_NAME)).expect("rm config");
        assert_own_because(&resolve(&wt, None), &wt, "is not a heph workspace");
    }

    #[test]
    fn main_config_that_fails_to_load_keeps_own() {
        let (_tmp, main, wt) = repo_with_worktree();
        std::fs::write(main.join(crate::CONFIG_FILE_NAME), "notAKey: [\n").expect("write");
        assert_own_because(
            &resolve(&wt, None),
            &wt,
            "loading the main checkout's config",
        );
    }

    /// The shared home is where the *main* checkout's config puts it. A
    /// worktree whose `homeDir` differs keeps its own rather than inventing a
    /// home in the main checkout.
    #[test]
    fn main_home_dir_differs_keeps_own() {
        let (_tmp, main, wt) = repo_with_worktree();
        std::fs::write(main.join(crate::CONFIG_FILE_NAME), "homeDir: .elsewhere\n").expect("write");
        assert_own_because(&resolve(&wt, None), &wt, "differs from this checkout's");

        // Both saying the same thing in different spellings is the same home.
        std::fs::write(main.join(crate::CONFIG_FILE_NAME), "homeDir: ./.heph\n").expect("write");
        assert_eq!(
            resolve(&wt, None).shared().as_path(),
            main.join(DEFAULT_HOME_DIR)
        );
    }

    /// A main checkout's home that this process cannot write — or, before it
    /// exists, whose nearest existing parent it cannot — is not shared.
    #[cfg(unix)]
    #[test]
    fn main_home_not_writable_keeps_own() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, main, wt) = repo_with_worktree();
        let set = |p: &Path, mode| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).expect("chmod");
        };
        // Root ignores mode bits; nothing to test there.
        let probe = main.join("probe");
        set(&main, 0o555);
        let as_root = std::fs::write(&probe, "").is_ok();
        if as_root {
            set(&main, 0o755);
            eprintln!(
                "skipping main_home_not_writable_keeps_own: running as root, which ignores \
                 mode bits, so an unwritable main home cannot be set up"
            );
            return;
        }

        // The home does not exist yet: its parent (the main checkout) decides.
        assert_own_because(&resolve(&wt, None), &wt, "is not writable");
        set(&main, 0o755);
        assert!(resolve(&wt, None).sharing().is_shared());

        // The home exists and is read-only.
        let home = main.join(DEFAULT_HOME_DIR);
        std::fs::create_dir_all(&home).expect("mkdir");
        set(&home, 0o555);
        let h = resolve(&wt, None);
        set(&home, 0o755);
        assert_own_because(&h, &wt, "is not writable");
    }

    #[test]
    fn share_home_disabled() {
        let (_tmp, _main, wt) = repo_with_worktree();
        let h = Homes::resolve(&wt, None, false).expect("resolve");
        assert_own(&h, &wt);
        assert!(!h.sharing().is_shared());
    }

    /// Engine tests run under a temp dir that may sit inside some git
    /// checkout; `for_tests` must not care.
    #[test]
    fn for_tests_does_not_detect() {
        let (_tmp, _main, wt) = repo_with_worktree();
        assert_own_because(&Homes::for_tests(&wt), &wt, "detection is off");
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
        std::fs::write(main.join("sub").join(crate::CONFIG_FILE_NAME), "").expect("config");
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
                worktrees: one_worktree()
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
        std::fs::write(main.join(crate::CONFIG_FILE_NAME), "homeDir: state/h\n").expect("write");
        let h = resolve(&wt, Some("state/h"));
        assert_eq!(h.shared().as_path(), main.join("state/h"));
        assert_eq!(h.checkout().as_path(), wt.join("state/h"));
    }

    #[test]
    fn stale_worktrees_are_noted() {
        let (tmp, main, _wt) = repo_with_worktree();
        let gone = tmp.path().canonicalize().expect("canon").join("gone");
        linked_worktree(&main, &gone, "gone", "b");
        std::fs::remove_dir_all(&gone).expect("rm");
        let m = resolve(&main, None);
        assert_eq!(
            m.sharing().stale_worktrees_note().as_deref(),
            Some("1 registered worktree(s) no longer exist; run `git worktree prune`")
        );
        assert_eq!(m.sharing().to_string(), "shared with 2 linked worktree(s)");
    }
}
