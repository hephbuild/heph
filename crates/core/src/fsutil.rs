//! Small filesystem helpers shared across crates.

use anyhow::Context;
use std::fs;
use std::io;
use std::path::Path;

/// Close `file` (a freshly-written, writable handle to `path`) in a way that
/// guarantees no writable descriptor for the file survives anywhere, then
/// return. Call this on any file that is about to be `exec`'d.
///
/// Works around <https://github.com/rust-lang/rust/issues/114554>. `File` sets
/// `O_CLOEXEC`, but that only closes the descriptor at `execve` — so if another
/// thread `fork`s between our `File::create` and a later `exec` of this file,
/// the child holds an inherited writable fd for the whole `fork`→`execve`
/// window, and any `exec` of the file during it fails with `ETXTBSY`. Closing
/// our own handle is therefore not enough: the racing child's copy is the one
/// that blocks the exec, and we cannot see it.
///
/// `flock` locks are held by the *open file description*, which a forked child
/// shares rather than copies, so a lock is the one handle we do have on that
/// invisible fd:
///
/// 1. take an exclusive lock on the writable fd — any forked child's copy of
///    the description now holds it too,
/// 2. close our fd (the lock lives on in every inherited copy),
/// 3. reopen read-only and take a shared lock — this blocks until the last
///    writable description is gone, i.e. until every racing child has reached
///    `execve` and dropped its `O_CLOEXEC` copy.
///
/// On return no writable description *derived from `file`* survives, and since
/// ours is closed no later `fork` can create one, so a subsequent `exec` cannot
/// see `ETXTBSY`.
///
/// Limits, in decreasing likelihood of mattering:
///
/// - **An independent writer is not covered.** Another thread or process that
///   opened the same path itself holds none of our lock and is invisible to the
///   shared acquire. Callers must not race their own writes against a path they
///   are about to exec.
/// - **`file` and `path` must name the same inode.** The reopen is by path, so
///   if anything renames over `path` in the window the barrier drains a
///   different inode and passes vacuously.
/// - **On NFS the barrier can silently degrade to a no-op.** Since Linux
///   2.6.37 an NFS client emulates `flock` as a whole-file POSIX record lock
///   unless mounted `-o local_lock=flock`, and record locks are process-owned:
///   closing our fd drops the exclusive lock, so the shared acquire returns
///   immediately even while a racing child holds a writable fd. Local
///   filesystems — where heph puts its cache, sandboxes and stage — are
///   unaffected.
///
/// Ports the Go tree's `xfs.CloseEnsureROFD`. Costs two `flock`s and one
/// `open` per executable file, so callers gate it on `+x` rather than paying
/// it for every write. Prefer [`write_executable`] over calling this directly:
/// it is the forgettable last step of a four-step sequence, and the flake this
/// exists to prevent came from a caller that did the first three.
pub fn close_ensure_ro_fd(file: fs::File, path: &Path) -> anyhow::Result<()> {
    // `File::lock`/`lock_shared` do not retry on `EINTR`. Unreachable today —
    // `flock` is restartable and the tree's only handler (signal-hook, via
    // `tokio::signal::ctrl_c`) installs `SA_RESTART` — but a hand-rolled
    // `sigaction` without it would turn this into a spurious failure.
    file.lock()
        .with_context(|| format!("flock(exclusive) writable fd {:?}", path))?;
    drop(file);

    let ro = fs::File::open(path).with_context(|| format!("reopen {:?} read-only", path))?;
    // Blocking, with no timeout and no cancellation. The wait is bounded by a
    // racing child's fork→execve — microseconds — because every heph child is
    // spawned with `Command` (fork+exec) and a grandchild spawned by the exec'd
    // program never inherits the fd. Keep it that way: a caller that hands this
    // an fd some long-lived child holds open turns it into a silent hang.
    ro.lock_shared()
        .with_context(|| format!("flock(shared) read-only fd {:?}", path))?;
    drop(ro);
    Ok(())
}

/// Write `bytes` to `path` as an executable (`0o755`) file that is safe to
/// `exec` the moment this returns.
///
/// Exists so the ordering cannot be got wrong. The sequence is create, write,
/// chmod, then drain writable descriptors via [`close_ensure_ro_fd`] — and the
/// last step is the one that gets forgotten, which is exactly how the `ETXTBSY`
/// flake this guards against was introduced. Callers that stream rather than
/// buffer (`unpack`) call the barrier directly; everyone else should use this.
pub fn write_executable(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write as _;

    let mut file = fs::File::create(path).with_context(|| format!("create {:?}", path))?;
    file.write_all(bytes)
        .with_context(|| format!("write {} bytes to {:?}", bytes.len(), path))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        // fchmod on the handle we hold, so no window where the path is
        // executable but still open for writing by someone we cannot see.
        file.set_permissions(fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod +x {:?}", path))?;
    }
    close_ensure_ro_fd(file, path)
}

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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::io::AsRawFd as _;
    use std::os::unix::process::CommandExt as _;
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;

    /// Long enough that a barrier which never blocks cannot accidentally exceed
    /// it (the pre-fix `drop` returns in microseconds), short enough to be free.
    const STILL_BLOCKED: Duration = Duration::from_millis(200);
    /// Ceiling for the barrier to notice the fd is gone. Only a hang trips it.
    const RELEASE: Duration = Duration::from_secs(10);

    fn write_script(path: &Path) -> fs::File {
        let mut file = fs::File::create(path).expect("create");
        file.write_all(b"#!/bin/sh\necho ok\n").expect("write");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod +x");
        file
    }

    /// Spawn a child holding a *duplicate* of `file`'s descriptor — which is
    /// precisely what a `fork` racing our write leaves behind: `dup` clears
    /// `O_CLOEXEC` and shares the open file description, so the child keeps a
    /// writable fd on the file across its own `execve`.
    ///
    /// The child blocks on `read`, a shell builtin, so nothing is looked up on
    /// `$PATH`; dropping the returned stdin closes the pipe and lets it exit.
    /// That makes the release causal rather than a wall-clock race.
    fn spawn_fd_holder(file: &fs::File) -> (Child, ChildStdin) {
        let raw = file.as_raw_fd();
        // SAFETY: pre_exec runs between fork and exec; only async-signal-safe
        // syscalls (`dup`) are invoked, and the error path uses
        // `Error::last_os_error` (`from_raw_os_error`), which does not allocate.
        #[expect(
            clippy::multiple_unsafe_ops_per_block,
            reason = "`pre_exec` and the `dup` inside its closure cannot be split in place: the closure body inherits the enclosing unsafe context, so a nested `unsafe` around the `dup` is `unused_unsafe`. The SAFETY comment above covers both operations."
        )]
        let mut child = unsafe {
            Command::new("/bin/sh")
                .arg("-c")
                .arg("read x")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .pre_exec(move || {
                    if libc::dup(raw) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                })
                .spawn()
        }
        .expect("spawn fd holder");
        let stdin = child.stdin.take().expect("holder stdin");
        (child, stdin)
    }

    /// The `fork`/`exec` race, made deterministic. While a child holds an
    /// inherited writable fd the barrier must not return; once the fd is gone
    /// it must, and the file must then be exec'able.
    ///
    /// The assertion is the platform-independent half of the contract ("no
    /// writable description survives the barrier") rather than `ETXTBSY`, which
    /// only Linux enforces — so this one test discriminates on all three
    /// supported targets. Replacing the barrier with a plain `drop(file)`
    /// returns immediately, the first `recv_timeout` succeeds, and it fails.
    #[test]
    fn close_ensure_ro_fd_waits_for_a_forked_childs_writable_fd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("prog.sh");
        let file = write_script(&script);
        let (mut holder, holder_stdin) = spawn_fd_holder(&file);

        let (tx, rx) = mpsc::channel();
        let barrier = {
            let script = script.clone();
            std::thread::spawn(move || {
                let r = close_ensure_ro_fd(file, &script);
                let _ = tx.send(());
                r
            })
        };

        assert!(
            matches!(
                rx.recv_timeout(STILL_BLOCKED),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "close_ensure_ro_fd returned while a forked child still held a writable \
             fd for the file; an exec of it can still fail with ETXTBSY"
        );

        // Causal release: closing the pipe ends the holder, dropping the last
        // writable descriptor. The barrier must now — and only now — complete.
        drop(holder_stdin);
        holder.wait().expect("reap fd holder");
        rx.recv_timeout(RELEASE)
            .expect("barrier must return once the last writable fd is gone");
        barrier
            .join()
            .expect("barrier thread")
            .expect("barrier failed");

        let out = Command::new(&script).output().expect("exec");
        assert!(out.status.success(), "exec after barrier failed: {out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "ok\n");
    }

    /// The symptom the barrier exists to prevent, asserted where the kernel
    /// actually produces it: heph behaves identically on all three targets, but
    /// Linux alone enforces `ETXTBSY`, so only there can the untreated failure
    /// be observed at all.
    ///
    /// Scope, precisely: this pins that a live writable fd really does make
    /// `execve` fail and that draining it really does fix that. It does *not*
    /// isolate the forked child's contribution — this process still holds its
    /// own writable `file` during the first exec, and either fd alone is enough
    /// for `ETXTBSY`. Nor does it exercise the barrier's *wait*, since the
    /// holder is already reaped before it is called. Both of those are the
    /// preceding test's job; this one is about the errno.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_live_writable_fd_makes_exec_fail_until_the_barrier_clears_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("prog.sh");
        let file = write_script(&script);
        let (mut holder, holder_stdin) = spawn_fd_holder(&file);

        let err = Command::new(&script)
            .output()
            .expect_err("exec must fail while a writable fd for the file is open");
        assert_eq!(
            err.raw_os_error(),
            Some(libc::ETXTBSY),
            "expected ETXTBSY, got: {err}"
        );

        drop(holder_stdin);
        holder.wait().expect("reap fd holder");
        close_ensure_ro_fd(file, &script).expect("barrier");

        let out = Command::new(&script).output().expect("exec after barrier");
        assert!(out.status.success(), "exec after barrier failed: {out:?}");
    }

    /// `write_executable`'s own contract: the bytes land, the exec bit is set,
    /// and the result runs. Its barrier step is covered by the tests above —
    /// it cannot be re-asserted from out here, because the only descriptor the
    /// barrier drains is the one `write_executable` creates internally, and an
    /// independently-opened fd is (documented as) outside the guarantee.
    #[test]
    fn write_executable_writes_a_runnable_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("tool");

        write_executable(&script, b"#!/bin/sh\necho ok\n").expect("write_executable");

        let mode = fs::metadata(&script).expect("stat").permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "must be executable, mode={mode:o}");
        let out = Command::new(&script).output().expect("exec");
        assert!(out.status.success(), "exec failed: {out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "ok\n");
    }

    /// Overwriting an existing file must truncate, not leave a tail behind.
    #[test]
    fn write_executable_truncates_a_longer_previous_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("tool");

        write_executable(&script, b"#!/bin/sh\necho aaaaaaaaaaaaaaaaaaaa\n").expect("first");
        write_executable(&script, b"#!/bin/sh\necho ok\n").expect("second");

        let out = Command::new(&script).output().expect("exec");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "ok\n");
    }

    /// The barrier must fail loudly, naming the step, if the file it is asked to
    /// drain is gone. Silently tolerating this would turn it into a no-op and
    /// bring the ETXTBSY flake back with nothing going red.
    #[test]
    fn close_ensure_ro_fd_errors_when_the_file_vanishes_before_the_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("prog.sh");
        let file = write_script(&script);
        fs::remove_file(&script).expect("unlink");

        let err = close_ensure_ro_fd(file, &script).expect_err("must not succeed");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("reopen"),
            "error must name the step, got: {msg}"
        );
    }
}
