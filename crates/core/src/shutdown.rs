//! In-process shutdown signal shared by the SIGINT listener (engine/bin) and the
//! TUI's Ctrl+C handler. Lives here (the lowest crate) so the TUI can hold a
//! `ShutdownTrigger` without depending on the bin's bootstrap module.

use std::sync::atomic::{AtomicBool, Ordering};
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
        if self.suppressed.load(Ordering::Acquire) {
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
