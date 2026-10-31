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
/// handed back to [`tool_exited`] once the program exits, after which a
/// signal is heph's again but changes nothing about how heph exits (R27).
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

impl Drop for TransparencyGuard {
    fn drop(&mut self) {
        TRANSPARENT.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Set once a program heph handed the terminal to has exited (R27).
static AFTER_TOOL: AtomicBool = AtomicBool::new(false);

/// The program heph handed the terminal to has exited: end this invocation's
/// transparency (`guard`), and from now on, once no other program holds it,
/// heph is finishing up on the program's behalf — uploads, cleanup — and
/// exits with the program's status whatever happens.
///
/// Signals are heph's again (R27): Ctrl-C, TERM and HUP cancel what is
/// left, so heph exits promptly. Only one thing of R9 survives: such a late
/// signal prints nothing and never changes the exit status (no cancellation
/// box, no 130, no hard abort). Read with [`is_after_tool`].
pub fn tool_exited(guard: TransparencyGuard) {
    AFTER_TOOL.store(true, Ordering::Release);
    drop(guard);
}

/// Whether a program heph handed the terminal to has exited
/// ([`tool_exited`]). One atomic load: signal-handler safe.
pub fn is_after_tool() -> bool {
    AFTER_TOOL.load(Ordering::Acquire)
}

type RestoreFn = Arc<dyn Fn() + Send + Sync>;

/// Closure that puts the terminal back into cooked mode, registered by
/// whichever raw-mode session (the TUI) is currently active. The second
/// Ctrl-C hard-exits via `process::exit`, which runs no destructors — the
/// registered closure is the only thing that still runs on that path, so it
/// must be self-contained: no crossterm (whose own raw-mode mutex it may
/// already be holding), no writes when nothing was ever recorded (a
/// redirected/non-tty run never registers one).
static TERMINAL_RESTORE: Mutex<Option<Registered>> = Mutex::new(None);

#[derive(Clone)]
struct Registered {
    restore: RestoreFn,
    /// Whether a panic runs it too. The TUI's says "aborted (second Ctrl-C)", and
    /// the panic hook has its own way of restoring what the TUI did.
    on_panic: bool,
}

/// Register the closure the hard-abort path calls to restore the terminal.
/// Call once raw mode is entered, with a closure that already captured
/// whatever state (e.g. the pre-raw-mode `termios`) it needs to restore.
pub fn set_terminal_restore(f: impl Fn() + Send + Sync + 'static) {
    *TERMINAL_RESTORE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Registered {
        restore: Arc::new(f),
        on_panic: false,
    });
}

/// Register a closure for the time a raw-mode session sits inside another one
/// (a tool's PTY while the TUI has stepped aside), for the hard-abort path and
/// for [`restore_terminal_on_panic`]. Dropping the returned scope puts back
/// whatever was registered before.
#[must_use = "dropping the scope ends the registration"]
pub fn scoped_terminal_restore(f: impl Fn() + Send + Sync + 'static) -> TerminalRestoreScope {
    let registered = Registered {
        restore: Arc::new(f),
        on_panic: true,
    };
    let previous = TERMINAL_RESTORE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(registered);
    TerminalRestoreScope { previous }
}

/// See [`scoped_terminal_restore`].
pub struct TerminalRestoreScope {
    previous: Option<Registered>,
}

impl std::fmt::Debug for TerminalRestoreScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalRestoreScope")
            .finish_non_exhaustive()
    }
}

impl Drop for TerminalRestoreScope {
    fn drop(&mut self) {
        *TERMINAL_RESTORE.lock().unwrap_or_else(|e| e.into_inner()) = self.previous.take();
    }
}

/// For the panic hook: run the registered closure if a scope registered it. A
/// panic can skip the `Drop` of what is raw, as an abort does, so this is what is
/// left to put the terminal back.
pub fn restore_terminal_on_panic() {
    let registered = TERMINAL_RESTORE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(registered) = registered.filter(|r| r.on_panic) {
        (registered.restore)();
    }
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
        (f.restore)();
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

    /// R27: one tool exiting ends only its own transparency — a press is
    /// still dropped while another invocation's tool runs — and once none
    /// runs, presses reach heph again, which now knows a tool has exited.
    #[test]
    fn tool_exited_ends_only_its_own_transparency() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (trigger, mut rx) = ShutdownTrigger::new();
        let first = hold_transparent();
        let second = hold_transparent();
        tool_exited(first);
        assert!(is_after_tool());
        assert!(is_transparent(), "the other tool still runs");
        trigger.trigger();
        assert!(rx.try_recv().is_err(), "its press is the tool's");
        tool_exited(second);
        assert!(!is_transparent());
        trigger.trigger();
        assert!(rx.try_recv().is_ok(), "heph's again once no tool runs");
    }

    /// While a scope holds it, the abort and a panic run its closure. Afterwards
    /// the TUI's is back for the abort, and a panic never runs that one, since it
    /// says "aborted (second Ctrl-C)".
    #[test]
    fn scoped_restore_puts_the_previous_back_and_alone_runs_on_panic() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let counter = || {
            let calls = Arc::new(AtomicUsize::new(0));
            let bump = Arc::clone(&calls);
            (calls, move || {
                bump.fetch_add(1, Ordering::SeqCst);
            })
        };
        let (tui, tui_restore) = counter();
        let (session, session_restore) = counter();

        set_terminal_restore(tui_restore);
        restore_terminal_on_panic();
        assert_eq!(tui.load(Ordering::SeqCst), 0, "a panic ran the TUI's");

        let scope = scoped_terminal_restore(session_restore);
        restore_terminal_on_panic();
        assert_eq!(session.load(Ordering::SeqCst), 1, "a panic");
        restore_terminal();
        assert_eq!(session.load(Ordering::SeqCst), 2, "the abort");
        assert_eq!(tui.load(Ordering::SeqCst), 0, "shadowed by the scope");

        drop(scope);
        restore_terminal();
        assert_eq!(tui.load(Ordering::SeqCst), 1, "the TUI's is back");
        assert_eq!(session.load(Ordering::SeqCst), 2, "the scope is over");
        clear_terminal_restore();
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
