//! On-demand stack growth for the transparent-group re-inline — the one
//! recursion in `result_addr` that still nests poll frames.
//!
//! With task-backed request memoizers, the memoized result/meta descent is a
//! chain of tasks: per-poll stack depth is O(1) in graph depth, and this
//! wrapper is not involved (the `deep_warm_chain_completes_on_a_2mib_stack`
//! test pins that without it). Transparent groups are different by design:
//! they are inlined *before* the memoizer — nothing to deduplicate — so a
//! group whose member is another group recurses through `result_addr` in the
//! caller's own poll, one boxed `#[async_recursion]` frame per nesting level.
//! Deep enough group chains overflow a 2 MiB worker stack (the
//! `deep_transparent_group_chain_completes_on_a_2mib_stack` test goes red
//! without this wrapper at 300 levels on a debug build).
//!
//! So the group branch wraps its recursive calls in [`GrowStack`]: every
//! `poll` runs under [`stacker::maybe_grow`] — a couple-instruction check on
//! the hot path, allocating a fresh stack segment only when headroom runs
//! low. The same approach rustc uses for deeply recursive ASTs.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// If less than this much stack remains when a wrapped future is polled, grow.
/// Inherited from the wrapper's memoized-descent era (sized for ~100 KiB
/// levels); generous for the group re-inline's ~KB frames, and the check is
/// cheap enough that right-sizing it buys nothing.
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

    /// `maybe_grow` has to work on a *tokio worker*, not just on a thread we sized
    /// ourselves: `remaining_stack()` must resolve the bounds of a runtime-owned
    /// thread, and the segment switch must survive being driven through a task.
    /// That is the shape every caller uses — `Engine::result` spawns each target,
    /// and both TUI backends spawn the app future — so the wrapper is load-bearing
    /// exactly here.
    ///
    /// The stack size is set explicitly rather than left to tokio's 2 MiB default:
    /// the default honours `RUST_MIN_STACK`, and under a large value this would
    /// pass with the wrapper removed — a test that cannot fail.
    #[test]
    fn grow_stack_survives_deep_recursion_on_a_tokio_worker() {
        const DEPTH: usize = 20_000;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_stack_size(2 * 1024 * 1024)
            .build()
            .expect("build runtime");
        let reached = rt.block_on(async { tokio::spawn(grow_stack(deep(DEPTH))).await });
        assert_eq!(reached.expect("app task"), DEPTH);
    }
}
