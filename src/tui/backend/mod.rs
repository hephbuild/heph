pub mod ci;
pub mod interactive;

use tokio::sync::mpsc::UnboundedReceiver;

use crate::engine::event::BuildEvent;

/// Fold every event currently buffered in `rx` into `view`, returning how many
/// were folded. Non-blocking: `try_recv` stops at the first empty (or closed)
/// read, so this drains the present backlog without ever waiting for more.
///
/// The backends call this when the build-event select arm wakes, so an emitted
/// burst (a warm run resolves its whole transitive closure near-instantly,
/// emitting tens of thousands of typed events) folds in one pass between two
/// render ticks instead of one event per `select!` iteration.
pub(crate) fn fold_buffered<V: ?Sized>(
    rx: &mut UnboundedReceiver<BuildEvent>,
    view: &mut V,
    apply: impl Fn(&mut V, &BuildEvent),
) -> usize {
    let mut folded = 0;
    while let Ok(ev) = rx.try_recv() {
        apply(view, &ev);
        folded += 1;
    }
    folded
}

#[cfg(test)]
mod tests {
    use super::fold_buffered;
    use crate::engine::event::{BuildEvent, BuildEventKind};
    use tokio::sync::mpsc;

    fn result_end(addr: &str) -> BuildEvent {
        BuildEvent {
            at_unix_ms: 0,
            kind: BuildEventKind::ResultEnd {
                addr: addr.to_string(),
                error: None,
            },
        }
    }

    /// One wake folds the entire buffered backlog and leaves the channel empty —
    /// the contract that replaces the one-event-per-`select!`-iteration drain.
    #[tokio::test]
    async fn drains_whole_backlog_in_one_pass() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        const K: usize = 5_000;
        for i in 0..K {
            tx.send(result_end(&format!("//pkg:t{i}"))).expect("send");
        }

        // Stand in for a view: count folds. The drain mechanics are what's under
        // test, not a specific aggregator.
        let mut count = 0usize;
        let folded = fold_buffered(&mut rx, &mut count, |c, _ev| *c += 1);

        assert_eq!(folded, K, "should fold every buffered event in one call");
        assert_eq!(count, K, "view sees every event");
        assert!(rx.try_recv().is_err(), "channel drained empty");
    }

    /// An empty channel folds nothing (and does not block).
    #[tokio::test]
    async fn empty_channel_folds_nothing() {
        let (_tx, mut rx) = mpsc::unbounded_channel();
        let mut count = 0usize;
        assert_eq!(fold_buffered(&mut rx, &mut count, |c, _e| *c += 1), 0);
        assert_eq!(count, 0);
    }
}
