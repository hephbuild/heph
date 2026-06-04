//! On-demand stack growth for the recursive result/meta traversal.
//!
//! `Engine::result_addr` resolves a target by recursively resolving its inputs'
//! results (to hash them) — one `#[async_recursion]` level per dependency-graph
//! edge. Because intermediate `result`/`meta`/`spec` lookups hit the memoizer
//! cache and resolve *synchronously* (no `Pending`, no yield), a single `poll`
//! can descend the whole dep graph in one go, building ~30 stack frames (~100 KiB)
//! per level. On a deep go monorepo this overflows the 2 MiB tokio worker stack
//! ("thread 'tokio-rt-worker' has overflowed its stack").
//!
//! Yielding doesn't help: the cached path never returns `Pending`, and re-polling
//! a suspended future re-descends the same nested boxed futures, rebuilding the
//! same depth. The fix is to grow the *physical* stack on demand — the same
//! approach rustc uses for deeply recursive ASTs. [`GrowStack`] wraps a future so
//! every `poll` runs under [`stacker::maybe_grow`]: a couple-instruction check on
//! the hot path, allocating a fresh stack segment only when headroom runs low.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// If less than this much stack remains when a wrapped future is polled, grow.
/// Sized well above the worst-case ~100 KiB a single dep-graph level consumes
/// between consecutive [`GrowStack`] poll points, so we never run out mid-level.
const RED_ZONE: usize = 512 * 1024;

/// Size of each freshly allocated stack segment. Large enough to host many
/// recursion levels before the next growth.
const STACK_PER_GROW: usize = 8 * 1024 * 1024;

/// Future wrapper that polls `inner` under [`stacker::maybe_grow`], so deep
/// synchronous poll cascades grow the physical stack instead of overflowing.
///
/// Requires `F: Unpin`; in practice the wrapped value is the `Pin<Box<dyn Future>>`
/// produced by `#[async_recursion]`, which is `Unpin`, so the bound is free.
pub struct GrowStack<F> {
    inner: F,
}

/// Wrap `inner` so each poll grows the stack on demand. Adds no heap allocation:
/// the returned value is a thin stack-held struct around the existing future.
pub fn grow_stack<F: Future + Unpin>(inner: F) -> GrowStack<F> {
    GrowStack { inner }
}

impl<F: Future + Unpin> Future for GrowStack<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        // `Self: Unpin` (F: Unpin), so a `&mut` projection is sound without pinning machinery.
        let inner = &mut self.get_mut().inner;
        stacker::maybe_grow(RED_ZONE, STACK_PER_GROW, || Pin::new(inner).poll(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deeply self-recursive future that resolves entirely synchronously (it
    /// never awaits anything that returns `Pending`), so driving it builds the
    /// full recursion depth on the poll stack in one shot — exactly the cascade
    /// that overflows the engine's worker stack. Each level is wrapped in
    /// `grow_stack`.
    fn deep(n: usize) -> Pin<Box<dyn Future<Output = usize> + Send>> {
        Box::pin(async move {
            if n == 0 {
                return 0;
            }
            grow_stack(deep(n - 1)).await + 1
        })
    }

    #[test]
    fn grow_stack_survives_deep_synchronous_recursion() {
        // A small stack that the un-grown recursion would blow well before this
        // depth. With `grow_stack` it allocates fresh segments and completes.
        let handle = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| futures::executor::block_on(deep(20_000)))
            .expect("spawn small-stack thread");
        assert_eq!(handle.join().expect("thread must not overflow"), 20_000);
    }
}
