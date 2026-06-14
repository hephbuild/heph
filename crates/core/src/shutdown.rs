//! In-process shutdown signal shared by the SIGINT listener (engine/bin) and the
//! TUI's Ctrl+C handler. Lives here (the lowest crate) so the TUI can hold a
//! `ShutdownTrigger` without depending on the bin's bootstrap module.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;

/// Producer-side handle for the in-process shutdown signal. Both the SIGINT
/// listener and the TUI's Ctrl+C key handler call `trigger()` on this; a single
/// consumer drives the shutdown state machine from the paired receiver.
///
/// While `SuppressionHandle::set(true)` is in effect (e.g. the TUI is paused for
/// an interactive prompt), `trigger()` silently drops presses.
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
