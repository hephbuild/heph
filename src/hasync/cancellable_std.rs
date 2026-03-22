use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use crate::hasync::Cancellable;

/// A manual, std-only implementation of a cancellation signal.
pub struct StdCancellationToken {
    inner: Arc<SharedState>,
}

struct SharedState {
    cancelled: AtomicBool,
    // We need to store the waker so the 'cancel' side can wake the 'await' side.
    waker: Mutex<Option<Waker>>,
}

impl StdCancellationToken {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(SharedState {
                cancelled: AtomicBool::new(false),
                waker: Mutex::new(None),
            }),
        }
    }

    pub fn cancel(&self) {
        // 1. Mark as cancelled
        self.inner.cancelled.store(true, Ordering::SeqCst);

        // 2. Wake the pending future if it's currently being awaited
        if let Ok(mut guard) = self.inner.waker.lock() {
            if let Some(waker) = guard.take() {
                waker.wake();
            }
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }
}

// This allows the token to be used in 'await' expressions
impl Future for StdCancellationToken {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // If already cancelled, resolve immediately
        if self.is_cancelled() {
            return Poll::Ready(());
        }

        // Otherwise, register the current task's waker
        let mut guard = self.inner.waker.lock().unwrap();
        *guard = Some(cx.waker().clone());

        Poll::Pending
    }
}

// Integration with our Trait from before
impl Cancellable for StdCancellationToken {
    fn is_cancelled(&self) -> bool {
        self.is_cancelled()
    }

    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        // Since StdCancellationToken implements Future, we just clone and box it
        let clone = StdCancellationToken { inner: self.inner.clone() };
        Box::pin(clone)
    }
}
