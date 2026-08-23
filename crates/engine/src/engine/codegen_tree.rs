//! Writing the per-directory codegen provenance registry (`.hephgen`).
//!
//! The reader lives in [`hwalk::codegen`]; this is the half that maintains it,
//! driven by the codegen write-back in [`result`](crate::engine::result). It
//! never walks the graph: a target states its own output paths, so registering
//! them is `O(files written)`.
//!
//! # Ordering: register before publish
//!
//! The run that writes a generated file is also a run that globs the tree, so
//! the two must not be able to observe each other half-done. Two rules make that
//! structural rather than something to time:
//!
//! 1. **The registry entry is written before the file it describes.** A net-new
//!    file is therefore registered before it exists, and an entry that names a
//!    file that is not there yet matches nothing and is inert. The reverse
//!    ordering is what the xattr had, and it leaves a window where the bytes are
//!    on disk and the provenance is not.
//! 2. **A rewrite widens the record for the duration of the swap.** Between
//!    updating the entry and renaming the new bytes in, the file still holds the
//!    old bytes while the entry names the new hash — so the entry carries the
//!    old hash as `prev=` until the rename lands, and every instant of the
//!    window has the on-disk content matching *some* accepted hash.
//!
//! Publication is always a rename into the destination directory, which is also
//! what keeps the reader's cache honest: renaming bumps the directory's mtime,
//! and the parsed registry rides inside the mtime-validated directory listing.
//! An in-place rewrite of a registry file would leave that listing valid and the
//! registry stale — so registry writes go through [`write_atomically`], always.

use anyhow::{Context, Result};
use hwalk::codegen::{Entry, REGISTRY_NAME, Record, Registry};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Prefix for the temporary file a registry or generated file is written to
/// before being renamed into place.
///
/// It starts with `.heph`, which every tree walk already skips as
/// engine-internal — so a concurrent glob can never pick up a half-written file
/// that is about to be renamed away.
const TMP_PREFIX: &str = ".heph-tmp-";

/// The line added to `.git/info/exclude`, and the comment that identifies it.
const GIT_EXCLUDE_PATTERN: &str = "**/.hephgen";
const GIT_EXCLUDE_COMMENT: &str = "# heph: per-directory codegen provenance registries";

/// Write `bytes` to `path` atomically: a sibling temp file, then a rename.
///
/// Readers never observe a torn file, and the rename bumps the parent
/// directory's mtime, which is what invalidates the cached listing the registry
/// is served from.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8], exec: bool) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent directory: {}", path.display()))?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create parent dir for {}", path.display()))?;
    // Unique per call: this process may be writing several files into the
    // directory at once, and another process its own.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("codegen");
    let tmp = dir.join(format!("{TMP_PREFIX}{}-{seq}.{name}", std::process::id()));
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    #[cfg(unix)]
    if exec {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&tmp)
            .map(|m| m.permissions().mode())
            .unwrap_or(0o644);
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode | 0o111))
            .with_context(|| format!("set exec bit on {}", tmp.display()))?;
    }
    #[cfg(not(unix))]
    let _ = exec;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Leaving a `.heph-tmp-*` behind would be invisible to globs but
            // visible to the user forever.
            drop(std::fs::remove_file(&tmp));
            Err(e).with_context(|| format!("publish {}", path.display()))
        }
    }
}

/// Exclusive lock over one directory's registry, held for a read-modify-write.
///
/// The lock file lives under the heph home rather than beside the registry: the
/// registry is published by rename, so its inode changes under any lock taken on
/// the path itself, and a second writer would then hold a lock on a dead inode.
/// A stable path in the home has neither problem and keeps the tree clean.
///
/// `flock` belongs to the open file description, so this excludes concurrent
/// write-backs in *this* process as well as in another `heph`.
struct DirLock {
    file: std::fs::File,
}

impl DirLock {
    fn acquire(home: &Path, dir: &Path) -> Result<Self> {
        use std::os::unix::io::AsRawFd as _;
        let locks = home.join("codegen-locks");
        std::fs::create_dir_all(&locks).with_context(|| format!("create {}", locks.display()))?;
        let mut h = xxhash_rust::xxh3::Xxh3::new();
        h.update(dir.to_string_lossy().as_bytes());
        let path = locks.join(format!("{:x}.lock", h.digest()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("open codegen lock {}", path.display()))?;
        // Blocking: the critical section is a small read + render + rename, and
        // contention needs two targets emitting into the same directory at once.
        // SAFETY: `file` owns a valid open fd for the duration of the call.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("lock codegen registry for {}", dir.display()));
        }
        Ok(Self { file })
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd as _;
        // SAFETY: the fd is still open — `file` is dropped after this.
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Whether a flush may drop this target's rows that are no longer in `mine`.
/// Only the final one may: see [`TreeRegistry::register`].
#[derive(Clone, Copy)]
enum Prune {
    Yes,
    No,
}

/// One directory's registry state for the duration of a write-back.
#[derive(Default)]
struct DirState {
    /// The registry as it was read from disk, used to answer ownership
    /// questions about files this target did not write.
    disk: Registry,
    /// The entries this target owns here, by name — what the next flush states.
    mine: BTreeMap<String, Entry>,
    /// Whether this target actually produced anything in this directory. Only
    /// then does the flush drop its other entries here as stale; a directory
    /// merely *consulted* must not have its records deleted.
    produced: bool,
}

/// Accumulates registry updates for one target's codegen write-back.
pub(crate) struct TreeRegistry {
    home: PathBuf,
    root: PathBuf,
    owner: String,
    git_exclude: bool,
    dirs: BTreeMap<PathBuf, DirState>,
    /// Whether any registry file on disk actually changed. Folds into the
    /// write-back's `wrote`: registering a path changes what a later glob
    /// sources, even when no byte of the file itself moved.
    changed: bool,
}

impl TreeRegistry {
    pub(crate) fn new(root: &Path, home: &Path, owner: String, git_exclude: bool) -> Self {
        // A `Config` built by hand can carry an empty or relative home (the
        // resolved one always defaults to `root/.heph3`), and the lock directory
        // must land there rather than in whatever the process's cwd happens to
        // be.
        let home = if home.as_os_str().is_empty() {
            root.join(".heph3")
        } else if home.is_relative() {
            root.join(home)
        } else {
            home.to_path_buf()
        };
        Self {
            home,
            root: root.to_path_buf(),
            owner,
            git_exclude,
            dirs: BTreeMap::new(),
            changed: false,
        }
    }

    fn state(&mut self, dir: &Path) -> &mut DirState {
        self.dirs
            .entry(dir.to_path_buf())
            .or_insert_with(|| DirState {
                disk: Registry::load(dir),
                ..DirState::default()
            })
    }

    /// The `codegen = "copy"` target that owns `dest`, if it is a generated file
    /// owned by someone *other* than this one.
    ///
    /// This is what stops an `in_place` target from rewriting a file a copy
    /// target owns: doing so would clobber the copy target's output and leave
    /// the provenance naming the wrong producer.
    pub(crate) fn foreign_owner(&mut self, dest: &Path) -> Option<String> {
        let (dir, name) = split(dest)?;
        let owner = self.owner.clone();
        let state = self.state(&dir);
        match hbuiltins::pluginfs::codegen_owner_uncached(&state.disk, dest, &name)? {
            hbuiltins::pluginfs::CodegenOwner::Target(t) if t == owner => None,
            hbuiltins::pluginfs::CodegenOwner::Target(t) => Some(t.to_string()),
            // A legacy xattr stamp names no owner. `in_place` outputs were never
            // stamped, so a stamp always belongs to some copy target — and this
            // target cannot be it, because a copy target that had written here
            // would have registered the path instead.
            hbuiltins::pluginfs::CodegenOwner::LegacyStamp => Some("<unknown>".to_string()),
        }
    }

    /// Record `dest` as this target's generated file holding `hash`, accepting
    /// `prev` as well until the swap that follows has landed. Flushed to disk
    /// immediately: the entry must be readable before the bytes are.
    pub(crate) fn register_file(
        &mut self,
        dest: &Path,
        hash: String,
        prev: Option<String>,
    ) -> Result<()> {
        self.register(dest, Record::File { hash, prev })
    }

    /// Record `dest` as this target's generated symlink to `target`.
    pub(crate) fn register_symlink(&mut self, dest: &Path, target: &str) -> Result<()> {
        self.register(
            dest,
            Record::Symlink {
                target: target.to_string(),
            },
        )
    }

    fn register(&mut self, dest: &Path, record: Record) -> Result<()> {
        let Some((dir, name)) = split(dest) else {
            return Ok(());
        };
        let owner = self.owner.clone();
        let entry = Entry {
            name: name.clone(),
            owner,
            record,
        };
        let state = self.state(&dir);
        state.produced = true;
        // Already on disk, exactly as stated: the file it describes cannot be
        // published ahead of a record that is already there, so there is nothing
        // to flush. This is the steady state — a cache hit whose write-back
        // rewrites nothing — and it must cost no lock, no read and no rename.
        let settled = state.disk.get(&name) == Some(&entry);
        state.mine.insert(name, entry);
        if settled {
            return Ok(());
        }
        // Additive only. Pruning what this target no longer emits waits for
        // `finish`: mid-write-back, `mine` holds the files registered *so far*,
        // and pruning against it would momentarily unregister the ones still to
        // come — whose bytes are already sitting in the tree from the last run.
        self.flush_dir(&dir, Prune::No)
    }

    /// Narrow every in-flight `prev=` back to the single current hash and drop
    /// entries this target no longer produces, then write each touched registry.
    ///
    /// Returns whether any registry file changed — the caller folds it into
    /// `wrote`, because a changed registry changes what the next glob sources.
    pub(crate) fn finish(&mut self) -> Result<bool> {
        for dir in self.dirs.keys().cloned().collect::<Vec<_>>() {
            if let Some(state) = self.dirs.get_mut(&dir) {
                if !state.produced {
                    continue;
                }
                for entry in state.mine.values_mut() {
                    if let Record::File { prev, .. } = &mut entry.record {
                        *prev = None;
                    }
                }
            }
            self.flush_dir(&dir, Prune::Yes)?;
        }
        Ok(self.changed)
    }

    /// Re-read the registry under the lock, apply this target's entries onto it,
    /// and publish if the result differs from what is there.
    ///
    /// The re-read is what makes concurrent write-backs safe: another target
    /// emitting into the same directory may have added its own rows since this
    /// one loaded the file, and they must survive. Only rows owned by *this*
    /// target are replaced.
    fn flush_dir(&mut self, dir: &Path, prune: Prune) -> Result<()> {
        let owner = self.owner.clone();
        let lock = DirLock::acquire(&self.home, dir)?;
        let path = dir.join(REGISTRY_NAME);
        let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
        let mut merged = Registry::parse(&on_disk);
        let Some(state) = self.dirs.get(dir) else {
            return Ok(());
        };
        // Only a directory this target wrote to gets its stale rows pruned;
        // otherwise a target that merely consulted a directory would delete
        // records it never replaced.
        if state.produced && matches!(prune, Prune::Yes) {
            let names: Vec<String> = state.mine.keys().cloned().collect();
            merged.retain_owned(&owner, &names);
        }
        for entry in state.mine.values() {
            merged.upsert(entry.clone());
        }
        let rendered = merged.render();
        let unchanged = rendered == on_disk;
        if !unchanged {
            write_atomically(&path, rendered.as_bytes(), false)?;
        }
        drop(lock);
        if let Some(state) = self.dirs.get_mut(dir) {
            state.disk = merged;
        }
        if unchanged {
            return Ok(());
        }
        self.changed = true;
        // Created, not updated: the first registry this clone has seen is what
        // arms the one-time `.git/info/exclude` line.
        if on_disk.is_empty() && self.git_exclude {
            ensure_git_exclude(&self.root);
        }
        Ok(())
    }
}

/// Split an absolute file path into (directory, file name).
fn split(path: &Path) -> Option<(PathBuf, String)> {
    let dir = path.parent()?.to_path_buf();
    let name = path.file_name()?.to_str()?.to_string();
    Some((dir, name))
}

/// Add `**/.hephgen` to this clone's `.git/info/exclude`, once.
///
/// The registry files are heph machinery that exists only where heph has run —
/// a per-clone fact, which is exactly what `info/exclude` is for. Putting the
/// pattern there means nothing is committed, no diff appears, and nobody has to
/// remember a command for their tree to stay clean. The *generated files*
/// themselves are a repo-wide fact and still belong in a committed `.gitignore`
/// (`heph tool gen-gitignore`).
///
/// Best-effort by design: no git, an unwritable file, or a repo laid out in a
/// way this does not recognise costs the user a few untracked entries, never a
/// build. Once per process, which is once per run — a process that served two
/// workspaces would only arm the first, and nothing that writes codegen back
/// does that.
fn ensure_git_exclude(root: &Path) {
    static DONE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    DONE.get_or_init(|| {
        if let Err(e) = try_git_exclude(root) {
            tracing::debug!(error = %format!("{e:#}"), "could not update .git/info/exclude");
        }
    });
}

fn try_git_exclude(root: &Path) -> Result<()> {
    let Some(gitdir) = git_common_dir(root) else {
        return Ok(());
    };
    let path = gitdir.join("info").join("exclude");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == GIT_EXCLUDE_PATTERN) {
        return Ok(());
    }
    std::fs::create_dir_all(gitdir.join("info")).context("create .git/info")?;
    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(GIT_EXCLUDE_COMMENT);
    out.push('\n');
    out.push_str(GIT_EXCLUDE_PATTERN);
    out.push('\n');
    std::fs::write(&path, out).with_context(|| format!("write {}", path.display()))?;
    tracing::debug!(path = %path.display(), "excluded {GIT_EXCLUDE_PATTERN} for this clone");
    Ok(())
}

/// The git *common* directory for the checkout at `root`, without shelling out
/// to `git`.
///
/// `.git` is a directory in an ordinary clone and a **file** in a linked
/// worktree (`gitdir: /path/to/.git/worktrees/name`). A worktree's gitdir in
/// turn holds a `commondir` file pointing at the shared directory, which is
/// where `info/exclude` lives — so a pattern written there covers every worktree
/// of the repo, which is what someone working across several of them wants.
fn git_common_dir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    let meta = std::fs::metadata(&dot_git).ok()?;
    let gitdir = if meta.is_dir() {
        dot_git
    } else {
        let text = std::fs::read_to_string(&dot_git).ok()?;
        let rel = text
            .lines()
            .find_map(|l| l.trim().strip_prefix("gitdir:"))?;
        resolve(root, rel.trim())
    };
    let common = gitdir.join("commondir");
    match std::fs::read_to_string(&common) {
        Ok(text) => Some(resolve(&gitdir, text.trim())),
        Err(_) => Some(gitdir),
    }
}

/// Resolve a possibly-relative git path against `base`.
fn resolve(base: &Path, p: &str) -> PathBuf {
    let p = Path::new(p);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_exclude_is_idempotent_and_preserves_existing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git/info")).expect("mkdir");
        std::fs::write(root.join(".git/info/exclude"), "*.local\n").expect("seed");

        try_git_exclude(root).expect("first");
        try_git_exclude(root).expect("second");

        let text = std::fs::read_to_string(root.join(".git/info/exclude")).expect("read");
        assert!(
            text.starts_with("*.local\n"),
            "existing lines survive: {text}"
        );
        assert_eq!(
            text.lines()
                .filter(|l| l.trim() == GIT_EXCLUDE_PATTERN)
                .count(),
            1,
            "the pattern is added exactly once: {text}"
        );
    }

    /// A linked worktree's `.git` is a file, and its gitdir points at the shared
    /// common dir — where the exclude that covers every worktree lives.
    #[test]
    fn git_exclude_follows_a_worktree_to_the_common_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let main = dir.path().join("main");
        let wt = dir.path().join("wt");
        let wt_gitdir = main.join(".git/worktrees/wt");
        std::fs::create_dir_all(&wt_gitdir).expect("mkdir");
        std::fs::create_dir_all(&wt).expect("mkdir");
        std::fs::write(wt_gitdir.join("commondir"), "../..\n").expect("commondir");
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", wt_gitdir.display()),
        )
        .expect("dotgit");

        try_git_exclude(&wt).expect("exclude");

        let text = std::fs::read_to_string(main.join(".git/info/exclude")).expect("read");
        assert!(text.contains(GIT_EXCLUDE_PATTERN), "{text}");
    }

    /// No git, no problem: a workspace that is not a checkout still generates.
    #[test]
    fn git_exclude_is_a_no_op_without_git() {
        let dir = tempfile::tempdir().expect("tempdir");
        try_git_exclude(dir.path()).expect("no error");
        assert!(!dir.path().join(".git").exists());
    }

    #[test]
    fn registers_before_publishing_and_prunes_its_own_stale_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("root");
        let home = dir.path().join("home");
        let pkg = root.join("pkg");
        std::fs::create_dir_all(&pkg).expect("mkdir");

        let mut reg = TreeRegistry::new(&root, &home, "//pkg:gen".to_string(), false);
        reg.register_file(&pkg.join("a.go"), "h1".to_string(), None)
            .expect("register a");
        reg.register_file(&pkg.join("b.go"), "h2".to_string(), None)
            .expect("register b");
        // Registered before either file exists — an entry naming a file that is
        // not there matches nothing, which is what makes the ordering safe.
        assert!(!pkg.join("a.go").exists());
        let parsed = Registry::load(&pkg);
        assert!(parsed.get("a.go").is_some() && parsed.get("b.go").is_some());
        assert!(reg.finish().expect("finish"));

        // A later run that only emits `a.go` drops its own record for `b.go`.
        let mut reg = TreeRegistry::new(&root, &home, "//pkg:gen".to_string(), false);
        reg.register_file(&pkg.join("a.go"), "h1".to_string(), None)
            .expect("register a");
        reg.finish().expect("finish");
        let parsed = Registry::load(&pkg);
        assert!(parsed.get("a.go").is_some());
        assert!(parsed.get("b.go").is_none(), "stale own entry pruned");
    }

    /// Mid-write-back, a target's *other* files are still on disk from the last
    /// run — so registering the first must not unregister them. Pruning waits
    /// for `finish`, when what the target emits is fully known.
    #[test]
    fn a_partial_write_back_never_unregisters_what_is_still_to_come() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("root");
        let home = dir.path().join("home");
        let pkg = root.join("pkg");
        std::fs::create_dir_all(&pkg).expect("mkdir");

        let mut first = TreeRegistry::new(&root, &home, "//pkg:gen".to_string(), false);
        first
            .register_file(&pkg.join("a.go"), "h1".to_string(), None)
            .expect("a");
        first
            .register_file(&pkg.join("b.go"), "h2".to_string(), None)
            .expect("b");
        first.finish().expect("finish");

        // Second run, one file in: `b.go` is still in the tree and must still be
        // registered at this instant.
        let mut second = TreeRegistry::new(&root, &home, "//pkg:gen".to_string(), false);
        second
            .register_file(
                &pkg.join("a.go"),
                "h1-new".to_string(),
                Some("h1".to_string()),
            )
            .expect("a again");
        let mid = Registry::load(&pkg);
        assert!(
            mid.get("b.go").is_some(),
            "a file not yet reached must keep its record: {mid:?}",
        );
        // ...and the in-flight rewrite of `a.go` accepts both sides of the swap.
        let a = mid.get("a.go").expect("a");
        assert!(a.accepts("h1") && a.accepts("h1-new"));

        second
            .register_file(&pkg.join("b.go"), "h2".to_string(), None)
            .expect("b again");
        second.finish().expect("finish");
        let end = Registry::load(&pkg);
        assert!(end.get("a.go").expect("a").accepts("h1-new"));
        assert!(
            !end.get("a.go").expect("a").accepts("h1"),
            "the rewrite window closes at finish",
        );
    }

    #[test]
    fn a_second_target_in_the_same_directory_is_preserved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("root");
        let home = dir.path().join("home");
        let pkg = root.join("pkg");
        std::fs::create_dir_all(&pkg).expect("mkdir");

        let mut a = TreeRegistry::new(&root, &home, "//pkg:a".to_string(), false);
        a.register_file(&pkg.join("a.go"), "h1".to_string(), None)
            .expect("a");
        a.finish().expect("finish a");

        let mut b = TreeRegistry::new(&root, &home, "//pkg:b".to_string(), false);
        b.register_file(&pkg.join("b.go"), "h2".to_string(), None)
            .expect("b");
        b.finish().expect("finish b");

        let parsed = Registry::load(&pkg);
        assert_eq!(parsed.entries().len(), 2, "{parsed:?}");
        assert_eq!(parsed.get("a.go").expect("a").owner, "//pkg:a");
        assert_eq!(parsed.get("b.go").expect("b").owner, "//pkg:b");
    }

    #[test]
    fn an_unchanged_registry_is_not_rewritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("root");
        let home = dir.path().join("home");
        let pkg = root.join("pkg");
        std::fs::create_dir_all(&pkg).expect("mkdir");

        let mut reg = TreeRegistry::new(&root, &home, "//pkg:gen".to_string(), false);
        reg.register_file(&pkg.join("a.go"), "h1".to_string(), None)
            .expect("register");
        assert!(reg.finish().expect("finish"), "first write is a change");

        // The steady state must not touch the file: rewriting it would bump the
        // directory mtime and throw away a perfectly good cached listing.
        let before = std::fs::metadata(pkg.join(REGISTRY_NAME))
            .and_then(|m| m.modified())
            .expect("mtime");
        let mut reg = TreeRegistry::new(&root, &home, "//pkg:gen".to_string(), false);
        reg.register_file(&pkg.join("a.go"), "h1".to_string(), None)
            .expect("register");
        assert!(!reg.finish().expect("finish"), "no change to report");
        let after = std::fs::metadata(pkg.join(REGISTRY_NAME))
            .and_then(|m| m.modified())
            .expect("mtime");
        assert_eq!(before, after);
    }
}
