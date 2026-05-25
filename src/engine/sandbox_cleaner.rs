//! Fire-and-forget sandbox cleanup on a dedicated OS thread.
//!
//! Posting to the queue is a non-blocking `crossbeam_channel::send`. A
//! single long-lived worker thread drains the queue and runs each job
//! inline. No tokio waker is involved (avoids the macOS cross-thread
//! waker bug — see `RCA_MACOS_WAKER.md`), and no tokio runtime worker
//! is parked for the cleanup (avoids the `block_in_place` concurrency
//! regression measured in `PERFORMANCE.md` suggestion #0).
//!
//! Each job is an opaque `FnOnce` so the layer that built the sandbox
//! also owns the knowledge of how to tear it down. The FUSE bridge
//! rms its upper-side dir directly (bypassing the live mount); the OS
//! bridge rms the plain sandbox dir. The cleaner doesn't branch.
//!
//! Ordering: callers must invoke `enqueue` only *after* any read of the
//! sandbox completes (per `project_sandbox_cleanup_ordering.md`). Within
//! the queue, jobs are processed in FIFO order on one thread.
use crossbeam_channel::{Sender, unbounded};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;
use std::{io, thread};

/// One cleanup unit. Returning an `io::Result` lets the cleaner thread
/// emit a uniform log line on failure (filtering out `NotFound`, which
/// is common when retries collide).
pub type SandboxCleanupJob = Box<dyn FnOnce() -> io::Result<()> + Send + 'static>;

static CLEANER: OnceLock<Sender<(String, SandboxCleanupJob)>> = OnceLock::new();

fn sender() -> &'static Sender<(String, SandboxCleanupJob)> {
    CLEANER.get_or_init(|| {
        let (tx, rx) = unbounded::<(String, SandboxCleanupJob)>();
        thread::Builder::new()
            .name("rheph-sandbox-cleaner".into())
            .spawn(move || {
                for (label, job) in rx.iter() {
                    // catch_unwind so a panicking job doesn't kill the
                    // long-lived cleaner thread and silently drop every
                    // subsequent cleanup for the process lifetime.
                    match catch_unwind(AssertUnwindSafe(job)) {
                        Ok(Ok(())) => (),
                        Ok(Err(err)) if err.kind() == io::ErrorKind::NotFound => (),
                        Ok(Err(err)) => {
                            tracing::error!(
                                error = %err,
                                label = %label,
                                "failed to clean up sandbox",
                            );
                        }
                        Err(_) => {
                            tracing::error!(
                                label = %label,
                                "sandbox cleanup job panicked",
                            );
                        }
                    }
                }
            })
            .expect("spawn sandbox-cleaner thread");
        tx
    })
}

/// Enqueue a cleanup job for asynchronous execution. `label` is used
/// only for log lines emitted if the job fails. Non-blocking.
pub fn enqueue(label: String, job: SandboxCleanupJob) {
    if let Err(err) = sender().send((label, job)) {
        tracing::error!(error = %err, "sandbox cleaner channel closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    fn wait_for(flag: &AtomicBool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !flag.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        true
    }

    #[test]
    fn enqueue_runs_job_on_cleaner_thread() {
        let ran = Arc::new(AtomicBool::new(false));
        let ran_clone = Arc::clone(&ran);
        enqueue(
            "enqueue_runs_job_on_cleaner_thread".to_string(),
            Box::new(move || {
                ran_clone.store(true, Ordering::SeqCst);
                Ok(())
            }),
        );
        assert!(
            wait_for(&ran, Duration::from_secs(2)),
            "job did not run within 2s"
        );
    }

    #[test]
    fn enqueue_removes_tempdir_via_closure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("nested");
        std::fs::create_dir_all(target.join("a/b")).expect("mkdir");
        std::fs::write(target.join("a/b/file"), b"x").expect("write file");
        assert!(target.exists());
        let target_for_job = target.clone();
        let done = Arc::new(AtomicBool::new(false));
        let done_clone = Arc::clone(&done);
        enqueue(
            "enqueue_removes_tempdir_via_closure".to_string(),
            Box::new(move || {
                let res = std::fs::remove_dir_all(&target_for_job);
                done_clone.store(true, Ordering::SeqCst);
                res
            }),
        );
        assert!(
            wait_for(&done, Duration::from_secs(2)),
            "job did not run within 2s"
        );
        assert!(!target.exists(), "cleanup closure did not remove target");
    }

    #[test]
    fn enqueue_swallows_notfound() {
        // No assertion on log output (tracing is global); the
        // important behavior is that the cleaner thread doesn't die
        // when a job returns NotFound. We follow up with another job
        // that must run on the same thread.
        enqueue(
            "enqueue_swallows_notfound_first".to_string(),
            Box::new(|| Err(io::Error::from(io::ErrorKind::NotFound))),
        );
        let ran = Arc::new(AtomicBool::new(false));
        let ran_clone = Arc::clone(&ran);
        enqueue(
            "enqueue_swallows_notfound_followup".to_string(),
            Box::new(move || {
                ran_clone.store(true, Ordering::SeqCst);
                Ok(())
            }),
        );
        assert!(
            wait_for(&ran, Duration::from_secs(2)),
            "cleaner thread stopped processing after NotFound"
        );
    }

    #[test]
    fn enqueue_survives_panicking_job() {
        // Same shape as the NotFound case: panic shouldn't kill the
        // thread; subsequent jobs still run.
        enqueue(
            "enqueue_survives_panicking_job_panicker".to_string(),
            Box::new(|| panic!("boom")),
        );
        let ran = Arc::new(AtomicBool::new(false));
        let ran_clone = Arc::clone(&ran);
        enqueue(
            "enqueue_survives_panicking_job_followup".to_string(),
            Box::new(move || {
                ran_clone.store(true, Ordering::SeqCst);
                Ok(())
            }),
        );
        assert!(
            wait_for(&ran, Duration::from_secs(2)),
            "cleaner thread stopped processing after panic"
        );
    }
}
