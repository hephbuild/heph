//! macOS subprocess pipeline: `std::process` + `std::thread` + `std::sync::mpsc`.
//!
//! No tokio types touch the spawn/drain/wait path. The only point where tokio
//! is involved is `block_in_place` at the async boundary, so the calling task
//! synchronously parks on a `std::sync::mpsc` condvar. Tokio's cross-thread
//! waker (`mio::Waker` → `EVFILT_USER`) is never used — see the module docs on
//! [`super`] for why, and note that this rules out `yield_now` *after* a
//! `block_in_place` as much as it rules out `spawn_blocking`: once the core has
//! been handed off, the task's own wake goes out over the remote path. Where a
//! blocking wait needs to be interruptible it terminates on a flag, not on a
//! wake.

use crate::process_supervisor;
use crate::process_watcher;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use hcore::hasync::Cancellable;
use std::io::{self, Write as _};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, ChildStdin, Command, ExitStatus, Output};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::{CHUNK_SIZE, STREAM_DRAIN_CHUNKS, Spec, StreamId};

/// Granularity for `recv_timeout` polls during `wait_or_cancel`. 100ms keeps
/// CPU idle while still giving sub-second cancel response. Independent of
/// the watcher's own 1s backstop poll.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Budget for joining drain threads after `exit_rx` has resolved. A drain
/// `read()` still blocking after this strongly implies a surviving
/// descendant (e.g. a daemon double-forked by the immediate child and
/// reparented to pid 1) is holding the pipe write end. We escalate with
/// `killpg` on the pgid (relies on the spawning driver setting
/// `setsid: true`) and grant a second budget.
///
/// One window, taken from the cross-backend [`super::DRAIN_DEADLINE`] so the
/// two backends share a single knob — but note this side spends it *twice*
/// (once before the `killpg`, once after), so the worst-case post-exit drain
/// is ~2x Linux's. See [`super::DRAIN_DEADLINE`] for what that divergence
/// does and does not cost.
const DRAIN_JOIN_BUDGET: Duration = super::DRAIN_DEADLINE;

/// Polling interval while waiting for drain threads' "done" flags after
/// `exit_rx`. Short enough to keep latency low; large enough not to spin.
const DRAIN_JOIN_POLL: Duration = Duration::from_millis(10);

/// How often the merged output park re-checks [`Handle`]'s `abandoned` flag.
///
/// This is a **liveness backstop, not a poll loop**: a chunk wakes the park
/// immediately, so a live stream never pays for it, and the whole loop runs
/// inside a *single* `block_in_place` — one worker handoff per park, exactly
/// as an unsliced `recv()` costs. The tick only decides how quickly the
/// reader notices that no chunk is ever coming.
///
/// It has to be a flag rather than an enclosing `timeout`. A future parked in
/// `block_in_place` is never `Pending`, so nothing outside it can end it —
/// `timeout` cannot fire, `select!` cannot drop the branch, `abort` has no
/// yield point. The obvious repair, returning `Pending` between ticks, is
/// worse than the disease: after `block_in_place` gives the core away the task
/// usually resumes core-less, so its next wake goes out through
/// `push_remote_task` → `notify_parked_remote` → `mio::Waker` → `EVFILT_USER`
/// — the very wake this module exists to avoid depending on. Terminating on a
/// flag the drain side sets keeps the whole path on the condvar.
const OUTPUT_PARK_TICK: Duration = Duration::from_millis(100);

fn is_multi_thread() -> bool {
    matches!(
        tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()),
        Ok(tokio::runtime::RuntimeFlavor::MultiThread)
    )
}

/// Synchronously park the calling worker on `f` (multi-thread) or call `f`
/// inline (current-thread). Used for short non-blocking work (closing
/// stdin, joining drain threads). Callers MUST NOT pass `f` that performs
/// an indefinite blocking wait on current-thread — use [`recv_async`] for
/// channel waits instead.
fn block_or_inline<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    if is_multi_thread() {
        tokio::task::block_in_place(f)
    } else {
        f()
    }
}

/// One item on the merged drain channel. Chunks carry the stream they came
/// from so the single consumer can route them.
///
/// There is no explicit per-stream EOF marker: a drain thread owns its clone
/// of the sender and drops it on return, so "both streams finished" is
/// exactly "the channel disconnected". Nothing downstream needs to observe
/// one stream ending while the other runs.
enum DrainMsg {
    Chunk(StreamId, Vec<u8>),
    Err(StreamId, io::Error),
}

/// How much the drain threads may buffer ahead of the consumer.
///
/// Deliberately per-call-site rather than one global policy:
///
/// - **Streaming** ([`spawn`]) has a consumer running concurrently with the
///   child, so a bound turns into backpressure on the child and caps the
///   parent's heap.
/// - **Batch** ([`output`]) has *no* consumer until the child has been
///   reaped — it collects with `take_queued` afterwards. A bound there is a
///   guaranteed deadlock: the drain would block in `send`, stop reading, the
///   pipe would fill, and the child would never exit to release the wait that
///   would start the consumer.
///
/// `crossbeam_channel` rather than `std::sync::mpsc` only because std splits
/// bounded and unbounded into two *sender* types, which would force an enum
/// and a hand-written `send` on every drain thread to carry the choice.
/// Crossbeam's park is the same kernel wait, so the no-tokio-waker property
/// is unchanged.
#[derive(Clone, Copy)]
enum DrainCapacity {
    Bounded(usize),
    Unbounded,
}

impl DrainCapacity {
    fn channel(self) -> (Sender<DrainMsg>, Receiver<DrainMsg>) {
        match self {
            Self::Bounded(n) => crossbeam_channel::bounded(n),
            Self::Unbounded => crossbeam_channel::unbounded(),
        }
    }
}

/// Async reader over **both** of the child's output streams, backed by one
/// `std::sync::mpsc::Receiver`. Each `recv()` parks the calling worker via
/// `block_in_place` on the condvar inside the channel; never goes through
/// tokio's waker.
///
/// One receiver for two streams is the point. A per-stream reader whose
/// `recv` blocks the task means the first stream polled owns the task until
/// the child exits, and the other stream's output is invisible until then —
/// a compile that is quiet on stdout and noisy on stderr showed nothing at
/// all until it finished.
pub struct OutputReader {
    rx: Receiver<DrainMsg>,
    /// Set by the [`Handle`] side once it has given up on a drain thread that
    /// will never reach EOF. Without it this receiver has no termination
    /// condition at all: a detached thread keeps its sender, so the channel
    /// never disconnects. See [`OUTPUT_PARK_TICK`].
    abandoned: Arc<AtomicBool>,
}

/// One spawned drain thread plus a flag the thread flips to `true` right
/// before returning. Lets `Handle::wait` poll for completion under a
/// deadline without calling `JoinHandle::join` (which has no timeout).
struct DrainHandle {
    join: JoinHandle<()>,
    done: Arc<AtomicBool>,
    /// True while the thread is parked handing a chunk to the consumer rather
    /// than reading the pipe. The two are opposite diagnoses: a thread stuck
    /// in `read` is waiting on a stray descendant and only `killpg` can help
    /// it, while one stuck in `send` is waiting on *our* consumer, which no
    /// signal can hurry and which always releases it in the end (a dropped
    /// receiver fails the `send` outright).
    sending: Arc<AtomicBool>,
}

impl DrainHandle {
    fn finished(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// Blocked on something only the consumer can end, so neither the
    /// escalation nor the abandon applies.
    fn waiting_on_consumer(&self) -> bool {
        !self.finished() && self.sending.load(Ordering::Acquire)
    }

    /// Blocked on a `read` that may never return.
    fn waiting_on_pipe(&self) -> bool {
        !self.finished() && !self.sending.load(Ordering::Acquire)
    }
}

/// Poll all drain `done` flags until they're set or `budget` elapses.
/// Returns `true` if every drain finished. Uses tokio's timer; the caller
/// must be in async context.
async fn poll_drains(drains: &[DrainHandle], budget: Duration) -> bool {
    if drains.is_empty() {
        return true;
    }
    let deadline = Instant::now() + budget;
    loop {
        if drains.iter().all(DrainHandle::finished) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let sleep = DRAIN_JOIN_POLL.min(deadline - now);
        tokio::time::sleep(sleep).await;
    }
}

/// Join every drain thread, dropping the result. The threads have set
/// `done` so the join is essentially non-blocking — this is just OS-level
/// cleanup of the thread resources.
fn join_finished(drains: Vec<DrainHandle>) {
    block_or_inline(|| {
        for d in drains {
            drop(d.join.join());
        }
    });
}

/// Wait for drain threads to reach EOF after the child has exited.
///
/// Happy path: every drain returns within `DRAIN_JOIN_BUDGET`. Bug-B
/// path: an orphaned descendant (reparented to pid 1 because the
/// immediate child double-forked) is still holding the pipe write end.
/// We escalate with `process_supervisor::kill_child(pid)` — `killpg` on
/// the pgid reaps the whole tree if `setsid: true` was used at spawn.
/// If even that fails (fd dup'd into a process outside our pgid), we
/// log and detach the still-running threads rather than parking the
/// runtime forever.
///
/// **The bounded streaming drain adds a third state**, and conflating it with
/// the second would be a bug in both directions. A thread parked in a full
/// `send` is not waiting on the child at all — it is waiting on our own
/// consumer, which no signal can hurry. Escalating would fire `killpg`+`kill`
/// at a pid the watcher has already reaped (macOS recycles pids, and heph
/// spawns thousands per build), and abandoning would tell the reader to stop
/// while chunks are still queued for it. So the escalation and the abandon
/// both key off [`DrainHandle::waiting_on_pipe`] only; a consumer-blocked
/// drain is simply left to finish, which it always does — its `send` fails
/// the moment the reader is dropped.
async fn drain_with_deadline(pid: i32, drains: Vec<DrainHandle>, abandoned: &AtomicBool) {
    if drains.is_empty() {
        return;
    }
    if poll_drains(&drains, DRAIN_JOIN_BUDGET).await {
        join_finished(drains);
        return;
    }

    if drains.iter().any(DrainHandle::waiting_on_pipe) {
        tracing::warn!(
            pid,
            "proc_exec: drain threads still reading after child exit; killpg on pgid"
        );
        process_supervisor::kill_child(pid);

        if poll_drains(&drains, DRAIN_JOIN_BUDGET).await {
            join_finished(drains);
            return;
        }
    }

    let stuck = drains.iter().filter(|d| d.waiting_on_pipe()).count();
    let slow = drains.iter().filter(|d| d.waiting_on_consumer()).count();
    let finished: Vec<DrainHandle> = drains.into_iter().filter(DrainHandle::finished).collect();
    join_finished(finished);

    if stuck > 0 {
        // Nothing will ever end these reads, and they still hold their
        // senders, so the channel will never disconnect. Tell the reader
        // directly — it has no other termination condition.
        abandoned.store(true, Ordering::Release);
        tracing::warn!(
            pid,
            stuck,
            "proc_exec: drain threads still blocked on read after killpg; detaching"
        );
    }
    if slow > 0 {
        tracing::debug!(
            pid,
            slow,
            "proc_exec: drain threads still handing output to a slower consumer; \
             leaving them to finish"
        );
    }
    // Stuck thread JoinHandles fall out of scope here; the OS threads
    // remain alive until their read() finally returns. Acceptable leak
    // — the alternative is parking a tokio worker indefinitely.
}

/// Name the stream a read error came from. With both streams on one channel
/// a bare `io::Error` no longer says which pipe failed, and "reading the
/// child's output failed" is not a diagnosis.
fn stream_error(id: StreamId, e: io::Error) -> io::Error {
    let stream = match id {
        StreamId::Stdout => "stdout",
        StreamId::Stderr => "stderr",
    };
    io::Error::new(e.kind(), format!("reading child {stream}: {e}"))
}

impl OutputReader {
    /// Wait for the next chunk from either stream. Returns `Ok(None)` once
    /// **both** streams have finished, `Ok(Some((stream, chunk)))` for data,
    /// or `Err(_)` if a drain thread reported an io error.
    ///
    /// Must not be polled on the same task as [`Handle::wait_or_cancel`] —
    /// see [`Handle`].
    ///
    /// Cancel-safe: there is no suspension point between taking a message off
    /// the channel and returning it, so a dropped `recv` cannot lose a chunk.
    pub async fn recv(&mut self) -> io::Result<Option<(StreamId, Vec<u8>)>> {
        match self.recv_msg().await {
            Some(DrainMsg::Chunk(id, chunk)) => Ok(Some((id, chunk))),
            Some(DrainMsg::Err(id, e)) => Err(stream_error(id, e)),
            None => Ok(None),
        }
    }

    /// Park until a message arrives, both streams finish, or the `Handle` side
    /// abandons a drain that will never reach EOF.
    ///
    /// The whole loop lives in **one** `block_in_place`: the tick re-checks a
    /// flag, it does not re-enter the runtime. Deliberately not
    /// `tokio::task::yield_now` between ticks — see [`OUTPUT_PARK_TICK`].
    async fn recv_msg(&mut self) -> Option<DrainMsg> {
        if is_multi_thread() {
            let rx = &self.rx;
            let abandoned = &self.abandoned;
            tokio::task::block_in_place(move || {
                loop {
                    match rx.recv_timeout(OUTPUT_PARK_TICK) {
                        Ok(msg) => return Some(msg),
                        Err(RecvTimeoutError::Disconnected) => return None,
                        // Queued messages always win: `recv_timeout` only
                        // times out on an *empty* channel, so giving up here
                        // cannot drop anything already handed over.
                        Err(RecvTimeoutError::Timeout) => {
                            if abandoned.load(Ordering::Acquire) {
                                return None;
                            }
                        }
                    }
                }
            })
        } else {
            loop {
                match self.rx.try_recv() {
                    Ok(msg) => return Some(msg),
                    Err(TryRecvError::Disconnected) => return None,
                    Err(TryRecvError::Empty) => {
                        if self.abandoned.load(Ordering::Acquire) {
                            return None;
                        }
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                }
            }
        }
    }
}

/// Async stdin writer wrapping `std::process::ChildStdin`. Each write goes
/// through `block_in_place` so the caller can sit in a tokio task while the
/// underlying write is a sync `std::io::Write`.
///
/// The write blocks once the child's 64 KiB stdin pipe fills and the child is
/// not reading, and `block_in_place` does not make that `Pending` — so a
/// caller must not drive this on the same task as an [`OutputReader`]. See
/// `pluginexec::pump_stdin`, which is why it spawns.
pub struct StdinPump {
    inner: Arc<Mutex<Option<ChildStdin>>>,
}

impl StdinPump {
    pub async fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        let inner = Arc::clone(&self.inner);
        let data = data.to_vec();
        block_or_inline(move || {
            let mut guard = inner
                .lock()
                .map_err(|e| io::Error::other(format!("stdin pump mutex poisoned: {e}")))?;
            if let Some(w) = guard.as_mut() {
                w.write_all(&data)
            } else {
                Err(io::Error::other("stdin pump closed"))
            }
        })
    }

    pub async fn shutdown(&mut self) -> io::Result<()> {
        let inner = Arc::clone(&self.inner);
        block_or_inline(move || {
            let mut guard = inner
                .lock()
                .map_err(|e| io::Error::other(format!("stdin pump mutex poisoned: {e}")))?;
            // Dropping ChildStdin closes the write end of the pipe; the child
            // sees EOF on its stdin.
            drop(guard.take());
            Ok(())
        })
    }
}

/// Live child handle. Internally owns the `std::process::Child` (whose Drop
/// would orphan the process), plus drain thread join handles and the merged
/// output channel. Consume via [`wait`](Self::wait) or [`wait_or_cancel`] to
/// reap.
///
/// # Invariant: never poll a wait on the same task as [`OutputReader::recv`]
///
/// Both park the worker in `block_in_place`, and `wait_or_cancel` does so in
/// a loop that only ends when the child does. A `join!` of the two therefore
/// resolves to "the wait owns the task; nothing is read from the pipes; the
/// child blocks in `write(2)` once they fill; the child never exits" — a
/// deadlock, not a slowdown. Callers must `tokio::spawn` the wait (which is
/// what `pluginexec` does) or drive the reader elsewhere. The same applies to
/// tests: a canceller or a wait `join!`ed onto the reader's task is not just
/// flaky, it is wrong.
pub struct Handle {
    pid: i32,
    child: Option<Child>,
    stdin: Option<StdinPump>,
    output: Option<OutputReader>,
    drains: Vec<DrainHandle>,
    /// Shared with the [`OutputReader`]. Set once nobody is left to make the
    /// drains reach EOF, which is the reader's only termination condition when
    /// a detached thread still holds its sender.
    abandoned: Arc<AtomicBool>,
    reaped: bool,
    /// Receiver registered with `process_watcher` at spawn time. The matching
    /// sender lives in the watcher's `pending` map; that registration is what
    /// guarantees `waitpid` runs even when the Handle is dropped without
    /// `wait*` being called (e.g. caller returns Err between spawn and wait).
    /// Without this, Drop's `kill_child` would SIGKILL the pid but nobody
    /// would reap it — permanent zombie.
    exit_rx: Option<mpsc::Receiver<io::Result<ExitStatus>>>,
    /// Auto-untracks the pid on the supervisor sidecar when the Handle is
    /// dropped. `None` if the supervisor was not initialized (e.g. tests).
    _track_guard: Option<process_supervisor::TrackGuard>,
}

impl Handle {
    pub fn pid(&self) -> i32 {
        self.pid
    }

    pub fn take_stdin(&mut self) -> Option<StdinPump> {
        self.stdin.take()
    }

    /// Take the merged stdout+stderr reader. `None` when neither stream was
    /// piped. Must be driven on a different task from the wait — see
    /// [`Handle`].
    pub fn take_output(&mut self) -> Option<OutputReader> {
        self.output.take()
    }

    /// Wait for exit, but cancel by `SIGKILL`-ing the child if `cancel`
    /// fires. Still consumes the final exit status before returning so the
    /// pid is reaped (no zombies).
    ///
    /// Parks its worker in `block_in_place` for the child's whole lifetime,
    /// and unlike [`OutputReader::recv`] has no tick: this wait is guaranteed
    /// to end (the watcher always sends an exit status) and on runtime
    /// teardown a wait that yielded might never be polled again, leaving the
    /// child unkilled and unreaped. The cost of that choice is the invariant
    /// on [`Handle`] — this must not share a task with an [`OutputReader`].
    pub(super) async fn wait_or_cancel(
        mut self,
        cancel: &(dyn Cancellable + Send + Sync),
    ) -> io::Result<ExitStatus> {
        let rx = self.exit_rx.take().expect("exit_rx must be set by spawn()");
        let pid = self.pid;
        drop(self.child.take());
        // Multi-thread: block_in_place + recv_timeout poll; no tokio waker.
        // Current-thread (tests): async try_recv + tokio::time::sleep poll
        // so sibling tasks can make progress on the single thread.
        let status = if is_multi_thread() {
            let cancel_ref = cancel;
            tokio::task::block_in_place(move || -> io::Result<ExitStatus> {
                // Cancellation escalates: SIGINT first, then SIGKILL if the
                // child outlives the grace window. `interrupted_at` records
                // when we sent SIGINT so the same poll loop can time the grace.
                let mut interrupted_at: Option<Instant> = None;
                let mut killed = false;
                loop {
                    match rx.recv_timeout(CANCEL_POLL_INTERVAL) {
                        Ok(Ok(s)) => return Ok(s),
                        Ok(Err(e)) => return Err(e),
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if cancel_ref.is_cancelled() {
                                match interrupted_at {
                                    None => {
                                        process_supervisor::interrupt_child(pid);
                                        interrupted_at = Some(Instant::now());
                                    }
                                    Some(at) if !killed && at.elapsed() >= super::CANCEL_GRACE => {
                                        process_supervisor::kill_child(pid);
                                        killed = true;
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            return Err(io::Error::other("watcher dropped sender"));
                        }
                    }
                }
            })?
        } else {
            let mut interrupted_at: Option<Instant> = None;
            let mut killed = false;
            loop {
                match rx.try_recv() {
                    Ok(Ok(s)) => break s,
                    Ok(Err(e)) => return Err(e),
                    Err(mpsc::TryRecvError::Empty) => {
                        if cancel.is_cancelled() {
                            match interrupted_at {
                                None => {
                                    process_supervisor::interrupt_child(pid);
                                    interrupted_at = Some(Instant::now());
                                }
                                Some(at) if !killed && at.elapsed() >= super::CANCEL_GRACE => {
                                    process_supervisor::kill_child(pid);
                                    killed = true;
                                }
                                _ => {}
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        return Err(io::Error::other("watcher dropped sender"));
                    }
                }
            }
        };
        self.reaped = true;
        let drains = std::mem::take(&mut self.drains);
        drain_with_deadline(self.pid, drains, &self.abandoned).await;
        if cancel.is_cancelled() {
            return Err(io::Error::other("cancelled"));
        }
        Ok(status)
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        // Killed-without-wait path: SIGKILL the child. The pid was registered
        // with `process_watcher` at spawn time, so the watcher's NOTE_EXIT
        // handler (or the 1s WNOHANG backstop) will call `waitpid` and reap
        // the zombie. We do NOT block on the exit_rx here — that would risk a
        // deadlock on a runtime worker — but dropping it is safe because the
        // watcher reaps before it tries to send on the sender.
        process_supervisor::kill_child(self.pid);
        // Nothing will run `drain_with_deadline` for this child now, so a
        // surviving descendant holding a pipe would leave an `OutputReader`
        // parked with no termination condition. Release it.
        self.abandoned.store(true, Ordering::Release);
    }
}

pub(super) fn spawn(spec: Spec) -> io::Result<Handle> {
    spawn_with(spec, DrainCapacity::Bounded(STREAM_DRAIN_CHUNKS))
}

fn spawn_with(spec: Spec, capacity: DrainCapacity) -> io::Result<Handle> {
    let Spec {
        program,
        args,
        env,
        cwd,
        stdin,
        stdout,
        stderr,
        setsid,
        ctty,
    } = spec;
    let mut cmd = Command::new(&program);
    cmd.args(&args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .current_dir(&cwd)
        .stdin(stdin.into_stdio())
        .stdout(stdout.into_stdio())
        .stderr(stderr.into_stdio());

    if setsid || ctty {
        #[expect(
            clippy::multiple_unsafe_ops_per_block,
            reason = "pre_exec + setsid + ioctl must share one unsafe context"
        )]
        // SAFETY: pre_exec runs between fork and exec; only async-signal-safe
        // syscalls (setsid, ioctl) are invoked.
        unsafe {
            cmd.pre_exec(move || {
                if setsid && libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                if ctty && libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    let mut child = cmd.spawn()?;
    let pid = child.id() as i32;

    let stdin_pump = child.stdin.take().map(|s| StdinPump {
        inner: Arc::new(Mutex::new(Some(s))),
    });

    // One channel, one clone of the sender per drain thread. The original is
    // dropped below so the receiver disconnects exactly when the *last*
    // stream finishes — that disconnect is the merged reader's EOF.
    let (tx, rx) = capacity.channel();
    let mut drains = Vec::with_capacity(2);
    if let Some(s) = child.stdout.take() {
        drains.push(spawn_drain_thread(s, StreamId::Stdout, tx.clone()));
    }
    if let Some(s) = child.stderr.take() {
        drains.push(spawn_drain_thread(s, StreamId::Stderr, tx.clone()));
    }
    drop(tx);
    let abandoned = Arc::new(AtomicBool::new(false));
    let output = (!drains.is_empty()).then_some(OutputReader {
        rx,
        abandoned: Arc::clone(&abandoned),
    });

    let track_guard = process_supervisor::register_child(pid);
    // Register with the kqueue watcher *before* returning the Handle. This
    // guarantees `waitpid` will eventually run on this pid even if the caller
    // drops the Handle without calling `wait*` (e.g. an error path between
    // spawn and wait in pluginexec). Without this registration, Drop's
    // `kill_child` would SIGKILL the pid but nobody would reap it → permanent
    // zombie observed under PPID of main heph.
    let exit_rx = process_watcher::register(pid);

    Ok(Handle {
        pid,
        child: Some(child),
        stdin: stdin_pump,
        output,
        drains,
        abandoned,
        reaped: false,
        exit_rx: Some(exit_rx),
        _track_guard: track_guard,
    })
}

pub(super) async fn output(
    spec: Spec,
    cancel: &(dyn Cancellable + Send + Sync),
) -> io::Result<Output> {
    // Unbounded on purpose. Nothing consumes the channel until the wait below
    // returns, so a bound here would stop the drains, fill the pipes, and
    // wedge the child — see `DrainCapacity`.
    let mut handle = spawn_with(spec, DrainCapacity::Unbounded)?;
    let queued = handle.take_output();

    // No collector needed on this side: the drain threads spawned by
    // `spawn_with` already read the pipes concurrently with the child (so the
    // 64 KiB pipe buffer can never wedge it) and park the chunks in their
    // unbounded channel. `wait_or_cancel` bounds the join on those threads
    // via `drain_with_deadline`, so by the time it returns either every
    // chunk has been queued or the drain has been abandoned.
    //
    // This drops the two `heph-proc-collect` threads a previous version
    // spawned per subprocess. Those threads were the actual hang: the
    // abandoned drain thread still holds its sender, so the collector's
    // `rx.recv()` never returned and `output` joined it under
    // `block_in_place`, parking a tokio worker for the descendant's whole
    // lifetime. The trade is memory: chunks now sit in the channel as
    // separate 8 KiB `Vec`s until the child exits instead of being folded
    // into one growing buffer as they arrive, so peak is ~2x the payload
    // while `take_queued` copies them out. Unbounded, as it was before.
    let status = handle.wait_or_cancel(cancel).await?;

    let (stdout, stderr) = take_queued(queued)?;

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Split everything the drain threads have already queued into the two
/// streams, without blocking.
///
/// Finished drains have dropped their senders, so this walks the channel to
/// `Disconnected` and returns both streams complete. An abandoned drain (a
/// descendant that outlived the child still holds the pipe write end) leaves
/// its sender alive; we stop at `Empty` and return what the child itself
/// wrote rather than parking on bytes that are not ours. Dropping the
/// receiver here also releases the stuck thread the moment its `read`
/// returns, since its next `send` fails.
fn take_queued(reader: Option<OutputReader>) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let Some(reader) = reader else {
        return Ok((Vec::new(), Vec::new()));
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    loop {
        match reader.rx.try_recv() {
            Ok(DrainMsg::Chunk(StreamId::Stdout, chunk)) => stdout.extend_from_slice(&chunk),
            Ok(DrainMsg::Chunk(StreamId::Stderr, chunk)) => stderr.extend_from_slice(&chunk),
            Ok(DrainMsg::Err(id, e)) => return Err(stream_error(id, e)),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
                return Ok((stdout, stderr));
            }
        }
    }
}

fn spawn_drain_thread<R: io::Read + Send + 'static>(
    mut src: R,
    id: StreamId,
    tx: Sender<DrainMsg>,
) -> DrainHandle {
    let done = Arc::new(AtomicBool::new(false));
    let sending = Arc::new(AtomicBool::new(false));
    let done_for_thread = Arc::clone(&done);
    let sending_for_thread = Arc::clone(&sending);
    let jh = std::thread::Builder::new()
        .name("heph-proc-drain".into())
        .spawn(move || {
            let mut buf = vec![0u8; CHUNK_SIZE];
            // Single exit point so `done` is always flipped to true on
            // any return path (EOF, send-error, read-error). The
            // bounded-join in `drain_with_deadline` relies on this flag.
            loop {
                let n = match src.read(&mut buf) {
                    Ok(0) => break, // EOF: drop tx → receiver sees Disconnected
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        // Bracket this send exactly like the chunk path below:
                        // on a full bounded channel it blocks on *our*
                        // consumer, not on the child, and `waiting_on_pipe`
                        // must not misread that as a stray descendant still
                        // holding the read end — that misread escalates to
                        // `killpg` on a pid the watcher may have already
                        // reaped and macOS may have already recycled.
                        sending_for_thread.store(true, Ordering::Release);
                        _ = tx.send(DrainMsg::Err(id, e));
                        sending_for_thread.store(false, Ordering::Release);
                        break;
                    }
                };
                #[expect(
                    clippy::indexing_slicing,
                    reason = "n <= buf.len() by Read::read contract"
                )]
                let slice = buf[..n].to_vec();
                // On a bounded channel this blocks once the consumer is
                // `STREAM_DRAIN_CHUNKS` behind, which stops us reading and
                // lets the pipe fill — deliberate backpressure onto the
                // child's `write(2)`. Published so `drain_with_deadline` can
                // tell that apart from a `read` that will never return; see
                // `DrainHandle::sending`.
                sending_for_thread.store(true, Ordering::Release);
                let handed_over = tx.send(DrainMsg::Chunk(id, slice));
                sending_for_thread.store(false, Ordering::Release);
                if handed_over.is_err() {
                    break; // receiver dropped: stop reading
                }
            }
            done_for_thread.store(true, Ordering::Release);
        })
        .expect("spawn heph-proc-drain thread");
    DrainHandle {
        join: jh,
        done,
        sending,
    }
}
