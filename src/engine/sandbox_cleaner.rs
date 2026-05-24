//! Fire-and-forget sandbox directory cleanup on a dedicated OS thread.
//!
//! Posting to the queue is a non-blocking `crossbeam_channel::send`. A
//! single long-lived worker thread drains the queue and runs
//! `std::fs::remove_dir_all` inline. No tokio waker is involved (avoids the
//! macOS cross-thread waker bug — see `RCA_MACOS_WAKER.md`), and no tokio
//! runtime worker is parked for the cleanup (avoids the `block_in_place`
//! concurrency regression measured in `PERFORMANCE.md` suggestion #0).
//!
//! Ordering: callers must invoke `enqueue` only *after* any read of the
//! sandbox completes (per `project_sandbox_cleanup_ordering.md`). Within
//! the queue, paths are processed in FIFO order on one thread, so two
//! enqueues of the same path are serialized.
use crossbeam_channel::{Sender, unbounded};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::{fs, io, thread};

static CLEANER: OnceLock<Sender<PathBuf>> = OnceLock::new();

fn sender() -> &'static Sender<PathBuf> {
    CLEANER.get_or_init(|| {
        let (tx, rx) = unbounded::<PathBuf>();
        thread::Builder::new()
            .name("rheph-sandbox-cleaner".into())
            .spawn(move || {
                for path in rx.iter() {
                    match fs::remove_dir_all(&path) {
                        Ok(()) => (),
                        Err(err) if err.kind() == io::ErrorKind::NotFound => (),
                        Err(err) => {
                            tracing::error!(
                                error = %err,
                                path = %path.display(),
                                "failed to clean up sandbox",
                            );
                        }
                    }
                }
            })
            .expect("spawn sandbox-cleaner thread");
        tx
    })
}

/// Enqueue `path` for asynchronous removal. Non-blocking. If the cleaner
/// thread has panicked or the channel is otherwise unusable, the request
/// is dropped and a log line is emitted.
pub fn enqueue(path: PathBuf) {
    if let Err(err) = sender().send(path) {
        tracing::error!(error = %err, "sandbox cleaner channel closed");
    }
}
