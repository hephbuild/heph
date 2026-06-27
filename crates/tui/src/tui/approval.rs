//! Shared state bridging an approval-gated target (engine side) and the
//! interactive TUI (render + keyboard). The engine's approval handler enqueues a
//! request and awaits a one-shot decision; the [`TuiProgressView`] renders the
//! pending prompt on the main view, and the interactive backend resolves it from
//! a `y`/`n` keypress.
//!
//! [`TuiProgressView`]: crate::tui::progress::TuiProgressView

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::oneshot;

/// One rendered notice for display: the input group name and its text.
#[derive(Clone, Debug)]
pub struct ApprovalNoticeView {
    pub name: String,
    pub content: String,
}

struct Pending {
    addr: String,
    notices: Vec<ApprovalNoticeView>,
    /// Resolved exactly once, when the user (or a programmatic responder)
    /// decides. Taken on `respond` so a second decision is a no-op.
    responder: Option<oneshot::Sender<bool>>,
}

#[derive(Default)]
struct Inner {
    /// FIFO of gated targets awaiting a decision. The front is the active
    /// prompt; the rest are shown only as a "+N waiting" count.
    queue: VecDeque<Pending>,
    /// Whether the active prompt's notice body is expanded (Enter toggles).
    expanded: bool,
}

/// Cloneable handle to the pending-approval queue, shared between the engine's
/// approval handler and the TUI renderer/backend.
#[derive(Clone, Default)]
pub struct ApprovalCenter {
    inner: Arc<Mutex<Inner>>,
}

/// A render-ready snapshot of the active prompt.
#[derive(Clone, Debug)]
pub struct ApprovalView {
    pub addr: String,
    pub notices: Vec<ApprovalNoticeView>,
    pub expanded: bool,
    /// How many further targets are queued behind this one.
    pub queued_behind: usize,
}

impl ApprovalCenter {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned lock means a panic while holding it; the queue is plain data,
        // so recover the guard rather than cascading the panic into the renderer.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Engine side: enqueue a gated target and return the receiver resolved when
    /// the user decides. Dropping the receiver (request cancelled) silently
    /// discards the eventual decision.
    pub fn request(
        &self,
        addr: String,
        notices: Vec<ApprovalNoticeView>,
    ) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel();
        let mut inner = self.lock();
        inner.queue.push_back(Pending {
            addr,
            notices,
            responder: Some(tx),
        });
        rx
    }

    /// Whether a prompt is currently awaiting a decision.
    pub fn is_active(&self) -> bool {
        !self.lock().queue.is_empty()
    }

    /// Snapshot the active prompt for rendering, or `None` when idle.
    pub fn current(&self) -> Option<ApprovalView> {
        let inner = self.lock();
        let front = inner.queue.front()?;
        Some(ApprovalView {
            addr: front.addr.clone(),
            notices: front.notices.clone(),
            expanded: inner.expanded,
            queued_behind: inner.queue.len().saturating_sub(1),
        })
    }

    /// Expand/collapse the active prompt's notice body (Enter).
    pub fn toggle_expanded(&self) {
        let mut inner = self.lock();
        if !inner.queue.is_empty() {
            inner.expanded = !inner.expanded;
        }
    }

    /// Resolve the active prompt and advance to the next queued one. A no-op when
    /// idle. Collapses the notice so the next prompt starts collapsed.
    pub fn respond(&self, approve: bool) {
        let mut inner = self.lock();
        if let Some(mut pending) = inner.queue.pop_front() {
            if let Some(tx) = pending.responder.take() {
                // Receiver gone (request cancelled) → decision is moot. Bind to a
                // named ignore: the `Result<(), bool>` is `Copy`, so `drop` is a
                // no-op and `let _` trips `let_underscore_must_use`.
                let _sent = tx.send(approve);
            }
            inner.expanded = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_then_approve_resolves_receiver() {
        let center = ApprovalCenter::new();
        let rx = center.request(
            "//a:b".to_string(),
            vec![ApprovalNoticeView {
                name: "plan".to_string(),
                content: "do the thing".to_string(),
            }],
        );
        assert!(center.is_active());
        let view = center.current().expect("active");
        assert_eq!(view.addr, "//a:b");
        assert_eq!(view.notices.len(), 1);
        assert!(!view.expanded);

        center.toggle_expanded();
        assert!(center.current().expect("active").expanded);

        center.respond(true);
        assert!(!center.is_active());
        assert!(rx.await.expect("decision"));
    }

    #[tokio::test]
    async fn reject_sends_false_and_queue_advances() {
        let center = ApprovalCenter::new();
        let rx1 = center.request("//a:1".to_string(), vec![]);
        let rx2 = center.request("//a:2".to_string(), vec![]);
        assert_eq!(center.current().expect("active").queued_behind, 1);

        center.respond(false);
        assert!(!rx1.await.expect("decision"));
        // Second prompt is now active.
        assert_eq!(center.current().expect("active").addr, "//a:2");
        center.respond(true);
        assert!(rx2.await.expect("decision"));
        assert!(!center.is_active());
    }
}
