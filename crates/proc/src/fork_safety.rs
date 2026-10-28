//! Make a `fork` from a multi-threaded process safe on macOS.
//!
//! macOS's libSystem runs `atfork` child handlers in every forked child, and
//! libnotify's (`_notify_fork_child`) lazily initializes its process globals
//! through `os_alloc_once`. When those globals were never initialized in the
//! parent and another thread is inside that initializer at the moment of the
//! `fork`, the child inherits a once-gate owned by a thread that does not
//! exist in it, and libplatform aborts the child
//! (`_os_once_gate_corruption_abort`, reported as `EXC_BREAKPOINT` / SIGKILL)
//! before it reaches `exec`. The parent then reads EOF on std's exec-error
//! pipe and reports a *successful* spawn of a process that has already died
//! of SIGKILL — a missing program "spawns", a real one is "killed by signal 9".
//!
//! [`prime`] initializes those globals once, before the first `fork` this
//! crate (or `execrunner`) performs, so the gate is in its final state by the
//! time any child copies it. Every spawn that goes through `Command::pre_exec`
//! (which forces `fork` rather than `posix_spawn`) calls it first. Linux has
//! no such handler; there it is a no-op.

/// Initialize the libSystem state a forked child's `atfork` handlers lazily
/// set up, exactly once per process. Cheap after the first call (one atomic
/// load). Call it before any `fork`: a `Command::spawn` with a `pre_exec`
/// closure.
pub fn prime() {
    #[cfg(target_os = "macos")]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            // Any libnotify entry point reaches `_notify_globals()`, the
            // `os_alloc_once` that `_notify_fork_child` would otherwise be
            // the first to run. Asking about an invalid token has no effect.
            // SAFETY: a pure query on an integer; libnotify is part of
            // libSystem, which every macOS process links.
            unsafe {
                notify_is_valid_token(-1);
            }
        });
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn notify_is_valid_token(val: libc::c_int) -> bool;
}
