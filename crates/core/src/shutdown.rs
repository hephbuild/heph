//! In-process shutdown signal shared by the SIGINT listener (engine/bin) and the
//! TUI's Ctrl+C handler. Lives here (the lowest crate) so the TUI can hold a
//! `ShutdownTrigger` without depending on the bin's bootstrap module.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Producer-side handle for the in-process shutdown signal. Both the SIGINT
/// listener and the TUI's Ctrl+C key handler call `trigger()` on this; a single
/// consumer drives the shutdown state machine from the paired receiver.
///
/// While `SuppressionHandle::set(true)` is in effect, `trigger()` silently drops
/// presses — every press, including the second one that would otherwise abort.
/// It is therefore for one situation only: the user is typing at something heph
/// is hosting (`heph run --shell`) and the keystrokes are that session's, not
/// heph's. Anything broader is a Ctrl-C that does nothing at all, since a TUI
/// that has stepped aside is in cooked mode and the kernel SIGINT is then the
/// only producer left.
#[derive(Clone)]
pub struct ShutdownTrigger {
    tx: mpsc::UnboundedSender<()>,
    suppressed: Arc<AtomicBool>,
}

impl ShutdownTrigger {
    /// Create a trigger and its paired receiver (the single consumer).
    pub fn new() -> (Self, mpsc::UnboundedReceiver<()>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                tx,
                suppressed: Arc::new(AtomicBool::new(false)),
            },
            rx,
        )
    }

    pub fn trigger(&self) {
        if self.suppressed.load(Ordering::Acquire) || is_transparent() {
            return;
        }
        _ = self.tx.send(());
    }

    pub fn suppression(&self) -> SuppressionHandle {
        SuppressionHandle {
            flag: Arc::clone(&self.suppressed),
        }
    }
}

#[derive(Clone)]
pub struct SuppressionHandle {
    flag: Arc<AtomicBool>,
}

impl SuppressionHandle {
    pub fn set(&self, suppressed: bool) {
        self.flag.store(suppressed, Ordering::Release);
    }
}

/// How many [`TransparencyGuard`]s are alive in this process.
static TRANSPARENT: AtomicUsize = AtomicUsize::new(0);

/// While held, heph is *transparent*: a program it handed the terminal to (an
/// entrypoint, E8/N4a) owns every signal, and heph acts on none of them. The
/// shutdown trigger drops every press (one it accepted before the hold was
/// taken still cancels: it was pressed before the program existed), and the
/// diagnostic handlers (`SIGQUIT` dump, `SIGUSR1`
/// per-thread backtraces, `SIGUSR2` profile dump) do nothing.
///
/// Refcounted, so two invocations in one process never end each other's
/// transparency: it lasts while any guard is alive. Taken right before the
/// program is forked — before that, Ctrl-C cancels as on any run — and
/// normally kept until heph exits ([`TransparencyGuard::until_exit`]), so a
/// signal arriving while heph tears down after the program exits prints
/// nothing either.
#[derive(Debug)]
#[must_use = "transparency ends when the guard drops"]
pub struct TransparencyGuard(());

/// Become transparent until the returned guard drops (see
/// [`TransparencyGuard`]).
pub fn hold_transparent() -> TransparencyGuard {
    TRANSPARENT.fetch_add(1, Ordering::AcqRel);
    TransparencyGuard(())
}

/// Whether any [`TransparencyGuard`] is alive. One atomic load, so it is safe
/// to call from a signal handler.
pub fn is_transparent() -> bool {
    TRANSPARENT.load(Ordering::Acquire) > 0
}

impl TransparencyGuard {
    /// Keep the hold for the rest of the process's life: heph exits with the
    /// program's status once it ends, and nothing in between is heph's to act
    /// on.
    pub fn until_exit(self) {
        // Never dropped: the count stays up until the process is gone.
        let _held = std::mem::ManuallyDrop::new(self);
    }
}

impl Drop for TransparencyGuard {
    fn drop(&mut self) {
        TRANSPARENT.fetch_sub(1, Ordering::AcqRel);
    }
}

type RestoreFn = Arc<dyn Fn() + Send + Sync>;

/// Closure that puts the terminal back into cooked mode, registered by
/// whichever raw-mode session (the TUI) is currently active. The second
/// Ctrl-C hard-exits via `process::exit`, which runs no destructors — the
/// registered closure is the only thing that still runs on that path, so it
/// must be self-contained: no crossterm (whose own raw-mode mutex it may
/// already be holding), no writes when nothing was ever recorded (a
/// redirected/non-tty run never registers one).
static TERMINAL_RESTORE: Mutex<Option<RestoreFn>> = Mutex::new(None);

/// Register the closure the hard-abort path calls to restore the terminal.
/// Call once raw mode is entered, with a closure that already captured
/// whatever state (e.g. the pre-raw-mode `termios`) it needs to restore.
pub fn set_terminal_restore(f: impl Fn() + Send + Sync + 'static) {
    *TERMINAL_RESTORE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(f));
}

/// Forget the registered closure — call once the session has torn itself
/// down normally, so a later abort in some other (non-interactive) run
/// doesn't fire a stale restore.
pub fn clear_terminal_restore() {
    *TERMINAL_RESTORE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Invoke the registered restore closure, if any. A no-op when no raw-mode
/// session is active — safe to call unconditionally right before a hard
/// `process::exit`.
///
/// Clones the `Arc` and drops the lock before calling `f()`: `Mutex` is
/// non-reentrant, so invoking the closure while still holding the guard
/// would deadlock this same thread if `f` ever touched `TERMINAL_RESTORE`
/// itself (e.g. a future closure that re-arms the restore after running).
pub fn restore_terminal() {
    let f = TERMINAL_RESTORE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(f) = f {
        f();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    // Serialized: TERMINAL_RESTORE is a process-global static, so concurrent
    // test runs on the same binary would stomp each other's registration.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Two holders never end each other's transparency, and a press while
    /// either is held is dropped — the case a single bool got wrong: the
    /// first invocation to finish cleared it under the second.
    #[test]
    fn transparency_is_refcounted_and_drops_presses() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (trigger, mut rx) = ShutdownTrigger::new();
        assert!(!is_transparent());

        let first = hold_transparent();
        let second = hold_transparent();
        drop(first);
        assert!(is_transparent(), "the second holder still holds");
        trigger.trigger();
        assert!(rx.try_recv().is_err(), "a press while held is dropped");

        drop(second);
        assert!(!is_transparent());
        trigger.trigger();
        assert!(rx.try_recv().is_ok(), "presses count again once released");
    }

    #[test]
    fn restore_terminal_is_noop_when_nothing_registered() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_terminal_restore();
        // Must not panic and must not call anything — there is nothing to
        // observe here beyond "returns", which is the point: a
        // non-interactive run that never registered a closure pays nothing.
        restore_terminal();
    }

    #[test]
    fn restore_terminal_invokes_registered_closure() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        set_terminal_restore(move || {
            calls_clone.fetch_add(1, Ordering::SeqCst);
        });

        restore_terminal();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Idempotent: the hard-abort path only ever calls this once, but a
        // second call (e.g. a future caller) must not panic or double-free.
        restore_terminal();
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        clear_terminal_restore();
        restore_terminal();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "cleared closure must not fire"
        );
    }
}
