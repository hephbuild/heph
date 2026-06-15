use std::io::{self, IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::app::{AppContext, Pauser};

const DEFAULT_INTERVAL: Duration = Duration::from_millis(100);

/// Accumulates lines off the hot path and periodically flushes them to stdout.
///
/// The TUI renders to stderr. When stdout is the *same* tty, a raw stdout write
/// would interleave with spinner redraws, so we pause the TUI around each flush.
/// When stdout is redirected (a pipe/file), there is nothing to interleave with —
/// pausing would only churn the TUI (each resume re-anchors the inline viewport,
/// scrolling blank lines onto stderr), so we write directly without pausing.
pub struct BufferedStdout {
    buf: Arc<Mutex<Vec<u8>>>,
    task: Option<JoinHandle<()>>,
    shutdown: Option<oneshot::Sender<()>>,
}

impl BufferedStdout {
    pub fn new(ctx: &AppContext) -> Self {
        Self::with_interval(ctx, DEFAULT_INTERVAL)
    }

    pub fn with_interval(ctx: &AppContext, interval: Duration) -> Self {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let pauser = ctx.pauser();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        // Decide once: pausing the TUI is only needed when stdout shares the
        // stderr tty (interleaving risk). Redirected stdout never interleaves.
        let pause_for_writes = io::stdout().is_terminal();
        let task = tokio::spawn({
            let buf = buf.clone();
            async move {
                drive(buf, pauser, pause_for_writes, interval, shutdown_rx).await;
            }
        });

        Self {
            buf,
            task: Some(task),
            shutdown: Some(shutdown_tx),
        }
    }

    pub fn println(&self, line: impl AsRef<str>) {
        let line = line.as_ref();
        let mut b = self.buf.lock().expect("stdout buffer lock");
        b.extend_from_slice(line.as_bytes());
        b.push(b'\n');
    }

    /// Stop the flusher, perform a final flush, and wait for the task
    /// to exit. Must be called before the TUI backend tears down,
    /// otherwise the final flush cannot acquire a pause guard.
    pub async fn close(mut self) {
        if let Some(tx) = self.shutdown.take()
            && tx.send(()).is_err()
        {
            // receiver dropped — task already exited
        }
        if let Some(task) = self.task.take() {
            drop(task.await);
        }
    }
}

impl Drop for BufferedStdout {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take()
            && tx.send(()).is_err()
        {
            // receiver dropped — task already exited
        }
    }
}

async fn drive(
    buf: Arc<Mutex<Vec<u8>>>,
    pauser: Pauser,
    pause_for_writes: bool,
    interval: Duration,
    mut shutdown: oneshot::Receiver<()>,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                flush_once(&buf, &pauser, pause_for_writes).await;
                break;
            }
            _ = ticker.tick() => {
                flush_once(&buf, &pauser, pause_for_writes).await;
            }
        }
    }
}

async fn flush_once(buf: &Arc<Mutex<Vec<u8>>>, pauser: &Pauser, pause_for_writes: bool) {
    let bytes = {
        let mut b = buf.lock().expect("stdout buffer lock");
        if b.is_empty() {
            return;
        }
        std::mem::take(&mut *b)
    };
    // Only pause the TUI when stdout shares the stderr tty; otherwise a redirected
    // write can't interleave, and pausing would needlessly re-anchor the viewport.
    let guard = if pause_for_writes {
        Some(pauser.pause().await)
    } else {
        None
    };
    drop(io::stdout().lock().write_all(&bytes));
    drop(guard);
}
