//! Sidecar supervisor process that reaps the entire descendant tree when the
//! main heph process dies, including hard-kill (SIGKILL/OOM) scenarios.
//!
//! See the design notes in this module's git history for the full rationale.
//! Summary: a small `__supervisor` subcommand of heph is spawned before heph's
//! first child (see [`init`] and [`ensure_started`] — not at startup, so a
//! command that spawns nothing never pays for it) and holds one end of an
//! `AF_UNIX` socketpair. Each driver-spawned child
//! enters its own session (`setsid` → `pid == pgid`) and is reported to the
//! supervisor via `TRACK <pgid>`. When the main process exits — for any
//! reason — the kernel closes the socket, the supervisor sees EOF, and it
//! `killpg(SIGKILL)`s every tracked pgid before exiting.
//!
//! Known race: between `Command::spawn()` returning and
//! [`ProcessTracker::track`] writing the `TRACK` line, a SIGKILL of the main
//! process leaves that one child unreaped. The window is microseconds; closing
//! it would require writing the pid from inside `pre_exec` over the inherited
//! socket. Deferred.
//!
//! A cdylib plugin is loaded into the host process but statically links its own
//! copy of this crate, so the `TRACKER` static it sees is not the one [`init`]
//! filled. The host hands it a [`SupervisorSink`] over the plugin ABI and the
//! plugin calls [`init_with_sink`], which routes its children's `TRACK` lines
//! through the host's socket. Without that, every plugin-spawned child (all of
//! plugin-go's compiles) misses the supervisor.
//!
//! Set `heph_DISABLE_REAPER=1` to bypass the sidecar — [`init`] becomes a
//! no-op and [`register_child`] returns `None`. The actual wait path lives
//! in [`crate::proc_exec`].

use anyhow::Context;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, OnceLock};

mod client;
mod protocol;
mod server;

pub use client::{ProcessTracker, SupervisorSink, TrackGuard};
pub use server::run_supervisor_main;

static TRACKER: OnceLock<Arc<ProcessTracker>> = OnceLock::new();

/// `heph_DISABLE_REAPER=1` short-circuits every supervisor entry point so
/// subprocess execution falls through to tokio's built-in waker. Cached in
/// a `OnceLock` because `std::env::var` takes a global libc mutex; consistent
/// with the `stall_threshold` / `cycle_detection_enabled` pattern in
/// `src/hmemoizer/mod.rs`.
fn reaper_disabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| matches!(std::env::var("heph_DISABLE_REAPER").as_deref(), Ok("1")))
}

/// Returns the process-global tracker handle. If [`init`] has not been called
/// (e.g. inside the supervisor child itself, or in unit tests) a no-op
/// tracker is returned whose `track`/`untrack` calls fail immediately.
pub fn tracker() -> Arc<ProcessTracker> {
    if let Some(t) = TRACKER.get() {
        return Arc::clone(t);
    }
    static NOOP: OnceLock<Arc<ProcessTracker>> = OnceLock::new();
    Arc::clone(NOOP.get_or_init(|| Arc::new(ProcessTracker::noop())))
}

/// Set up the supervisor sidecar and store its client handle in the global
/// tracker slot. Idempotent: subsequent calls return `Ok(())`.
///
/// This creates the socketpair but does **not** start the supervisor process:
/// that waits for [`ensure_started`] — called by `proc_exec` before every child
/// spawn — so a command that spawns nothing (`version`, `query`, a fully cached
/// `run`) never pays a fork+exec of the 70 MB binary.
///
/// Must still be called early — before any thread spawns its own children.
/// On macOS `socketpair` and the `CLOEXEC` that follows are two syscalls, and a
/// fork landing between them would leak the parent end into a child that could
/// then hold it open past heph's death, so the supervisor never saw EOF. Both
/// ends stay `CLOEXEC` from here on; the supervisor's end crosses into it by a
/// `dup2` inside its own spawn, never by clearing the flag in this process
/// where a concurrent fork on another thread could inherit it.
///
/// The binary's *path* is resolved here, but the file is only executed at the
/// first child. If it was replaced or unlinked in between (an upgrade landing
/// mid-run), that start runs the new file or fails with `ENOENT`, failing the
/// spawn that needed it. Accepted, and uniform across OSes — there is no
/// `/proc/self/exe` re-exec on Linux. See `client::Sidecar`.
pub fn init() -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("locate current_exe for supervisor")?;
    init_with_exe(exe)
}

/// [`init`] with the binary that serves `__supervisor` given explicitly — for a
/// test harness whose own executable is not heph.
pub fn init_with_exe(exe: std::path::PathBuf) -> anyhow::Result<()> {
    if reaper_disabled() {
        return Ok(());
    }
    if TRACKER.get().is_some() {
        return Ok(());
    }

    // Ignore SIGPIPE in the main process too, so a dead supervisor never
    // raises a signal when a driver writes a `TRACK` line.
    // SAFETY: signal(2) at startup, before driver threads run.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    let (parent, child) = UnixStream::pair().context("create supervisor socketpair")?;
    set_cloexec(parent.as_raw_fd(), true).context("set CLOEXEC on parent supervisor fd")?;
    set_cloexec(child.as_raw_fd(), true).context("set CLOEXEC on supervisor child fd")?;

    let sidecar = client::Sidecar::new(exe, std::os::fd::OwnedFd::from(child));
    let tracker = Arc::new(ProcessTracker::from_stream(parent, sidecar));
    TRACKER
        .set(tracker)
        .map_err(|_already_set| anyhow::anyhow!("process supervisor already initialised"))?;
    Ok(())
}

/// Start the supervisor process now if it is not running yet. Call before
/// creating anything that must not outlive heph: `proc_exec` calls it before
/// every child spawn, and the plugin loader before handing a cdylib the
/// supervisor sink (a cdylib's children report only *after* they exist, so its
/// supervisor must already be up).
///
/// A failure is not sticky: it is returned to every caller for a short backoff
/// window, then the next call tries again (see `client::Sidecar`).
///
/// A no-op when the reaper is disabled, when [`init`] never ran, and in a cdylib
/// (whose tracker forwards to the host's, which the host started).
pub fn ensure_started() -> std::io::Result<()> {
    if reaper_disabled() {
        return Ok(());
    }
    tracker()
        .ensure_started()
        .map_err(|e| std::io::Error::other(format!("{e:#}")))
}

/// The supervisor process's pid, if this process has started one — `None`
/// before the first child spawn (it starts lazily), with the reaper disabled,
/// and in a cdylib. For diagnostics and tests.
pub fn sidecar_pid() -> Option<i32> {
    TRACKER.get()?.sidecar_pid()
}

/// The fd number the supervisor receives its socket end on (the number after
/// stdio), unless the end already sits there.
const SIDECAR_IPC_FD: RawFd = 3;

/// Spawn `exe __supervisor --ipc-fd <N>` with `child_end` as fd `N` and stdio on
/// `/dev/null`, returning its pid.
///
/// `posix_spawn`, not `fork` + `pre_exec`: this now runs on whichever thread
/// spawns heph's first child, with the runtime's other threads live, and
/// `posix_spawn` neither duplicates the parent's address space nor runs any of
/// its code in the child. The `dup2` file action is what carries the socket
/// end across — the `dup2`'d copy has `CLOEXEC` clear in the child only — and
/// the signal mask is reset, since the calling worker's mask is not the main
/// thread's. The child is never waited on: it outlives this process by design,
/// and is reparented and reaped once heph is gone.
fn spawn_sidecar(exe: &std::path::Path, child_end: &std::os::fd::OwnedFd) -> anyhow::Result<i32> {
    use std::ffi::CString;
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

    let src = child_end.as_raw_fd();
    let target = if src == SIDECAR_IPC_FD {
        SIDECAR_IPC_FD + 1
    } else {
        SIDECAR_IPC_FD
    };

    let path = CString::new(exe.as_os_str().as_bytes()).context("supervisor path has a NUL")?;
    let args = [
        path.clone(),
        CString::from(c"__supervisor"),
        CString::from(c"--ipc-fd"),
        CString::new(target.to_string()).context("encoding supervisor fd")?,
    ];
    // The environment is passed through: the supervisor is this binary, and may
    // need what the host needed to load (`LD_LIBRARY_PATH` and the like).
    let env = std::env::vars_os()
        .map(|(k, v)| {
            let mut kv = k.into_vec();
            kv.push(b'=');
            kv.extend(v.into_vec());
            CString::new(kv)
        })
        .collect::<Result<Vec<_>, _>>()
        .context("environment entry has a NUL")?;
    let devnull = CString::from(c"/dev/null");
    let argv: Vec<*mut libc::c_char> = args
        .iter()
        .map(|a| a.as_ptr().cast_mut())
        .chain(std::iter::once(std::ptr::null_mut()))
        .collect();
    let envp: Vec<*mut libc::c_char> = env
        .iter()
        .map(|e| e.as_ptr().cast_mut())
        .chain(std::iter::once(std::ptr::null_mut()))
        .collect();

    let check = |rc: libc::c_int, what: &str| -> anyhow::Result<()> {
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(rc)).with_context(|| what.to_string())
        }
    };

    let mut actions = SpawnFileActions::new()?;
    let mut attr = SpawnAttr::new()?;
    let mut pid: libc::pid_t = 0;
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "the posix_spawn setup calls share the same initialised objects"
    )]
    // SAFETY: `actions` and `attr` are initialised (their constructors ran
    // `*_init`) and destroyed by their own Drop. Every pointer passed —
    // `devnull`, `path`, `argv`, `envp` and the strings they point into — is a
    // live, NUL-terminated buffer that outlives the `posix_spawn` call, and the
    // two arrays are NULL-terminated. `src` is an open fd we own.
    unsafe {
        check(
            libc::posix_spawn_file_actions_adddup2(&mut actions.0, src, target),
            "posix_spawn: dup2 the supervisor socket",
        )?;
        for (fd, flags) in [
            (0, libc::O_RDONLY),
            (1, libc::O_WRONLY),
            (2, libc::O_WRONLY),
        ] {
            check(
                libc::posix_spawn_file_actions_addopen(
                    &mut actions.0,
                    fd,
                    devnull.as_ptr(),
                    flags,
                    0,
                ),
                "posix_spawn: stdio to /dev/null",
            )?;
        }
        let mut empty: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut empty);
        check(
            libc::posix_spawnattr_setsigmask(&mut attr.0, &empty),
            "posix_spawn: signal mask",
        )?;
        check(
            libc::posix_spawnattr_setflags(&mut attr.0, libc::POSIX_SPAWN_SETSIGMASK as _),
            "posix_spawn: flags",
        )?;
        check(
            libc::posix_spawn(
                &mut pid,
                path.as_ptr(),
                &actions.0,
                &attr.0,
                argv.as_ptr(),
                envp.as_ptr(),
            ),
            "spawn supervisor sidecar",
        )?;
    }
    Ok(pid)
}

/// An initialised `posix_spawn_file_actions_t`, destroyed on drop.
struct SpawnFileActions(libc::posix_spawn_file_actions_t);

impl SpawnFileActions {
    fn new() -> anyhow::Result<Self> {
        // SAFETY: a zeroed value is a valid argument to `*_init`, which fully
        // initialises it (the type is a pointer on macOS, a struct on Linux).
        let mut raw: libc::posix_spawn_file_actions_t = unsafe { std::mem::zeroed() };
        // SAFETY: `raw` is writable and not yet initialised.
        let rc = unsafe { libc::posix_spawn_file_actions_init(&mut raw) };
        if rc != 0 {
            return Err(std::io::Error::from_raw_os_error(rc))
                .context("posix_spawn_file_actions_init");
        }
        Ok(Self(raw))
    }
}

impl Drop for SpawnFileActions {
    fn drop(&mut self) {
        // SAFETY: initialised by `new`, destroyed exactly once here.
        unsafe { libc::posix_spawn_file_actions_destroy(&mut self.0) };
    }
}

/// An initialised `posix_spawnattr_t`, destroyed on drop.
struct SpawnAttr(libc::posix_spawnattr_t);

impl SpawnAttr {
    fn new() -> anyhow::Result<Self> {
        // SAFETY: as for `SpawnFileActions::new`.
        let mut raw: libc::posix_spawnattr_t = unsafe { std::mem::zeroed() };
        // SAFETY: `raw` is writable and not yet initialised.
        let rc = unsafe { libc::posix_spawnattr_init(&mut raw) };
        if rc != 0 {
            return Err(std::io::Error::from_raw_os_error(rc)).context("posix_spawnattr_init");
        }
        Ok(Self(raw))
    }
}

impl Drop for SpawnAttr {
    fn drop(&mut self) {
        // SAFETY: initialised by `new`, destroyed exactly once here.
        unsafe { libc::posix_spawnattr_destroy(&mut self.0) };
    }
}

/// Adopt a host-provided [`SupervisorSink`] as this copy-of-the-crate's tracker.
///
/// A cdylib plugin is dlopen'd into the host process but statically links its own
/// `proc`, so the `TRACKER` static it sees is *not* the one [`init`] filled — its
/// children would silently miss the supervisor and leak on a hard kill of the
/// host. The host calls this across the plugin ABI right after load, wiring the
/// plugin's tracker to the host's socket-owning one.
///
/// Idempotent: a second call (or one made after [`init`] already ran in this
/// binary, i.e. the host itself) keeps the existing tracker. A no-op when the
/// reaper is disabled — there is nothing to forward to.
pub fn init_with_sink(sink: Box<dyn SupervisorSink>) {
    if reaper_disabled() {
        return;
    }
    // `set` is a no-op if a tracker is already installed (or if a concurrent init
    // wins the race): the loser's sink is dropped and the winner's tracker used.
    // Both reach the same supervisor.
    drop(TRACKER.set(Arc::new(ProcessTracker::from_sink(sink))));
}

/// Run `f` synchronously. On macOS, uses `block_in_place` so the calling
/// worker parks on a kernel condvar rather than yielding through tokio's
/// broken cross-thread waker (`mio::Waker` → `EVFILT_USER`). On Linux,
/// calls `f` inline — there's no waker reliability issue and the call
/// sites are short enough that yielding to the scheduler would add more
/// overhead than the inline blocking saves.
#[cfg(target_os = "macos")]
pub fn block_or_inline<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    if matches!(
        tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()),
        Ok(tokio::runtime::RuntimeFlavor::MultiThread)
    ) {
        tokio::task::block_in_place(f)
    } else {
        f()
    }
}

/// Linux variant: no waker reliability bug, so inline calls are fine.
#[cfg(not(target_os = "macos"))]
pub fn block_or_inline<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    f()
}

/// Send `signal` to a child by id. Always issues both `killpg` and `kill`:
///   - `killpg` reaches the whole tree if the child is a session leader
///     (i.e. it called `setsid` in `pre_exec` — e.g. pluginexec shell mode).
///   - `kill` covers drivers that did not `setsid` (plugingo, pluginnix).
///
/// PIDs are unique system-wide, so `killpg(pid)` is a safe no-op when `pid`
/// is not a group leader. `ESRCH` from either call is ignored.
fn signal_child(pid: i32, signal: libc::c_int) {
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "killpg + kill are paired best-effort delivery to the same pid"
    )]
    // SAFETY: signal via killpg/kill on a pid we spawned and own.
    unsafe {
        libc::killpg(pid, signal);
        libc::kill(pid, signal);
    }
}

/// Hard-kill a child (and its process group) with `SIGKILL`. Used for the
/// non-graceful paths: Handle drop, drain-thread escalation, supervisor
/// parent-death reap.
pub fn kill_child(pid: i32) {
    signal_child(pid, libc::SIGKILL);
}

/// Politely ask a child (and its process group) to stop with `SIGINT` —
/// the same signal a terminal Ctrl-C delivers. Used by the cancellation
/// path so well-behaved children can clean up before we escalate to
/// `kill_child` (`SIGKILL`) if they ignore it.
pub fn interrupt_child(pid: i32) {
    signal_child(pid, libc::SIGINT);
}

/// Register a freshly-spawned child's pgid with the supervisor, returning a
/// guard that untracks on drop. If the supervisor is unavailable (e.g. in
/// unit tests where [`init`] never ran), logs a warning and returns `None`
/// rather than failing the build — the caller still benefits from `kill_on_drop`
/// as a fallback.
pub fn register_child(pgid: i32) -> Option<TrackGuard> {
    if reaper_disabled() {
        return None;
    }
    let tracker = tracker();
    if let Err(e) = tracker.track(pgid) {
        tracing::warn!(pgid, error = %format!("{e:#}"), "child not registered with process supervisor");
        return None;
    }
    Some(TrackGuard::new(tracker, pgid))
}

/// Tell the supervisor about a sandboxfuse root so it can force-umount on
/// parent death. Best-effort: if the supervisor isn't running (reaper
/// disabled, init never called), this is a no-op. The mount still has the
/// in-process drop watchdog as a fallback.
pub fn register_fuse_root(root: std::path::PathBuf) {
    if reaper_disabled() {
        return;
    }
    let tracker = tracker();
    if let Err(e) = tracker.register_fuse_root(root.clone()) {
        tracing::warn!(
            root = ?root,
            error = %format!("{e:#}"),
            "fuse root not registered with process supervisor"
        );
    }
}

fn set_cloexec(fd: RawFd, on: bool) -> std::io::Result<()> {
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "F_GETFD and F_SETFD are paired, must share one unsafe context"
    )]
    // SAFETY: fcntl(F_GETFD/F_SETFD) on a fd we just created and own.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let new = if on {
            flags | libc::FD_CLOEXEC
        } else {
            flags & !libc::FD_CLOEXEC
        };
        if libc::fcntl(fd, libc::F_SETFD, new) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::os::fd::OwnedFd;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::time::{Duration, Instant};

    /// A stand-in for `heph __supervisor`: it reports the fd it was handed, then
    /// echoes everything it reads on that fd back down it, and exits on EOF —
    /// when the test drops its end. So a test can see the socket crossed into
    /// the supervisor on the fd its argv names.
    const FAKE_SIDECAR: &str = "#!/bin/sh\neval \"echo FD=$3 >&$3; exec cat <&$3 >&$3\"\n";

    fn write_fake_sidecar(path: &Path) {
        std::fs::write(path, FAKE_SIDECAR).expect("write fake sidecar");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake sidecar");
    }

    /// The fake sidecar's greeting, then one round trip through it.
    fn assert_sidecar_echoes(parent: &UnixStream, want_fd: RawFd) {
        let mut reader = BufReader::new(parent.try_clone().expect("clone parent end"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("read greeting");
        assert_eq!(line.trim_end(), format!("FD={want_fd}"));
        (&*parent).write_all(b"PING\n").expect("write ping");
        line.clear();
        reader.read_line(&mut line).expect("read echo");
        assert_eq!(line.trim_end(), "PING");
    }

    fn socketpair() -> (UnixStream, OwnedFd) {
        let (parent, child) = UnixStream::pair().expect("socketpair");
        set_cloexec(parent.as_raw_fd(), true).expect("cloexec parent");
        set_cloexec(child.as_raw_fd(), true).expect("cloexec child");
        // Set while the peer is open (macOS refuses it once the peer is gone),
        // so a sidecar that never got its end reads as EOF, not a hang.
        parent
            .set_read_timeout(Some(Duration::from_secs(15)))
            .expect("read timeout");
        (parent, OwnedFd::from(child))
    }

    /// A failed start fails the caller and is latched for the backoff window —
    /// a call inside it makes no new attempt even once the binary exists — and
    /// the first call after the window starts the supervisor. A second failure
    /// doubles the window. The socket end survives the failures: the supervisor
    /// that finally starts is reachable on it.
    #[test]
    fn a_failed_start_is_retried_after_a_backoff_not_latched_forever() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join("not-yet-heph");
        let (parent, child_end) = socketpair();
        let sidecar = client::Sidecar::new(exe.clone(), child_end);

        let t0 = Instant::now();
        let first = sidecar
            .ensure_started_at(t0)
            .expect_err("no binary at the path: the start must fail");
        assert!(
            format!("{first:#}").contains("spawn supervisor sidecar"),
            "{first:#}"
        );
        assert_eq!(sidecar.pid(), None);

        // The window has passed but the binary is still missing: a real second
        // attempt, which fails and doubles the window.
        let t1 = t0 + client::RETRY_INITIAL;
        sidecar
            .ensure_started_at(t1)
            .expect_err("still missing: the retry must fail");

        // Now the binary exists, but the doubled window has not passed: the
        // latched error comes back without an attempt.
        write_fake_sidecar(&exe);
        let latched = sidecar
            .ensure_started_at(t1 + client::RETRY_INITIAL + Duration::from_millis(50))
            .expect_err("inside the doubled window: no new attempt");
        assert!(
            format!("{latched:#}").contains("next attempt in"),
            "{latched:#}"
        );
        assert_eq!(sidecar.pid(), None, "nothing may start inside the window");

        sidecar
            .ensure_started_at(t1 + client::RETRY_INITIAL * 2)
            .expect("after the window the start is retried and succeeds");
        assert!(sidecar.pid().is_some_and(|p| p > 0));
        assert_sidecar_echoes(&parent, SIDECAR_IPC_FD);

        // Started is final: no further spawn, whatever the clock says.
        let pid = sidecar.pid();
        sidecar.ensure_started_at(t0).expect("already started");
        assert_eq!(sidecar.pid(), pid);
    }

    /// The backoff never grows past its cap, however many starts fail.
    #[test]
    fn the_retry_backoff_is_capped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join("never");
        let (_parent, child_end) = socketpair();
        let sidecar = client::Sidecar::new(exe.clone(), child_end);
        let mut now = Instant::now();
        for _ in 0..12 {
            sidecar
                .ensure_started_at(now)
                .expect_err("no binary: every attempt fails");
            now += client::RETRY_MAX;
        }
        // One cap past the last attempt is always enough for a real attempt —
        // which succeeds now that the binary exists.
        write_fake_sidecar(&exe);
        sidecar
            .ensure_started_at(now)
            .expect("the window is at most RETRY_MAX");
    }

    /// Env var naming the fake sidecar for
    /// [`spawn_sidecar_child_end_on_fd_3_child`]; unset under a normal run.
    const FD3_CHILD_ENV: &str = "HEPH_TEST_SIDECAR_FD3_CHILD";

    /// The child half of the test below: put the supervisor's socket end on fd 3
    /// itself — the fd the supervisor is told to read — and spawn. A `dup2` of
    /// fd 3 onto fd 3 would leave `CLOEXEC` set and the end would vanish at
    /// exec, so `spawn_sidecar` must move it to fd 4. Runs alone in its own
    /// process: claiming fd 3 would clobber whatever another test held there.
    #[test]
    fn spawn_sidecar_child_end_on_fd_3_child() {
        let Some(exe) = std::env::var_os(FD3_CHILD_ENV) else {
            return;
        };
        let (parent, child_end) = socketpair();
        // In a fresh process the pair itself lands on fds 3 and 4, so move the
        // parent end clear of fd 3 before claiming it.
        // SAFETY: F_DUPFD_CLOEXEC on an open fd we own; the result is a new fd
        // owned by `parent` alone.
        let moved = unsafe { libc::fcntl(parent.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
        assert!(moved >= 10, "move the parent end off fd 3");
        drop(parent);
        // SAFETY: `moved` is a fresh, open fd owned by nothing else.
        let parent = unsafe { <UnixStream as std::os::fd::FromRawFd>::from_raw_fd(moved) };
        // SAFETY: dup2 onto fd 3 in a process running only this test; the new
        // fd is then owned by `on_3` alone.
        let rc = unsafe { libc::dup2(child_end.as_raw_fd(), SIDECAR_IPC_FD) };
        assert_eq!(rc, SIDECAR_IPC_FD, "dup2 onto fd 3");
        drop(child_end);
        // SAFETY: fd 3 is open (dup2 just made it) and owned by nothing else.
        let on_3 = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(SIDECAR_IPC_FD) };
        set_cloexec(on_3.as_raw_fd(), true).expect("cloexec fd 3");

        let pid = spawn_sidecar(Path::new(&exe), &on_3).expect("spawn with the end on fd 3");
        assert!(pid > 0);
        drop(on_3);
        assert_sidecar_echoes(&parent, SIDECAR_IPC_FD + 1);
        println!("FD3_CASE_DONE");
    }

    #[test]
    fn spawn_sidecar_moves_a_child_end_already_on_fd_3() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join("fake-sidecar");
        write_fake_sidecar(&exe);
        let out = std::process::Command::new(std::env::current_exe().expect("current_exe"))
            .args([
                "--exact",
                "process_supervisor::tests::spawn_sidecar_child_end_on_fd_3_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(FD3_CHILD_ENV, &exe)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run the fd 3 child");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success() && stdout.contains("FD3_CASE_DONE"),
            "fd 3 child failed ({}):\nstdout:\n{stdout}\nstderr:\n{stderr}",
            out.status
        );
    }
}
