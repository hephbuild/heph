//! The sandbox of one entrypoint invocation (N10, R11, E20).
//!
//! A build's sandbox is per address (`<home>/sandbox/<pkg>/__target_<name>`)
//! and serialized by the execute lock. An invocation takes no lock, so it gets
//! its own directory instead, and any number of invocations of one target run
//! at once — in one heph process or in many sharing a home:
//!
//! ```text
//! <home>/sandbox/<pkg>/__invoke/<name>/<random>/
//!                                        .owner   flock'd for the invocation's life
//! ```
//!
//! * **Created exclusively.** The leaf `create_dir` is exclusive and retried,
//!   a bounded number of times, on `AlreadyExists` only — a pre-created name is
//!   skipped, never reused. Any other error (EACCES, ENOSPC, ENAMETOOLONG)
//!   fails at once. The name is random, not `<pid>_<n>`: two containers sharing
//!   a home can both be pid 1.
//! * **Owned through `.owner`.** The lock file is created under a name the
//!   sweep never looks at (`.owner-pending`), locked, and only then renamed to
//!   `.owner`, so a sweeper can never find an `.owner` that is not yet held.
//!   The kernel drops the lock if heph is SIGKILLed, which is what makes the
//!   leftover sweepable.
//! * **Torn down synchronously** on every exit, success or failure, before the
//!   invocation returns: an invocation has no log tail to keep for a
//!   diagnostic, and a unique path would leak one tree per failed run. The
//!   `.owner` fd is held until `remove_dir_all` has finished, so a sweeper
//!   cannot race the teardown into the same tree.
//! * **Swept** when a crash left one: only a directory containing `.owner`
//!   (or, a crash between the lock and the rename, only `.owner-pending`)
//!   whose lock is free is eligible, nothing is followed through a symlink,
//!   and a directory with neither — anything else that happens to live there —
//!   is left alone. A creator whose fresh `.owner-pending` a sweeper locked
//!   first takes another name. The lock file is removed last, so a teardown
//!   that fails partway stays sweepable. A live invocation's tree is never
//!   walked into. `invoke`
//!   sweeps its own target's siblings (on the cleaner's reclaim lane);
//!   `heph gc` sweeps every one under `<home>/sandbox`.
//!
//! No `sandbox_cleaner::generation` claim is taken: that registry orders the
//! jobs of *one reused path* and is never pruned, so claiming a unique path per
//! invocation would grow it by one entry per call (Open 9). Exclusive creation
//! already makes the path this invocation's alone.

use anyhow::Context as _;
use hmodel::htaddr::Addr;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

/// The directory component every invocation sandbox lives under, inside its
/// package's sandbox dir.
pub(crate) const INVOKE_DIR: &str = "__invoke";
/// The prefix of a build's per-address sandbox (`__target_<name>`), which an
/// invocation's directory must never be mistaken for or land in.
pub(crate) const BUILD_SANDBOX_PREFIX: &str = "__target_";
/// The lock file that marks a live invocation.
const OWNER: &str = ".owner";
/// The lock file's name until it is held.
const OWNER_PENDING: &str = ".owner-pending";
/// How many names an exclusive create tries before giving up. Each retry needs
/// a 128-bit random name to be taken already.
const CREATE_ATTEMPTS: usize = 8;

/// `<home>/sandbox/<pkg>/__invoke/<name>`: the parent of every invocation
/// sandbox of `addr`. Nested rather than `__invoke_<name>`, so the siblings
/// of target `foo` never include `foo_bar`'s.
pub(crate) fn invoke_parent(home: &Path, addr: &Addr) -> PathBuf {
    let mut dir = home.join("sandbox");
    for c in addr.package.components() {
        dir = dir.join(c);
    }
    let dir = dir.join(INVOKE_DIR);
    if addr.args.is_empty() {
        dir.join(&addr.name)
    } else {
        dir.join(format!("{}_{}", addr.name, addr.hash_str()))
    }
}

/// One live invocation's sandbox: the directory, and the held `.owner` lock
/// that says so.
///
/// [`teardown`](Self::teardown) removes it synchronously. Dropped without one
/// — the invoking future was abandoned — it queues the same removal on the
/// cleaner's reclaim lane, the `.owner` fd moving into the job, when it was
/// created with a pending counter; without one (tests) it only releases the
/// lock, leaving the tree for the next sweep exactly as a SIGKILL would.
#[derive(Debug)]
pub(crate) struct InvokeSandbox {
    dir: PathBuf,
    /// The `flock`ed `.owner`, held, never read; `None` once torn down.
    owner: Option<File>,
    pending: Option<crate::engine::sandbox_cleaner::PendingCounter>,
}

impl InvokeSandbox {
    /// Create a fresh sandbox under `parent`, exclusively. `pending` is the
    /// request's background counter, for the drop path's queued removal.
    pub(crate) fn create(
        parent: &Path,
        pending: Option<crate::engine::sandbox_cleaner::PendingCounter>,
    ) -> anyhow::Result<Self> {
        let mut sbx = Self::create_with_names(
            parent,
            std::iter::repeat_with(|| uuid::Uuid::new_v4().simple().to_string()),
        )?;
        sbx.pending = pending;
        Ok(sbx)
    }

    /// [`create`](Self::create) with the candidate names given, so a test can
    /// pre-create one.
    fn create_with_names(
        parent: &Path,
        names: impl IntoIterator<Item = String>,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create invocation sandbox parent {}", parent.display()))?;
        for name in names.into_iter().take(CREATE_ATTEMPTS) {
            let dir = parent.join(&name);
            match std::fs::create_dir(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("create invocation sandbox {}", dir.display()));
                }
            }
            let owner = match claim(&dir) {
                Ok(owner) => owner,
                // A sweeper took the fresh `.owner-pending` for a crash's
                // leftover in the instant before we locked it: it is removing
                // the directory (`WouldBlock`), or already has, under the open
                // or the rename (`NotFound`). Take another name.
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::NotFound
                    ) =>
                {
                    continue;
                }
                Err(e) => {
                    // Ours, and nobody else's yet: leave nothing behind.
                    drop(hcore::fsutil::remove_dir_all(&dir));
                    return Err(e)
                        .with_context(|| format!("claim invocation sandbox {}", dir.display()));
                }
            };
            return Ok(Self {
                dir,
                owner: Some(owner),
                pending: None,
            });
        }
        anyhow::bail!(
            "could not create a fresh invocation sandbox under {}: {CREATE_ATTEMPTS} names were \
             all taken",
            parent.display()
        )
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Remove the sandbox, holding `.owner` until the tree is gone. Blocking;
    /// call it through `hcore::blocking`.
    pub(crate) fn teardown(mut self) -> io::Result<()> {
        let owner = self.owner.take();
        let res = remove(&self.dir);
        drop(owner);
        res
    }
}

impl Drop for InvokeSandbox {
    fn drop(&mut self) {
        let (Some(owner), Some(pending)) = (self.owner.take(), self.pending.take()) else {
            return;
        };
        let dir = self.dir.clone();
        crate::engine::sandbox_cleaner::enqueue(
            crate::engine::sandbox_cleaner::Lane::Reclaim,
            format!("reclaim abandoned invocation sandbox {}", dir.display()),
            Box::new(move || {
                let res = remove(&dir);
                drop(owner);
                res
            }),
            pending,
        );
    }
}

/// Remove an owned invocation sandbox, with an already-gone tree a success.
///
/// The lock file goes last: a removal that fails partway (EACCES on something
/// the tool left, EBUSY) leaves the tree with its `.owner` — or its
/// `.owner-pending` — still there, so the next sweep finds it eligible once
/// the lock is free, instead of an owner-less tree no sweep may touch. A
/// directory the tool made read-only is made writable and the removal retried
/// once, as `hcore::fsutil::remove_dir_all` does.
fn remove(dir: &Path) -> io::Result<()> {
    match remove_owner_last(dir) {
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            hcore::fsutil::make_readwrite_tree(dir);
            remove_owner_last(dir)
        }
        other => other,
    }
}

fn remove_owner_last(dir: &Path) -> io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if name == OWNER || name == OWNER_PENDING {
            continue;
        }
        let path = entry.path();
        // The entry's own type: a symlink is unlinked, never followed.
        let res = if entry.file_type()?.is_dir() {
            hcore::fsutil::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match res {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    for lock in [OWNER, OWNER_PENDING] {
        match std::fs::remove_file(dir.join(lock)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    match std::fs::remove_dir(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Create `.owner-pending` in the fresh `dir`, lock it, and only then name it
/// `.owner`: no sweeper ever sees an `.owner` that is not held.
fn claim(dir: &Path) -> io::Result<File> {
    let pending = dir.join(OWNER_PENDING);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&pending)?;
    if !try_lock_exclusive(&file)? {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "a just-created owner file is already locked",
        ));
    }
    std::fs::rename(&pending, dir.join(OWNER))?;
    Ok(file)
}

/// `flock(LOCK_EX | LOCK_NB)`: `Ok(false)` when another open file description
/// holds it, in this process or another.
fn try_lock_exclusive(file: &File) -> io::Result<bool> {
    // SAFETY: `file` owns a valid open fd for the duration of the call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(err)
    }
}

/// Remove every invocation sandbox directly under `parent` whose `.owner` lock
/// is free: what a SIGKILLed heph left. Returns how many went. Best-effort: a
/// failure is logged and the entry skipped.
pub(crate) fn sweep(parent: &Path) -> usize {
    let entries = match std::fs::read_dir(parent) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return 0,
        Err(e) => {
            tracing::warn!(error = %e, dir = %parent.display(), "invocation sweep: read dir");
            return 0;
        }
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        // `.owner-pending`-era names and anything hidden are never candidates.
        if entry.file_name().as_encoded_bytes().starts_with(b".") {
            continue;
        }
        // `file_type` is the entry's own type: a symlink is never followed.
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let dir = entry.path();
        match sweep_one(&dir) {
            Ok(true) => removed += 1,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(error = %e, dir = %dir.display(), "invocation sweep: remove");
            }
        }
    }
    removed
}

/// Open `dir/name` as a lock file: `None` when it is absent or not a plain
/// file (a symlink is never followed).
fn open_lock(dir: &Path, name: &str) -> io::Result<Option<File>> {
    match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir.join(name))
    {
        Ok(f) if f.metadata()?.is_file() => Ok(Some(f)),
        Ok(_) => Ok(None),
        Err(e)
            if e.kind() == io::ErrorKind::NotFound
                || e.raw_os_error() == Some(libc::ELOOP)
                || e.raw_os_error() == Some(libc::ENOTDIR) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// Whether `dir` is an invocation sandbox, live or not: it has a lock file.
fn has_lock_file(dir: &Path) -> bool {
    [OWNER, OWNER_PENDING]
        .iter()
        .any(|name| std::fs::symlink_metadata(dir.join(name)).is_ok_and(|m| m.is_file()))
}

/// Remove `dir` if it has an `.owner` nobody holds — or, failing that, an
/// `.owner-pending` nobody holds: a crash between the lock and the rename.
/// A creator that has made `.owner-pending` but not locked it yet loses the
/// race to the lock and takes another name. `Ok(false)` when it is not
/// eligible or is live.
fn sweep_one(dir: &Path) -> io::Result<bool> {
    let lock = match open_lock(dir, OWNER)? {
        Some(owner) => owner,
        None => match open_lock(dir, OWNER_PENDING)? {
            Some(pending) => pending,
            // No lock file: not an invocation sandbox this sweep may touch.
            None => return Ok(false),
        },
    };
    if !try_lock_exclusive(&lock)? {
        return Ok(false);
    }
    // Held across the removal, as the owner's own teardown holds it.
    match remove(dir) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// `heph gc`'s sweep: every invocation sandbox under `sandbox_root` whose lock
/// is free. Walks package directories only — never into a build's
/// `__target_*` sandbox, never into an invocation's own tree, never through a
/// symlink.
pub(crate) fn sweep_all(sandbox_root: &Path) -> usize {
    let mut removed = 0;
    let mut stack = vec![sandbox_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = entry.file_name();
            let name = name.as_encoded_bytes();
            if name.starts_with(BUILD_SANDBOX_PREFIX.as_bytes()) || name.starts_with(b".") {
                continue;
            }
            let path = entry.path();
            // A live invocation's own tree (what the sweep below left) is
            // never walked: it is the tool's, and can be arbitrarily deep.
            if has_lock_file(&path) {
                continue;
            }
            if name == INVOKE_DIR.as_bytes() {
                // `__invoke/<name>/<random>`: sweep each target's siblings. A
                // package component that happens to be named `__invoke` is
                // walked too: its subdirectories have no `.owner`, so the
                // sweep leaves them, and their packages are descended into.
                if let Ok(targets) = std::fs::read_dir(&path) {
                    for t in targets.flatten() {
                        if t.file_type().is_ok_and(|ft| ft.is_dir())
                            && !t
                                .file_name()
                                .as_encoded_bytes()
                                .starts_with(BUILD_SANDBOX_PREFIX.as_bytes())
                        {
                            removed += sweep(&t.path());
                        }
                    }
                }
            }
            stack.push(path);
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pre-created name is skipped, not reused; any other create error fails
    /// at once rather than retrying (R11).
    #[test]
    fn invoke_sandbox_name_collision_retries() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let parent = tmp.path().join("p");
        std::fs::create_dir_all(parent.join("taken"))?;
        std::fs::write(parent.join("taken/mine"), b"someone else's")?;

        let sbx = InvokeSandbox::create_with_names(&parent, ["taken", "fresh"].map(String::from))?;
        assert_eq!(sbx.dir(), parent.join("fresh"));
        assert!(sbx.dir().join(OWNER).is_file(), "owned");
        assert!(!sbx.dir().join(OWNER_PENDING).exists());
        assert!(
            parent.join("taken/mine").exists(),
            "the taken one is untouched"
        );

        let all_taken = InvokeSandbox::create_with_names(&parent, ["taken"].map(String::from));
        assert!(all_taken.is_err(), "exhausted names fail");

        // ENAMETOOLONG is not AlreadyExists: no retry, a named error.
        let err =
            InvokeSandbox::create_with_names(&parent, ["x".repeat(300)]).expect_err("too long");
        assert!(
            format!("{err:#}").contains("create invocation sandbox"),
            "{err:#}"
        );

        // A name near NAME_MAX still works.
        let long = InvokeSandbox::create_with_names(&parent, ["y".repeat(250)])?;
        long.teardown()?;

        sbx.teardown()?;
        assert!(!parent.join("fresh").exists(), "teardown removes it");
        Ok(())
    }

    /// A sweeper running while invocations create their sandboxes never fails
    /// a create: whether it took the fresh `.owner-pending` before the creator
    /// locked it (`WouldBlock`) or had already removed the directory
    /// (`NotFound` from the open or the rename), the creator takes another
    /// name. 16 concurrent invocations hit the second case about 1 in 120.
    #[test]
    fn create_survives_a_concurrent_sweep() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let parent = tmp.path().join("p");
        std::fs::create_dir_all(&parent)?;
        let done = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| -> anyhow::Result<()> {
            s.spawn(|| {
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    sweep(&parent);
                }
            });
            let creators: Vec<_> = (0..8)
                .map(|_| {
                    s.spawn(|| -> anyhow::Result<()> {
                        for _ in 0..300 {
                            InvokeSandbox::create(&parent, None)?.teardown()?;
                        }
                        Ok(())
                    })
                })
                .collect();
            let results: Vec<_> = creators.into_iter().map(|h| h.join()).collect();
            done.store(true, std::sync::atomic::Ordering::Relaxed);
            for r in results {
                r.map_err(|p| anyhow::anyhow!("creator panicked: {p:?}"))??;
            }
            Ok(())
        })
    }

    /// The env var under which the test re-execs itself as a lock holder.
    const HOLDER: &str = "HEPH_TEST_INVOKE_OWNER_HOLDER";

    /// A crash leftover is swept; a live one — held in this process or in
    /// another — is not; nothing without `.owner` or behind a symlink is
    /// touched (R11's adversarial rows).
    #[test]
    fn stale_invoke_sandbox_swept() -> anyhow::Result<()> {
        // The cross-process holder: lock `.owner`, say so, wait for EOF.
        if let Some(path) = std::env::var_os(HOLDER) {
            let f = std::fs::OpenOptions::new().read(true).open(&path)?;
            assert!(try_lock_exclusive(&f)?, "the holder locks");
            // libtest prints `test <name> ... ` without a newline first, so
            // the token is searched for, not matched as a whole line.
            println!("\nheph-owner-locked");
            let mut sink = String::new();
            drop(std::io::stdin().read_line(&mut sink));
            std::process::exit(0);
        }

        let tmp = tempfile::tempdir()?;
        let parent = tmp.path().join("sandbox/pkg/__invoke/t");

        // A crash leftover: owned, then the owner went away. Read-only
        // contents included.
        let dead = InvokeSandbox::create(&parent, None)?;
        let dead_dir = dead.dir().to_path_buf();
        std::fs::create_dir_all(dead_dir.join("ws/ro"))?;
        std::fs::write(dead_dir.join("ws/ro/f"), b"x")?;
        std::fs::set_permissions(
            dead_dir.join("ws/ro"),
            std::os::unix::fs::PermissionsExt::from_mode(0o555),
        )?;
        drop(dead);

        // Live in this process.
        let live = InvokeSandbox::create(&parent, None)?;

        // No lock file at all; a crash between the lock and the rename (an
        // unlocked `.owner-pending`, swept); a create in that window, live.
        let bare = parent.join("bare");
        std::fs::create_dir_all(&bare)?;
        let pending = parent.join("pending");
        std::fs::create_dir_all(&pending)?;
        std::fs::write(pending.join(OWNER_PENDING), b"")?;
        let pending_live = parent.join("pending-live");
        std::fs::create_dir_all(&pending_live)?;
        std::fs::write(pending_live.join(OWNER_PENDING), b"")?;
        let pending_lock = std::fs::File::open(pending_live.join(OWNER_PENDING))?;
        assert!(try_lock_exclusive(&pending_lock)?);

        // `.owner` as a symlink to a free lock file elsewhere.
        let linked = parent.join("linked");
        std::fs::create_dir_all(&linked)?;
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::write(&elsewhere, b"")?;
        std::os::unix::fs::symlink(&elsewhere, linked.join(OWNER))?;

        // A symlinked sandbox dir pointing at a free, owned tree elsewhere.
        let victim = tmp.path().join("victim");
        std::fs::create_dir_all(&victim)?;
        std::fs::write(victim.join(OWNER), b"")?;
        std::os::unix::fs::symlink(&victim, parent.join("via-symlink"))?;

        // Live in another process.
        let other = InvokeSandbox::create(&parent, None)?;
        let other_dir = other.dir().to_path_buf();
        drop(other);
        let mut holder = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "engine::invoke_sandbox::tests::stale_invoke_sandbox_swept",
            ])
            .args(["--nocapture", "--test-threads=1"])
            .env(HOLDER, other_dir.join(OWNER))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        {
            use std::io::BufRead as _;
            let out = holder.stdout.take().context("holder stdout")?;
            // Read off-thread, bounded: a holder that never locks must fail
            // the test, not hang it.
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut lines = std::io::BufReader::new(out).lines();
                let locked =
                    lines.any(|l| l.as_ref().is_ok_and(|l| l.contains("heph-owner-locked")));
                // A gone receiver already failed the test.
                let _sent = tx.send(locked);
            });
            let locked = rx.recv_timeout(std::time::Duration::from_secs(60));
            if locked != Ok(true) {
                drop(holder.kill());
                panic!("the holder never took the lock: {locked:?}");
            }
        }

        assert_eq!(sweep(&parent), 2, "only the crash leftovers go");
        assert!(!dead_dir.exists());
        assert!(
            !pending.exists(),
            "an unlocked `.owner-pending` is a leftover"
        );
        for kept in [live.dir(), &bare, &pending_live, &linked, &other_dir] {
            assert!(kept.exists(), "{} kept", kept.display());
        }
        assert!(victim.join(OWNER).exists(), "a symlink is never followed");

        // Released, in each process, they go.
        drop(holder.stdin.take());
        holder.wait()?;
        let live_dir = live.dir().to_path_buf();
        drop(live);
        drop(pending_lock);
        assert_eq!(sweep(&parent), 3);
        assert!(!live_dir.exists() && !other_dir.exists() && !pending_live.exists());
        assert!(bare.exists() && linked.exists());
        Ok(())
    }

    /// `heph gc`'s walk finds leftovers in any package, steps over a build's
    /// sandbox and over a package component named `__invoke`'s own targets.
    #[test]
    fn sweep_all_finds_every_package() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().join("sandbox");
        let a = InvokeSandbox::create(&root.join("a/__invoke/t"), None)?;
        let b = InvokeSandbox::create(&root.join("a/b/__invoke/u"), None)?;
        // A package named `__invoke`, with a build sandbox in it.
        let build = root.join("a/__invoke/__target_x");
        std::fs::create_dir_all(build.join("ws"))?;
        std::fs::write(build.join(OWNER), b"")?;
        // An invocation sandbox inside a build sandbox is not one.
        let inside = root.join("a/__target_y/__invoke/v/z");
        std::fs::create_dir_all(&inside)?;
        std::fs::write(inside.join(OWNER), b"")?;
        // A live invocation whose tool built what looks like a leftover in its
        // own tree: the walk never descends into a live invocation.
        let live = InvokeSandbox::create(&root.join("c/__invoke/w"), None)?;
        let nested = live.dir().join("ws/c/__invoke/n/q");
        std::fs::create_dir_all(&nested)?;
        std::fs::write(nested.join(OWNER), b"")?;
        let (a_dir, b_dir) = (a.dir().to_path_buf(), b.dir().to_path_buf());
        drop((a, b));
        assert_eq!(sweep_all(&root), 2);
        assert!(!a_dir.exists() && !b_dir.exists());
        assert!(inside.exists(), "a build sandbox is never walked into");
        assert!(
            nested.exists(),
            "a live invocation's tree is never walked into"
        );
        live.teardown()?;
        Ok(())
    }
}
