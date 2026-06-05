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
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{fs, io, thread};

/// `remove_dir_all` that recovers from `PermissionDenied`. The Go module
/// cache (and other read-only tooling) leaves `0555` directories behind;
/// the kernel refuses to unlink their children until the dir is writable.
/// On the first permission failure we recursively `chmod 0777` every
/// directory under `dir` and retry the removal once.
///
/// Borrowed from the Go toolchain's `modfetch.MakeDirsReadWrite`:
/// https://github.com/golang/go/blob/3c72dd513c30df60c0624360e98a77c4ae7ca7c8/src/cmd/go/internal/modfetch/fetch.go
pub fn remove_dir_all(dir: &Path) -> io::Result<()> {
    match fs::remove_dir_all(dir) {
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            make_dirs_read_write(dir);
            fs::remove_dir_all(dir)
        }
        other => other,
    }
}

/// Recursively make every directory under `dir` writable (`0777`) so its
/// contents can be removed. Errors walking the tree are ignored — this is
/// a best-effort prelude to a removal retry, mirroring Go's helper.
#[cfg(unix)]
fn make_dirs_read_write(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;

    fn walk(path: &Path) {
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(_) => return,
        };
        if !meta.is_dir() {
            return;
        }
        drop(fs::set_permissions(path, fs::Permissions::from_mode(0o777)));
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            walk(&entry.path());
        }
    }

    walk(dir);
}

#[cfg(not(unix))]
fn make_dirs_read_write(_dir: &Path) {}

/// One cleanup unit. Returning an `io::Result` lets the cleaner thread
/// emit a uniform log line on failure (filtering out `NotFound`, which
/// is common when retries collide).
pub type SandboxCleanupJob = Box<dyn FnOnce() -> io::Result<()> + Send + 'static>;

/// Per-request count of cleanup jobs enqueued but not yet finished (queued +
/// in-flight). Lives in the request state (see `RequestStateData::bg_pending`)
/// and is carried alongside each job so the global cleaner thread can decrement
/// the right request's counter once the job has run. The shutdown path keeps
/// the TUI open — and the process alive — until this drains to zero, so we never
/// exit out from under an in-progress rmdir.
pub type PendingCounter = Arc<AtomicUsize>;

/// Queue entry: a failure `label`, the job, and the request counter to
/// decrement when it completes.
type Job = (String, SandboxCleanupJob, PendingCounter);

static CLEANER: OnceLock<Sender<Job>> = OnceLock::new();

fn sender() -> &'static Sender<Job> {
    CLEANER.get_or_init(|| {
        let (tx, rx) = unbounded::<Job>();
        thread::Builder::new()
            .name("heph-sandbox-cleaner".into())
            .spawn(move || {
                for (label, job, pending) in rx.iter() {
                    // catch_unwind so a panicking job doesn't kill the
                    // long-lived cleaner thread and silently drop every
                    // subsequent cleanup for the process lifetime.
                    let outcome = catch_unwind(AssertUnwindSafe(job));
                    // Decrement after the job runs (not on dequeue) so the
                    // counter only hits zero once the work is genuinely done.
                    pending.fetch_sub(1, Ordering::AcqRel);
                    match outcome {
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

/// Enqueue a cleanup job for asynchronous execution. `label` is used only for
/// log lines emitted if the job fails. `pending` is the request's in-flight
/// counter, bumped here and dropped back by the cleaner once the job has run.
/// Non-blocking.
pub fn enqueue(label: String, job: SandboxCleanupJob, pending: PendingCounter) {
    // Count before sending so the counter can never observe an enqueued job as
    // already drained. The worker decrements once the job has run.
    pending.fetch_add(1, Ordering::AcqRel);
    if let Err(err) = sender().send((label, job, Arc::clone(&pending))) {
        pending.fetch_sub(1, Ordering::AcqRel);
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

    fn counter() -> PendingCounter {
        Arc::new(AtomicUsize::new(0))
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
            counter(),
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
            counter(),
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
            counter(),
        );
        let ran = Arc::new(AtomicBool::new(false));
        let ran_clone = Arc::clone(&ran);
        enqueue(
            "enqueue_swallows_notfound_followup".to_string(),
            Box::new(move || {
                ran_clone.store(true, Ordering::SeqCst);
                Ok(())
            }),
            counter(),
        );
        assert!(
            wait_for(&ran, Duration::from_secs(2)),
            "cleaner thread stopped processing after NotFound"
        );
    }

    #[test]
    fn pending_counter_drains_to_zero_after_job_runs() {
        // A blocked job holds the request counter at 1 until released, then drops
        // it back to 0 once the cleaner finishes it. This is the signal the
        // shutdown path waits on to keep the TUI/process alive during drain.
        let pending = counter();
        let gate = Arc::new(AtomicBool::new(false));
        let gate_job = Arc::clone(&gate);
        let done = Arc::new(AtomicBool::new(false));
        let done_job = Arc::clone(&done);
        enqueue(
            "pending_counter_drains_to_zero_after_job_runs".to_string(),
            Box::new(move || {
                while !gate_job.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(2));
                }
                done_job.store(true, Ordering::SeqCst);
                Ok(())
            }),
            Arc::clone(&pending),
        );
        assert_eq!(
            pending.load(Ordering::Acquire),
            1,
            "counter must rise while job is in flight"
        );
        gate.store(true, Ordering::SeqCst);
        assert!(wait_for(&done, Duration::from_secs(2)), "job did not run");
        // Spin until the worker's post-job decrement lands.
        let deadline = Instant::now() + Duration::from_secs(2);
        while pending.load(Ordering::Acquire) > 0 {
            assert!(Instant::now() < deadline, "counter did not drain to zero");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[cfg(unix)]
    #[test]
    fn remove_dir_all_recovers_from_readonly_dirs() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("sandbox");
        let inner = root.join("ro");
        std::fs::create_dir_all(&inner).expect("mkdir");
        std::fs::write(inner.join("file"), b"x").expect("write file");
        // 0555 dir: kernel refuses to unlink children → plain remove_dir_all
        // fails with PermissionDenied.
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o555)).expect("chmod ro");

        assert!(
            std::fs::remove_dir_all(&root).is_err(),
            "precondition: plain removal must fail on read-only dir"
        );

        remove_dir_all(&root).expect("chmod-retry removal must succeed");
        assert!(!root.exists(), "directory should be gone");
    }

    #[test]
    fn enqueue_survives_panicking_job() {
        // Same shape as the NotFound case: panic shouldn't kill the
        // thread; subsequent jobs still run.
        let pending = counter();
        enqueue(
            "enqueue_survives_panicking_job_panicker".to_string(),
            Box::new(|| panic!("boom")),
            Arc::clone(&pending),
        );
        let ran = Arc::new(AtomicBool::new(false));
        let ran_clone = Arc::clone(&ran);
        enqueue(
            "enqueue_survives_panicking_job_followup".to_string(),
            Box::new(move || {
                ran_clone.store(true, Ordering::SeqCst);
                Ok(())
            }),
            Arc::clone(&pending),
        );
        assert!(
            wait_for(&ran, Duration::from_secs(2)),
            "cleaner thread stopped processing after panic"
        );
        // A panicking job must still decrement the counter (catch_unwind path).
        let deadline = Instant::now() + Duration::from_secs(2);
        while pending.load(Ordering::Acquire) > 0 {
            assert!(Instant::now() < deadline, "counter leaked after panic");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
