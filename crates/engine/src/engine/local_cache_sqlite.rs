use crate::engine::local_cache::{
    Existence, LocalCache, NotFoundError, PendingWrite, SizedReader, TargetStream,
};
use anyhow::{Context, Result};
use hcore::hartifactcontent;
use hmodel::htaddr::Addr;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, OpenFlags};
use std::collections::HashMap;
use std::future::{Future, poll_fn};
use std::io::{self, Seek};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Poll, Waker};
use std::thread::JoinHandle;
use std::time::Duration;
use tempfile::SpooledTempFile;

const SPOOL_MEM_THRESHOLD: usize = 1024 * 1024;
pub const DEFAULT_MAX_CONCURRENT_PIPES: usize = 64;
const WRITE_BATCH_MAX: usize = 64;

/// How long `read_pool.get()` waits before giving up.
///
/// Its job is to make exhaustion *diagnosable* rather than to prevent it: see
/// [`read_pool_headroom`], which is a budget and not a guarantee. It stays
/// generous because the alternative failure is worse — a shortage that would have
/// cleared turns into a failed build.
const READ_POOL_TIMEOUT: Duration = Duration::from_secs(30);

/// Read connections to keep beyond the pipe budget, for the callers that take one
/// with **no** pipe permit.
///
/// Sizing the pool at exactly the pipe budget leaves zero headroom, and the pipe
/// path holds its connection for the *consumer's* whole drain — so a burst of
/// streaming reads can own every connection for an unbounded time while `exists`,
/// `list_targets`, `list_target_entries`, `seekable_reader` and the SELECT phase
/// of `reader` are still trying to acquire one. Those then block for
/// [`READ_POOL_TIMEOUT`] on paths that are supposed to be a single indexed
/// lookup.
///
/// It also breaks a scheduling cycle: a queued `rayon::spawn` pipe-copy owns a
/// connection it cannot release until rayon runs it, while a rayon worker sits in
/// `read_pool.get()`. With headroom the unpermitted caller is served from spare
/// capacity and the copy gets to run.
///
/// **A budget, not a proof.** The demand it covers is real threads — tokio
/// workers, the `hcore::blocking` pool, rayon, and on Linux one FUSE session
/// thread per core, each of which takes an unpermitted connection per `read` and
/// per `copy_up` — and under simultaneous peak load from all of them the pool can
/// still be exhausted. Two of the callers are not brief either: `list_targets`'
/// producer holds its connection for a whole GC stream, and `seekable_reader`'s
/// `OwnedBlob` holds one for the reader's lifetime.
///
/// Derived from the pipe budget, so it tracks the parallelism the *caller* asked
/// for (`2 * cfg.parallelism`) rather than what the machine happens to have —
/// `--jobs 4` on a 64-core box should not size a pool for 64-way work. Capped
/// because every connection carries `cache_size = -64000` and `mmap_size = 256
/// MiB`, so this is not a free sum.
fn read_pool_headroom(pipe_limit: usize) -> usize {
    pipe_limit.clamp(16, 64)
}

/// Cap on the bytes one write transaction moves.
///
/// Batching amortizes the commit, but the blob `io::copy` happens *inside* the
/// transaction, so the batch's byte total is how long the single writer thread is
/// unavailable — and every reader of a key in it, plus every reader of a key
/// merely queued behind it, waits on `PendingSlot`'s untimed condvar for that
/// whole time. Counted alone, 64 jobs is anywhere from a few KiB to the spill
/// threshold times 64.
///
/// Whichever limit is reached first ends the batch. A single job larger than this
/// still goes on its own — one blob cannot be split across transactions — so this
/// bounds the batching, not the blob.
const WRITE_BATCH_MAX_BYTES: i64 = 32 * 1024 * 1024;

type Key = (String, String, String);

struct PipeSemaphore {
    count: Mutex<usize>,
    condvar: Condvar,
}

impl PipeSemaphore {
    fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            count: Mutex::new(limit),
            condvar: Condvar::new(),
        })
    }

    /// Blocks the calling thread until a permit is available.
    ///
    /// **Contract:** callers must be on a thread that may block — a dedicated OS
    /// thread (the sqlite writer thread, the `hcore::blocking` pool, a rayon
    /// worker) or a tokio worker that has handed off its core via
    /// `tokio::task::block_in_place`. Calling this directly from a tokio task
    /// parks the worker and can starve the runtime; so does
    /// `hproc::process_supervisor::block_or_inline`, which on Linux *is* inline on
    /// the worker — the cache write path goes through `hcore::blocking` instead.
    fn acquire(self: &Arc<Self>) -> PipePermit {
        let mut count = self.count.lock().expect("pipe semaphore mutex poisoned");
        while *count == 0 {
            count = self
                .condvar
                .wait(count)
                .expect("pipe semaphore condvar wait failed");
        }
        *count -= 1;
        PipePermit { sem: self.clone() }
    }
}

struct PipePermit {
    sem: Arc<PipeSemaphore>,
}

impl Drop for PipePermit {
    fn drop(&mut self) {
        let mut count = self
            .sem
            .count
            .lock()
            .expect("pipe semaphore mutex poisoned in drop");
        *count += 1;
        self.sem.condvar.notify_one();
    }
}

struct GuardedReader<R: io::Read> {
    inner: R,
    _permit: PipePermit,
}

impl<R: io::Read> io::Read for GuardedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

/// One queued command's completion signal, waitable from either side of the
/// async boundary.
///
/// Both kinds of waiter are real: GC and the FUSE reader are on OS threads where
/// parking is exactly right, while the engine's read path is a tokio task where
/// parking is the bug. [`complete`](Self::complete) serves both out of one
/// critical section.
struct PendingSlot {
    state: Mutex<SlotState>,
    cond: Condvar,
}

#[derive(Default)]
struct SlotState {
    done: bool,
    /// Tasks awaiting this slot.
    ///
    /// Raw wakers rather than a `tokio::sync::Notify`/`oneshot` so the slot needs
    /// no runtime to exist: a cdylib plugin's futures are polled by host workers
    /// with no reactor of the plugin's own, and any tokio timer/IO type there
    /// panics across the `extern "C"` seam, which aborts. Plain waker traffic is
    /// safe (same stance as `hcore::blocking`).
    wakers: Vec<Waker>,
}

impl PendingSlot {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(SlotState::default()),
            cond: Condvar::new(),
        })
    }

    /// Park the calling thread until the command lands. For OS-thread callers
    /// only — see [`PendingSlot`].
    fn wait(&self) {
        let mut state = self.state.lock().expect("pending slot mutex poisoned");
        while !state.done {
            state = self
                .cond
                .wait(state)
                .expect("pending slot condvar wait failed");
        }
    }

    /// Suspend the calling *task* until the command lands, leaving its worker
    /// free to poll everything else.
    fn wait_async(self: &Arc<Self>) -> impl Future<Output = ()> + Send + 'static {
        let slot = self.clone();
        // Held for the whole wait and dropped with the future, so the
        // registration never outlives what it is waking. See `hcore::blocking`.
        let armed = hcore::blocking::Backstop::new();
        poll_fn(move |cx| {
            let mut state = slot.state.lock().expect("pending slot mutex poisoned");
            if state.done {
                return Poll::Ready(());
            }
            if !state.wakers.iter().any(|w| w.will_wake(cx.waker())) {
                state.wakers.push(cx.waker().clone());
            }
            drop(state);
            // The wake-up is issued by the sqlite writer thread, off-runtime —
            // the same dropped-cross-thread-wake-up exposure `hcore::blocking`
            // documents, so the same backstop. A lost wake costs latency instead
            // of stranding the task.
            armed.arm(cx.waker());
            Poll::Pending
        })
    }

    fn complete(&self) {
        let wakers = {
            let mut state = self.state.lock().expect("pending slot mutex poisoned");
            state.done = true;
            std::mem::take(&mut state.wakers)
        };
        self.cond.notify_all();
        // Woken with the lock released: a wake can poll the future inline, and
        // that poll takes this same mutex.
        for waker in wakers {
            waker.wake();
        }
    }
}

#[derive(Default)]
struct PendingTracker {
    /// Key → the slot that will be the *last* to complete for that key.
    ///
    /// Upheld by [`register_and_send`](Self::register_and_send) holding this lock
    /// across the channel send, so map order is channel order and the writer
    /// thread's FIFO drain completes slots in registration order.
    map: Mutex<HashMap<Key, Arc<PendingSlot>>>,
}

impl PendingTracker {
    /// Register a completion slot for `key` and queue the command it belongs to,
    /// in one critical section.
    ///
    /// The lock spans the send because the tracker's whole contract is that the
    /// slot a reader finds for a key is the last one to complete for it.
    /// Registering and sending separately breaks that: two writers of one key
    /// interleave as *A registers, B registers, B sends, A sends*, leaving the map
    /// pointing at B's slot while the channel completes it first — the reader
    /// wakes and reads bytes A is about to overwrite. The send is a push onto an
    /// unbounded channel, so the critical section stays a pointer swap either way.
    ///
    /// Registration deliberately happens here, at enqueue, and not when a writer
    /// is *opened*: the window a reader can be made to wait for is the queue→commit
    /// gap, which the writer thread bounds. Registering at open would stretch it
    /// across the caller's whole streaming write — a remote blob's download and
    /// gunzip, or a target's tar and compress — which nothing bounds.
    /// `None` when the writer thread is gone and nothing could be queued.
    fn register_and_send(
        &self,
        tx: &mpsc::Sender<WriterCmd>,
        key: Key,
        make: impl FnOnce(Key, Arc<PendingSlot>) -> WriterCmd,
    ) -> Option<Arc<PendingSlot>> {
        let slot = PendingSlot::new();
        let cmd = make(key.clone(), slot.clone());
        let mut m = self.map.lock().expect("pending tracker poisoned");
        let superseded = m.insert(key.clone(), slot.clone());
        if tx.send(cmd).is_ok() {
            return Some(slot);
        }
        // The send fails only when the receiver is gone, i.e. the writer thread
        // died — which also dropped every command still in the channel. So neither
        // this slot nor the one it displaced will ever be completed by anyone.
        // Clear the key and complete the displaced slot by hand, or a reader that
        // already found it waits on a commit that is never coming: an OS thread
        // parks forever, and a task hangs on a `PendingWrite` with no timeout and
        // nothing to see. Our own slot was never published — the insert and this
        // rollback are one critical section — so it needs no completion.
        m.remove(&key);
        drop(m);
        if let Some(previous) = superseded {
            previous.complete();
        }
        None
    }

    /// The in-flight slot for a key, for a caller that wants to await the commit
    /// rather than park on it. `None` — the common case — means a probe of this
    /// key will not block.
    fn pending(&self, addr: &str, hashin: &str, name: &str) -> Option<Arc<PendingSlot>> {
        let m = self.map.lock().expect("pending tracker poisoned");
        // Idle map short-circuits before the scan; the caller already paid for the
        // formatted addr, which its own lookup needs either way.
        if m.is_empty() {
            return None;
        }
        Self::find(&m, addr, hashin, name)
    }

    fn wait_if_pending(&self, addr: &str, hashin: &str, name: &str) {
        let slot_opt = {
            let m = self.map.lock().expect("pending tracker poisoned");
            Self::find(&m, addr, hashin, name)
        };
        if let Some(slot) = slot_opt {
            slot.wait();
        }
    }

    /// Map holds only in-flight writes/deletes (typically empty on the read hot
    /// path), so a borrowed scan beats allocating an owned `Key` tuple purely to
    /// call `get`.
    fn find(
        m: &HashMap<Key, Arc<PendingSlot>>,
        addr: &str,
        hashin: &str,
        name: &str,
    ) -> Option<Arc<PendingSlot>> {
        m.iter()
            .find(|(k, _)| k.0 == addr && k.1 == hashin && k.2 == name)
            .map(|(_, slot)| slot.clone())
    }

    fn complete(&self, key: &Key, slot: &Arc<PendingSlot>) {
        {
            let mut m = self.map.lock().expect("pending tracker poisoned");
            // Only remove if this slot is still the latest registered. Otherwise a newer
            // write has superseded ours and owns the map entry.
            if let Some(current) = m.get(key)
                && Arc::ptr_eq(current, slot)
            {
                m.remove(key);
            }
        }
        slot.complete();
    }
}

struct WriteJob {
    key: Key,
    buf: SpooledTempFile,
    size: i64,
    slot: Arc<PendingSlot>,
}

struct DeleteJob {
    key: Key,
    slot: Arc<PendingSlot>,
}

enum WriterCmd {
    Write(WriteJob),
    Delete(DeleteJob),
}

/// Test-only brake on the writer thread, held closed while a test observes the
/// queued-but-uncommitted states.
///
/// `Existence::Queued`, a superseded write and a queued delete all live in the
/// window between enqueue and commit, which the writer thread closes in
/// microseconds — racing it produces flaky tests that mostly assert nothing. The
/// whole type and its call site are `#[cfg(test)]`, so production keeps the
/// unbraked loop.
#[cfg(test)]
#[derive(Default)]
struct WriterGate {
    closed: Mutex<bool>,
    cond: Condvar,
}

#[cfg(test)]
impl WriterGate {
    /// Called by the writer thread before it drains each batch.
    fn wait_while_closed(&self) {
        let mut closed = self.closed.lock().expect("writer gate poisoned");
        while *closed {
            closed = self.cond.wait(closed).expect("writer gate wait failed");
        }
    }

    fn close(&self) {
        *self.closed.lock().expect("writer gate poisoned") = true;
    }

    fn open(&self) {
        *self.closed.lock().expect("writer gate poisoned") = false;
        self.cond.notify_all();
    }
}

pub struct LocalCacheSQLite {
    read_pool: r2d2::Pool<SqliteConnectionManager>,
    writer_tx: Option<mpsc::Sender<WriterCmd>>,
    writer_handle: Option<JoinHandle<()>>,
    pending: Arc<PendingTracker>,
    pipe_sem: Arc<PipeSemaphore>,
    inline_threshold: usize,
    #[cfg(test)]
    gate: Arc<WriterGate>,
}

impl LocalCacheSQLite {
    pub fn with_pipe_limit(
        db_path: PathBuf,
        inline_threshold: usize,
        pipe_limit: usize,
    ) -> Result<Self> {
        let pipe_limit = pipe_limit.max(DEFAULT_MAX_CONCURRENT_PIPES);
        Self::with_pool_config(
            db_path,
            inline_threshold,
            pipe_limit,
            read_pool_headroom(pipe_limit),
            READ_POOL_TIMEOUT,
        )
    }

    /// [`Self::with_pipe_limit`] with the pool knobs given rather than derived:
    /// without the [`DEFAULT_MAX_CONCURRENT_PIPES`] floor a test can saturate the
    /// pipe budget without writing 64 blobs, and with an explicit headroom and
    /// timeout it can reach the exhaustion path in milliseconds instead of 30
    /// seconds.
    fn with_pool_config(
        db_path: PathBuf,
        inline_threshold: usize,
        pipe_limit: usize,
        headroom: usize,
        read_pool_timeout: Duration,
    ) -> Result<Self> {
        let read_pool_size = u32::try_from(pipe_limit.saturating_add(headroom))
            .unwrap_or(u32::MAX)
            .max(1);
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating sqlite cache dir {parent:?}"))?;
        }

        let mut write_conn = Connection::open(&db_path)
            .with_context(|| format!("opening sqlite cache at {db_path:?}"))?;

        write_conn
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA busy_timeout = 10000;
                 PRAGMA synchronous = NORMAL;
                 PRAGMA foreign_keys = ON;
                 PRAGMA temp_store = MEMORY;
                 PRAGMA auto_vacuum = INCREMENTAL;
                 PRAGMA page_size = 8192;
                 PRAGMA cache_size = -64000;
                 PRAGMA mmap_size = 268435456;
                 CREATE TABLE IF NOT EXISTS artifacts (
                     addr   TEXT NOT NULL,
                     hashin TEXT NOT NULL,
                     name   TEXT NOT NULL,
                     data   BLOB NOT NULL,
                     PRIMARY KEY (addr, hashin, name)
                 );
                 CREATE INDEX IF NOT EXISTS idx_artifacts_addr_hashin ON artifacts (addr, hashin);",
            )
            .context("initialising sqlite cache schema")?;

        let manager = SqliteConnectionManager::file(&db_path)
            .with_flags(OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_init(|conn| {
                conn.execute_batch(
                    "PRAGMA busy_timeout = 10000;
                     PRAGMA synchronous = NORMAL;
                     PRAGMA temp_store = MEMORY;
                     PRAGMA cache_size = -64000;
                     PRAGMA mmap_size = 268435456;",
                )
            });

        let read_pool = r2d2::Pool::builder()
            .max_size(read_pool_size)
            .min_idle(Some(1))
            // Explicit rather than inherited: this bound is load-bearing for the
            // stall it converts into an error, and it should not move because
            // r2d2 changed a default.
            .connection_timeout(read_pool_timeout)
            // Likewise pinned. It defaults to `true`, running `manager.is_valid()`
            // on every checkout — free today only because `r2d2_sqlite`'s
            // `is-valid` feature is off, so that call is `execute_batch("")`. One
            // feature unification away it becomes a round trip per checkout, i.e.
            // per FUSE read, from an unrelated crate.
            .test_on_check_out(false)
            .build(manager)
            .context("building sqlite read connection pool")?;

        let pending = Arc::new(PendingTracker::default());
        let (writer_tx, writer_rx) = mpsc::channel::<WriterCmd>();
        let pending_bg = pending.clone();
        #[cfg(test)]
        let gate = Arc::new(WriterGate::default());
        #[cfg(test)]
        let gate_bg = gate.clone();
        let writer_handle = std::thread::Builder::new()
            .name("heph-sqlite-writer".to_string())
            .spawn(move || {
                writer_loop(
                    &mut write_conn,
                    &writer_rx,
                    &pending_bg,
                    #[cfg(test)]
                    &gate_bg,
                )
            })
            .context("spawning sqlite writer thread")?;

        Ok(Self {
            read_pool,
            writer_tx: Some(writer_tx),
            writer_handle: Some(writer_handle),
            pending,
            pipe_sem: PipeSemaphore::new(pipe_limit),
            inline_threshold,
            #[cfg(test)]
            gate,
        })
    }

    fn key(addr: &Addr) -> String {
        addr.format()
    }

    /// A pooled read connection.
    ///
    /// Failure here is the connection timeout elapsing far more often than sqlite
    /// being broken, and the two have nothing in common to investigate — so the
    /// message names the pool rather than the query. r2d2's own error is kept as
    /// the source, which is what distinguishes "waited and nothing freed up" from
    /// "could not open a connection at all".
    fn read_conn(&self) -> Result<r2d2::PooledConnection<SqliteConnectionManager>> {
        self.read_pool.get().with_context(|| {
            format!(
                "acquiring a sqlite read connection: pool of {} exhausted",
                self.read_pool.max_size(),
            )
        })
    }

    fn writer_tx(&self) -> Result<&mpsc::Sender<WriterCmd>> {
        self.writer_tx
            .as_ref()
            .context("sqlite cache writer thread has shut down")
    }

    /// The presence of a key in the *committed* state, with no regard for the
    /// write queue. One indexed point lookup on a mmap'd read connection; it
    /// never blocks on the writer thread.
    fn exists_committed(&self, addr_key: &str, hashin: &str, name: &str) -> Result<bool> {
        let conn = self.read_conn()?;

        let mut stmt = conn
            .prepare_cached(
                "SELECT 1 FROM artifacts WHERE addr=?1 AND hashin=?2 AND name=?3 LIMIT 1",
            )
            .context("preparing exists lookup")?;
        match stmt.query_row(rusqlite::params![addr_key, hashin, name], |_| Ok(())) {
            Ok(()) => Ok(true),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(e) => Err(e).context("checking artifact existence in sqlite cache"),
        }
    }
}

impl Drop for LocalCacheSQLite {
    fn drop(&mut self) {
        // Close the channel so the writer thread observes a Disconnected and exits cleanly.
        self.writer_tx = None;
        if let Some(handle) = self.writer_handle.take() {
            // If join fails (panic in bg thread), there's nothing useful to do here.
            drop(handle.join());
        }
    }
}

/// Releases every slot still registered when the writer thread goes away, so that
/// a reader waiting on one falls through to the DB and observes NotFound.
///
/// A panic anywhere in [`writer_loop`] used to leave the whole map registered
/// forever. That parked an OS thread — visible in a stack dump — but now an
/// awaiting task would hang on a `PendingWrite` that never resolves, with no
/// timeout, no error and nothing to see. Draining on unwind turns a silent hang
/// back into a cache miss.
struct DrainOnExit<'a>(&'a PendingTracker);

impl Drop for DrainOnExit<'_> {
    fn drop(&mut self) {
        let stranded: Vec<Arc<PendingSlot>> = {
            let mut m = self.0.map.lock().expect("pending tracker poisoned");
            m.drain().map(|(_, slot)| slot).collect()
        };
        if !stranded.is_empty() {
            tracing::error!(
                slots = stranded.len(),
                "sqlite cache writer thread exited with writes still queued; releasing waiters"
            );
        }
        for slot in stranded {
            slot.complete();
        }
    }
}

/// Bytes a command will move inside the transaction. A delete moves none — it is
/// one indexed `DELETE`, and batching those freely is the point.
fn write_bytes(cmd: &WriterCmd) -> i64 {
    match cmd {
        WriterCmd::Write(job) => job.size,
        WriterCmd::Delete(_) => 0,
    }
}

/// Take `first` plus whatever is already queued, up to [`WRITE_BATCH_MAX`] jobs
/// or [`WRITE_BATCH_MAX_BYTES`] of blob data — whichever comes first.
fn collect_batch(first: WriterCmd, rx: &mpsc::Receiver<WriterCmd>) -> Vec<WriterCmd> {
    let mut batch = Vec::with_capacity(WRITE_BATCH_MAX);
    let mut batch_bytes = write_bytes(&first);
    batch.push(first);
    // The byte check is *before* the `try_recv`, so a first job that is already
    // over the cap goes alone rather than dragging a second one in with it.
    while batch.len() < WRITE_BATCH_MAX && batch_bytes < WRITE_BATCH_MAX_BYTES {
        match rx.try_recv() {
            Ok(cmd) => {
                batch_bytes = batch_bytes.saturating_add(write_bytes(&cmd));
                batch.push(cmd);
            }
            Err(_) => break,
        }
    }
    batch
}

fn writer_loop(
    conn: &mut Connection,
    rx: &mpsc::Receiver<WriterCmd>,
    pending: &PendingTracker,
    #[cfg(test)] gate: &WriterGate,
) {
    let _drain = DrainOnExit(pending);
    loop {
        let first = match rx.recv() {
            Ok(cmd) => cmd,
            Err(_) => return,
        };

        #[cfg(test)]
        gate.wait_while_closed();

        let mut batch = collect_batch(first, rx);

        if let Err(e) = process_batch(conn, &mut batch) {
            tracing::error!(error = %format!("{e:#}"), "sqlite cache writer: batch failed");
        }

        // Whether the batch succeeded or not, the pending slots must be released so that
        // readers don't hang. On failure the readers will simply observe NotFound from the
        // DB, which is the correct behavior for a write that didn't land.
        for cmd in &batch {
            match cmd {
                WriterCmd::Write(j) => pending.complete(&j.key, &j.slot),
                WriterCmd::Delete(j) => pending.complete(&j.key, &j.slot),
            }
        }
    }
}

fn process_batch(conn: &mut Connection, batch: &mut [WriterCmd]) -> Result<()> {
    let tx = conn
        .transaction()
        .context("starting sqlite write transaction")?;

    for cmd in batch.iter_mut() {
        match cmd {
            WriterCmd::Write(job) => {
                tx.execute(
                    "INSERT OR REPLACE INTO artifacts (addr, hashin, name, data) \
                     VALUES (?1, ?2, ?3, zeroblob(?4))",
                    rusqlite::params![job.key.0, job.key.1, job.key.2, job.size],
                )
                .with_context(|| {
                    format!(
                        "inserting artifact {addr}/{hashin}/{name}",
                        addr = job.key.0,
                        hashin = job.key.1,
                        name = job.key.2
                    )
                })?;
                let row_id = tx.last_insert_rowid();
                let mut blob = tx
                    .blob_open(rusqlite::MAIN_DB, "artifacts", "data", row_id, false)
                    .with_context(|| format!("opening blob for {}", job.key.2))?;
                job.buf
                    .seek(io::SeekFrom::Start(0))
                    .with_context(|| format!("rewinding spool for {}", job.key.2))?;
                io::copy(&mut job.buf, &mut blob)
                    .with_context(|| format!("writing blob for {}", job.key.2))?;
            }
            WriterCmd::Delete(job) => {
                tx.execute(
                    "DELETE FROM artifacts WHERE addr=?1 AND hashin=?2 AND name=?3",
                    rusqlite::params![job.key.0, job.key.1, job.key.2],
                )
                .with_context(|| {
                    format!(
                        "deleting artifact {addr}/{hashin}/{name}",
                        addr = job.key.0,
                        hashin = job.key.1,
                        name = job.key.2
                    )
                })?;
            }
        }
    }

    tx.commit().context("committing sqlite write transaction")?;
    Ok(())
}

impl LocalCache for LocalCacheSQLite {
    fn reader(&self, addr: &Addr, hashin: &str, name: &str) -> Result<SizedReader> {
        let addr_key = Self::key(addr);
        self.pending.wait_if_pending(&addr_key, hashin, name);

        let conn = self.read_conn()?;

        let mut stmt = conn
            .prepare_cached(
                "SELECT rowid, length(data) FROM artifacts WHERE addr=?1 AND hashin=?2 AND name=?3",
            )
            .context("preparing reader lookup")?;
        let (row_id, blob_len): (i64, usize) =
            match stmt.query_row(rusqlite::params![addr_key, hashin, name], |row| {
                let row_id: i64 = row.get(0)?;
                let blob_len: i64 = row.get(1)?;
                Ok((row_id, usize::try_from(blob_len).unwrap_or(0)))
            }) {
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    return Err(anyhow::anyhow!(NotFoundError));
                }
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("looking up {name} hashin={hashin} in sqlite cache")
                    });
                }
                Ok(v) => v,
            };

        let size = blob_len as u64;

        if blob_len <= self.inline_threshold {
            let mut blob = conn
                .blob_open(rusqlite::MAIN_DB, "artifacts", "data", row_id, true)
                .with_context(|| format!("opening blob for {name}"))?;
            let mut buf = Vec::with_capacity(blob_len);
            io::copy(&mut blob, &mut buf)
                .with_context(|| format!("reading small blob for {name}"))?;
            let arc: Arc<[u8]> = Arc::from(buf);
            return Ok(SizedReader {
                size,
                reader: Box::new(io::Cursor::new(arc.clone())),
                bytes: Some(arc),
            });
        }

        // Release the SELECT connection before acquiring semaphore + a fresh pipe connection.
        drop(stmt);
        drop(conn);

        // Semaphore acquired before pool to bound concurrent open pipes (= open FDs).
        let permit = self.pipe_sem.acquire();
        let conn = self.read_conn()?;

        let (pipe_reader, mut pipe_writer) =
            io::pipe().with_context(|| format!("creating pipe for sqlite blob read of {name}"))?;

        // Move the pooled connection into the rayon pool; returns to pool on drop.
        rayon::spawn(move || {
            let mut blob =
                match conn.blob_open(rusqlite::MAIN_DB, "artifacts", "data", row_id, true) {
                    Ok(b) => b,
                    Err(_) => return,
                };
            drop(io::copy(&mut blob, &mut pipe_writer));
        });

        Ok(SizedReader {
            size,
            reader: Box::new(GuardedReader {
                inner: pipe_reader,
                _permit: permit,
            }),
            bytes: None,
        })
    }

    fn writer(&self, addr: &Addr, hashin: &str, name: &str) -> Result<Box<dyn io::Write>> {
        // No pending registration here: the key becomes pending when the finished
        // spool is *queued*, in `SqliteCacheWriter::drop`. Registering at open
        // would make every reader of the key wait out the caller's whole
        // streaming write — see `PendingTracker::register_and_send`.
        Ok(Box::new(SqliteCacheWriter {
            writer_tx: self.writer_tx()?.clone(),
            pending: self.pending.clone(),
            key: Some((Self::key(addr), hashin.to_string(), name.to_string())),
            buf: Some(SpooledTempFile::new(SPOOL_MEM_THRESHOLD)),
            size: 0,
        }))
    }

    fn exists(&self, addr: &Addr, hashin: &str, name: &str) -> Result<bool> {
        let addr_key = Self::key(addr);
        self.pending.wait_if_pending(&addr_key, hashin, name);
        self.exists_committed(&addr_key, hashin, name)
    }

    fn existence(&self, addr: &Addr, hashin: &str, name: &str) -> Result<Existence> {
        // Report the queue rather than wait on it — deliberately *not*
        // `self.exists`, whose `wait_if_pending` would park on a slot registered
        // after the check below and put the caller's thread right back on the
        // condvar this method exists to avoid.
        //
        // Nothing here blocks: a write landing between the check and the SELECT
        // simply isn't seen, and "absent" is a legitimate answer for a reader with
        // no happens-before against a concurrent write.
        let addr_key = Self::key(addr);
        if let Some(slot) = self.pending.pending(&addr_key, hashin, name) {
            return Ok(Existence::Queued(PendingWrite::new(slot.wait_async())));
        }
        Ok(Existence::Committed(
            self.exists_committed(&addr_key, hashin, name)?,
        ))
    }

    fn exists_committed(&self, addr: &Addr, hashin: &str, name: &str) -> Result<bool> {
        // The inherent `exists_committed` is exactly this method's contract: one
        // indexed point lookup, no `wait_if_pending`.
        Self::exists_committed(self, &Self::key(addr), hashin, name)
    }

    fn list_targets(&self) -> Result<TargetStream> {
        // Stream distinct addrs over a bounded channel: the producer holds one
        // pooled connection and a `SELECT DISTINCT addr` cursor on a dedicated
        // thread, the consumer pulls one addr at a time. Bounded so a slow
        // consumer (GC locking/resolving each target) applies backpressure
        // instead of buffering every target in memory.
        let conn = self.read_conn()?;
        let (tx, rx) = mpsc::sync_channel::<Result<String>>(256);
        // Dedicated thread (not rayon) so a saturated rayon pool can't starve
        // this long-lived producer and deadlock the consumer on `recv`.
        std::thread::Builder::new()
            .name("heph-gc-list-targets".to_string())
            .spawn(move || {
                let mut stmt = match conn.prepare("SELECT DISTINCT addr FROM artifacts") {
                    Ok(s) => s,
                    Err(e) => {
                        drop(tx.send(Err(anyhow::Error::from(e).context("prepare list_targets"))));
                        return;
                    }
                };
                match stmt.query_map([], |row| row.get::<_, String>(0)) {
                    Ok(rows) => {
                        for row in rows {
                            let item = row.context("read target addr row");
                            if tx.send(item).is_err() {
                                break; // consumer dropped
                            }
                        }
                    }
                    Err(e) => {
                        drop(tx.send(Err(anyhow::Error::from(e).context("query list_targets"))));
                    }
                }
            })
            .context("spawning gc list-targets thread")?;
        Ok(Box::new(rx.into_iter()))
    }

    fn list_target_entries(&self, addr: &Addr) -> Result<Vec<String>> {
        let key = Self::key(addr);
        let conn = self.read_conn()?;
        let mut stmt = conn
            .prepare_cached("SELECT DISTINCT hashin FROM artifacts WHERE addr=?1")
            .context("preparing list_target_entries query")?;
        let rows = stmt
            .query_map([&key], |row| row.get::<_, String>(0))
            .with_context(|| format!("listing entries for {addr}"))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .with_context(|| format!("collecting entries for {addr}"))
    }

    fn delete(&self, addr: &Addr, hashin: &str, name: &str) -> Result<()> {
        let key = (Self::key(addr), hashin.to_string(), name.to_string());
        let slot = self
            .pending
            .register_and_send(self.writer_tx()?, key, |key, slot| {
                WriterCmd::Delete(DeleteJob { key, slot })
            })
            .context("queueing a cache delete: sqlite writer thread is gone")?;
        // A delete must have landed before the caller moves on — GC counts freed
        // bytes against it — so this waits rather than reporting the queue like
        // `existence` does. That makes `delete` a thread-parking call by contract:
        // its callers (`gc_entry`, `SpillWriter::spill`) must be on the blocking
        // pool or a dedicated OS thread, never on a runtime worker.
        slot.wait();
        Ok(())
    }

    fn seekable_reader(
        &self,
        addr: &Addr,
        hashin: &str,
        name: &str,
    ) -> Result<Option<Box<dyn hartifactcontent::ReadSeek + Send>>> {
        let addr_key = Self::key(addr);
        self.pending.wait_if_pending(&addr_key, hashin, name);

        let conn = self.read_conn()?;

        let mut stmt = conn
            .prepare_cached("SELECT rowid FROM artifacts WHERE addr=?1 AND hashin=?2 AND name=?3")
            .context("preparing seekable_reader lookup")?;
        let row_id: i64 =
            match stmt.query_row(rusqlite::params![addr_key, hashin, name], |row| row.get(0)) {
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    return Err(anyhow::anyhow!(NotFoundError));
                }
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("looking up {name} hashin={hashin} for seekable read")
                    });
                }
                Ok(v) => v,
            };
        drop(stmt);

        Ok(Some(Box::new(OwnedBlob::new(conn, row_id)?)))
    }
}

/// Owns both a pooled sqlite connection and a `Blob` opened against it.
///
/// `rusqlite::blob::Blob<'conn>` borrows its connection; lifetime extension
/// to `'static` is sound because the blob is dropped before `_conn` (Rust
/// drops struct fields in declaration order).
struct OwnedBlob {
    blob: rusqlite::blob::Blob<'static>,
    _conn: r2d2::PooledConnection<SqliteConnectionManager>,
}

// SAFETY: rusqlite::Connection is Send. The blob holds a raw sqlite3
// statement pointer whose ownership transfers with the connection. Both
// fields are Send-compatible; the borrow we extended to 'static is local
// to this struct and never observed externally.
unsafe impl Send for OwnedBlob {}

impl OwnedBlob {
    fn new(conn: r2d2::PooledConnection<SqliteConnectionManager>, row_id: i64) -> Result<Self> {
        let conn_ref: &Connection = &conn;
        let blob = conn_ref
            .blob_open(rusqlite::MAIN_DB, "artifacts", "data", row_id, true)
            .context("opening seekable sqlite blob")?;
        // SAFETY: `blob` borrows from `conn` which is owned alongside it in
        // the returned struct; struct field drop order (blob before _conn)
        // guarantees the borrow outlives no longer than the connection.
        let blob_static: rusqlite::blob::Blob<'static> = unsafe { std::mem::transmute(blob) };
        Ok(Self {
            blob: blob_static,
            _conn: conn,
        })
    }
}

impl io::Read for OwnedBlob {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.blob.read(buf)
    }
}

impl io::Seek for OwnedBlob {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        self.blob.seek(pos)
    }
}

struct SqliteCacheWriter {
    writer_tx: mpsc::Sender<WriterCmd>,
    pending: Arc<PendingTracker>,
    key: Option<Key>,
    buf: Option<SpooledTempFile>,
    size: usize,
}

impl io::Write for SqliteCacheWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self
            .buf
            .as_mut()
            .expect("writer buffer missing")
            .write(buf)?;
        self.size += n;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.buf.as_mut().expect("writer buffer missing").flush()
    }
}

impl Drop for SqliteCacheWriter {
    fn drop(&mut self) {
        let (Some(key), Some(buf)) = (self.key.take(), self.buf.take()) else {
            return;
        };

        let Ok(size) = i64::try_from(self.size) else {
            // Pathological size. Nothing was ever registered for this key — that
            // now happens at enqueue — so dropping the write strands no waiter.
            tracing::error!(
                addr = key.0,
                hashin = key.1,
                name = key.2,
                size = self.size,
                "sqlite cache write is too large to address; dropping it"
            );
            return;
        };

        if self
            .pending
            .register_and_send(&self.writer_tx, key, move |key, slot| {
                WriterCmd::Write(WriteJob {
                    key,
                    buf,
                    size,
                    slot,
                })
            })
            .is_none()
        {
            // Writer thread is gone; the registration was rolled back inside
            // `register_and_send`, so readers fall through to the DB and observe
            // NotFound rather than waiting for a commit that will never come.
            tracing::error!("sqlite cache writer thread is gone; write dropped");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use tempfile::tempdir;

    /// A write command of a declared size. The spool is empty — `collect_batch`
    /// only reads `size`, which is what the real writer records too.
    fn write_cmd(name: &str, size: i64) -> WriterCmd {
        WriterCmd::Write(WriteJob {
            key: ("//pkg:t".to_string(), "h".to_string(), name.to_string()),
            buf: SpooledTempFile::new(SPOOL_MEM_THRESHOLD),
            size,
            slot: PendingSlot::new(),
        })
    }

    /// The blob `io::copy` runs *inside* the write transaction, so the batch's
    /// byte total is how long the single writer thread is unavailable — and every
    /// reader of a key in that batch, or merely queued behind it, waits on an
    /// untimed condvar for exactly that long. Counting to 64 says nothing about
    /// it: 64 jobs is anywhere from a few KiB to 64× the spill threshold.
    #[test]
    fn a_write_batch_is_bounded_by_bytes_as_well_as_count() {
        let (tx, rx) = mpsc::channel();
        // Twenty jobs at a tenth of the cap each — well under the count bound of
        // `WRITE_BATCH_MAX`, so only the byte cap can stop this batch.
        const JOBS: usize = 20;
        let each = WRITE_BATCH_MAX_BYTES / 10;
        for i in 0..JOBS {
            tx.send(write_cmd(&format!("blob{i}"), each)).expect("send");
        }

        let first = rx.recv().expect("first");
        let batch = collect_batch(first, &rx);

        assert!(
            batch.len() < JOBS,
            "the byte cap must end the batch early, took all {} jobs",
            batch.len()
        );
        let bytes: i64 = batch.iter().map(write_bytes).sum();
        assert!(
            bytes <= WRITE_BATCH_MAX_BYTES + each,
            "a batch may only overshoot by its last job, took {bytes} bytes"
        );
    }

    /// One blob cannot be split across transactions, so an over-cap job goes
    /// alone rather than being joined by another.
    #[test]
    fn an_oversized_write_is_batched_alone() {
        let (tx, rx) = mpsc::channel();
        tx.send(write_cmd("huge", WRITE_BATCH_MAX_BYTES * 4))
            .expect("send");
        tx.send(write_cmd("small", 1)).expect("send");

        let first = rx.recv().expect("first");
        let batch = collect_batch(first, &rx);

        assert_eq!(
            batch.len(),
            1,
            "an already-over-cap job must not drag another into its transaction"
        );
    }

    /// Deletes move no bytes — one indexed `DELETE` each — so batching them
    /// freely is the whole point and the byte cap must not throttle them.
    #[test]
    fn deletes_do_not_count_against_the_byte_cap() {
        let (tx, rx) = mpsc::channel();
        for i in 0..WRITE_BATCH_MAX {
            tx.send(WriterCmd::Delete(DeleteJob {
                key: ("//pkg:t".to_string(), "h".to_string(), format!("blob{i}")),
                slot: PendingSlot::new(),
            }))
            .expect("send");
        }

        let first = rx.recv().expect("first");
        let batch = collect_batch(first, &rx);

        assert_eq!(
            batch.len(),
            WRITE_BATCH_MAX,
            "deletes should still batch up to the count bound"
        );
    }

    fn make_addr(pkg: &str, name: &str) -> hmodel::htaddr::Addr {
        hmodel::htaddr::Addr::new(
            hmodel::htpkg::PkgBuf::from(pkg),
            name.to_string(),
            Default::default(),
        )
    }

    /// Blobs whose streaming reads will park with a pooled connection held.
    ///
    /// Deliberately small: each held reader leaves a `rayon::spawn`'d `io::copy`
    /// blocked writing into a full pipe, and rayon's pool is global to the test
    /// binary. Taking more than a couple would stall unrelated tests that read a
    /// blob, and the flake would be reported against them.
    const HELD_PIPES: usize = 2;

    /// Writes `HELD_PIPES` oversized blobs and returns readers for all of them,
    /// undrained — so every pipe permit and its pooled connection stays taken.
    fn saturate_pipes(cache: &LocalCacheSQLite, addr: &Addr) -> Result<Vec<SizedReader>> {
        // Over the pipe buffer, so the copy blocks with the connection held
        // rather than finishing and handing it straight back.
        let payload = vec![7u8; 4 * 1024 * 1024];
        for i in 0..HELD_PIPES {
            let mut w = cache.writer(addr, "h", &format!("blob{i}"))?;
            w.write_all(&payload)?;
            drop(w);
        }
        (0..HELD_PIPES)
            .map(|i| cache.reader(addr, "h", &format!("blob{i}")))
            .collect()
    }

    /// Runs `f` on another thread and fails if it has not answered within 5s.
    ///
    /// Racing against a deadline rather than measuring: on a loaded CI box a
    /// wall-clock assertion on an unblocked lookup is a coin flip, but "did it
    /// beat 5s" separates it cleanly from a pool parked for the full timeout.
    fn answers_promptly<T: Send + 'static>(
        what: &str,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || drop(tx.send(f())));
        rx.recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("{what} must not queue behind a saturated pool"))
    }

    /// Streaming reads hold a pooled connection for the *consumer's* whole drain.
    /// Sized at exactly the pipe budget, a handful of undrained readers therefore
    /// owned every connection in the pool, and the callers that take one with no
    /// pipe permit at all — `exists` here, but equally `list_targets`,
    /// `list_target_entries` and `seekable_reader` — blocked behind them for
    /// `READ_POOL_TIMEOUT` on what is a single indexed lookup.
    #[test]
    fn undrained_streaming_reads_do_not_starve_an_indexed_lookup() -> Result<()> {
        let dir = tempdir()?;
        let cache = Arc::new(LocalCacheSQLite::with_pool_config(
            dir.path().join("cache.db"),
            // Everything takes the pipe path, nothing is served inline.
            0,
            HELD_PIPES,
            read_pool_headroom(HELD_PIPES),
            READ_POOL_TIMEOUT,
        )?);

        let addr = make_addr("pkg", "t");
        let _held = saturate_pipes(&cache, &addr)?;

        let found = answers_promptly(
            "exists",
            enclose::enclose!((cache, addr) move || cache.exists(&addr, "h", "blob0")),
        )?;
        assert!(found, "the blob was written, so exists must report it");

        Ok(())
    }

    /// The pipe path is the *bursty* holder; `list_targets` is the permanent one.
    /// Its producer thread keeps a pooled connection and an open cursor for the
    /// whole GC stream, and blocks in `send` on a bounded channel — so a consumer
    /// that stops pulling parks that connection indefinitely. Headroom has to
    /// survive its own named long-hold callers, not just a burst.
    #[test]
    fn an_undrained_target_stream_does_not_starve_an_indexed_lookup() -> Result<()> {
        let dir = tempdir()?;
        let cache = Arc::new(LocalCacheSQLite::with_pool_config(
            dir.path().join("cache.db"),
            0,
            HELD_PIPES,
            read_pool_headroom(HELD_PIPES),
            READ_POOL_TIMEOUT,
        )?);

        let addr = make_addr("pkg", "t");
        let _held = saturate_pipes(&cache, &addr)?;
        // Started and never pulled from: the producer keeps its connection.
        let _stream = cache.list_targets()?;

        let found = answers_promptly(
            "exists",
            enclose::enclose!((cache, addr) move || cache.exists(&addr, "h", "blob0")),
        )?;
        assert!(found, "the blob was written, so exists must report it");

        Ok(())
    }

    /// Headroom is a budget, not a guarantee — under simultaneous peak load from
    /// every unpermitted caller the pool can still run out. What must not happen
    /// then is a silent stall: the wait is bounded, and the error says the pool
    /// was exhausted rather than blaming the query that happened to be last.
    #[test]
    fn an_exhausted_pool_fails_with_a_bounded_diagnosable_error() -> Result<()> {
        let dir = tempdir()?;
        // One pipe, no headroom: a single undrained reader owns the whole pool.
        let cache = LocalCacheSQLite::with_pool_config(
            dir.path().join("cache.db"),
            0,
            1,
            0,
            Duration::from_millis(200),
        )?;

        let addr = make_addr("pkg", "t");
        let mut w = cache.writer(&addr, "h", "blob")?;
        w.write_all(&vec![7u8; 4 * 1024 * 1024])?;
        drop(w);
        let _held = cache.reader(&addr, "h", "blob")?;

        let err = cache
            .exists(&addr, "h", "blob")
            .expect_err("an exhausted pool must fail, not hang");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("pool of 1 exhausted"),
            "the error must name the pool, got: {msg}"
        );

        Ok(())
    }

    #[test]
    fn test_local_cache_sqlite() -> Result<()> {
        let dir = tempdir()?;
        let cache = LocalCacheSQLite::with_pipe_limit(
            dir.path().join("cache.db"),
            16 * 1024,
            DEFAULT_MAX_CONCURRENT_PIPES,
        )?;

        let addr = make_addr("test_pkg", "test_target");
        let hashin = "abc123hash";
        let name = "output.txt";

        assert!(!cache.exists(&addr, hashin, name)?);

        let mut writer = cache.writer(&addr, hashin, name)?;
        writer.write_all(b"hello sqlite cache")?;
        drop(writer);

        assert!(cache.exists(&addr, hashin, name)?);

        let sized = cache.reader(&addr, hashin, name)?;
        assert_eq!(sized.size, b"hello sqlite cache".len() as u64);
        let mut reader = sized.reader;
        let mut content = String::new();
        reader.read_to_string(&mut content)?;
        assert_eq!(content, "hello sqlite cache");

        cache.delete(&addr, hashin, name)?;
        assert!(!cache.exists(&addr, hashin, name)?);

        Ok(())
    }

    /// A key becomes pending when its finished spool is *queued*, not when a
    /// writer is opened for it. Registering at open makes every reader of the key
    /// wait out the caller's whole streaming write — a remote blob's download and
    /// gunzip, or a target's tar and compress — which nothing bounds; only the
    /// queue→commit gap is the writer thread's to close.
    ///
    /// Probed on a helper thread so a regression fails on the timeout instead of
    /// hanging the suite on an untimed condvar.
    #[test]
    fn an_open_writer_does_not_make_the_key_pending() -> Result<()> {
        let dir = tempdir()?;
        let cache = Arc::new(LocalCacheSQLite::with_pipe_limit(
            dir.path().join("cache.db"),
            16 * 1024,
            DEFAULT_MAX_CONCURRENT_PIPES,
        )?);
        let addr = make_addr("pkg", "streaming");

        // Open and half-write, holding the writer the way a slow download does.
        let mut writer = cache.writer(&addr, "h", "blob")?;
        writer.write_all(b"first half")?;

        let (tx, rx) = mpsc::channel();
        std::thread::spawn({
            let (cache, addr) = (cache.clone(), addr.clone());
            move || {
                drop(tx.send(cache.exists(&addr, "h", "blob")));
            }
        });

        let found = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("exists must not wait on a writer that is still being written to")?;
        assert!(
            !found,
            "an unfinished write has no committed bytes, so the key is absent"
        );

        // And once queued, the wait is back on: the committed value is visible.
        drop(writer);
        assert!(cache.exists(&addr, "h", "blob")?);
        Ok(())
    }

    /// The tracker's contract is that the slot a reader finds for a key is the
    /// *last* one to complete for it, which holds only while map order matches
    /// channel order. `register_and_send` keeps them in step by holding the map
    /// lock across the send; registering and sending separately lets two writers
    /// of one key interleave and leaves the map pointing at the slot that
    /// completes first, so a reader wakes and reads bytes the other write is about
    /// to overwrite.
    #[test]
    fn the_mapped_slot_belongs_to_the_last_queued_write() {
        let tracker = PendingTracker::default();
        // Receiver held here, so nothing drains and both commands stay queued.
        let (tx, rx) = mpsc::channel();
        let key: Key = ("//pkg:t".to_string(), "h".to_string(), "blob".to_string());

        let first = tracker
            .register_and_send(&tx, key.clone(), |key, slot| {
                WriterCmd::Write(WriteJob {
                    key,
                    buf: SpooledTempFile::new(SPOOL_MEM_THRESHOLD),
                    size: 1,
                    slot,
                })
            })
            .expect("first send");
        let second = tracker
            .register_and_send(&tx, key.clone(), |key, slot| {
                WriterCmd::Write(WriteJob {
                    key,
                    buf: SpooledTempFile::new(SPOOL_MEM_THRESHOLD),
                    size: 2,
                    slot,
                })
            })
            .expect("second send");

        let queued: Vec<WriterCmd> = rx.try_iter().collect();
        assert_eq!(queued.len(), 2);
        let slot_of = |cmd: &WriterCmd| match cmd {
            WriterCmd::Write(j) => j.slot.clone(),
            WriterCmd::Delete(j) => j.slot.clone(),
        };
        assert!(
            Arc::ptr_eq(&slot_of(&queued[0]), &first) && Arc::ptr_eq(&slot_of(&queued[1]), &second),
            "the channel must carry the slots in registration order"
        );

        let mapped = {
            let m = tracker.map.lock().expect("tracker");
            PendingTracker::find(&m, &key.0, &key.1, &key.2).expect("key must be pending")
        };
        assert!(
            Arc::ptr_eq(&mapped, &second),
            "the map must point at the slot the writer thread completes last"
        );
    }

    /// A failed send must leave nothing registered: the command was never queued,
    /// so no slot for it will ever be completed and a reader that found one would
    /// wait forever.
    #[test]
    fn a_rejected_send_registers_nothing() {
        let tracker = PendingTracker::default();
        let (tx, rx) = mpsc::channel::<WriterCmd>();
        drop(rx);
        let key: Key = ("//pkg:t".to_string(), "h".to_string(), "blob".to_string());

        let sent = tracker.register_and_send(&tx, key.clone(), |key, slot| {
            WriterCmd::Delete(DeleteJob { key, slot })
        });

        assert!(sent.is_none(), "a closed channel must report the failure");
        let m = tracker.map.lock().expect("tracker");
        assert!(
            PendingTracker::find(&m, &key.0, &key.1, &key.2).is_none(),
            "a write that was never queued must not leave the key pending"
        );
    }

    /// The point of the async waiter: a task waiting on a queued write must leave
    /// its worker free for everything else.
    ///
    /// The waiter is `tokio::spawn`ed rather than awaited inline, and that is the
    /// whole test. `#[tokio::test(flavor = "multi_thread")]` drives the test body
    /// on the *calling* thread via `block_on`, so an inline wait leaves the worker
    /// free and the test passes even against a thread-parking implementation —
    /// verified by swapping in `PendingSlot::wait`. Spawned, the waiter competes
    /// for the single worker: a parking implementation occupies it, the ticker
    /// never advances, the completer never fires, and the timeout fails the test.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn awaiting_a_slot_leaves_the_worker_polling() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let slot = PendingSlot::new();
        let ticks = Arc::new(AtomicUsize::new(0));

        let waiter = tokio::spawn({
            let slot = slot.clone();
            async move { slot.wait_async().await }
        });
        let ticker = tokio::spawn({
            let ticks = ticks.clone();
            async move {
                for _ in 0..10 {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    ticks.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
        // Stands in for the sqlite writer thread: completes the slot off-runtime,
        // and only once the runtime has demonstrably kept running. The spin is
        // bounded so a failing run ends instead of burning a core for the rest of
        // the test binary's life.
        let completer = std::thread::spawn({
            let (slot, ticks) = (slot.clone(), ticks.clone());
            move || {
                for _ in 0..2_000 {
                    if ticks.load(Ordering::SeqCst) >= 3 {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                slot.complete();
            }
        });

        tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .expect("a spawned waiter must not park the runtime's only worker")
            .expect("waiter task");

        assert!(
            ticks.load(Ordering::SeqCst) >= 3,
            "the runtime must have kept polling while the slot was pending"
        );
        ticker.await.expect("ticker");
        completer.join().expect("completer");
    }

    /// A slot completed before the first poll must resolve at once rather than
    /// stranding a late waiter on a wake-up that already happened.
    #[tokio::test]
    async fn a_slot_completed_before_the_first_poll_resolves_at_once() {
        let slot = PendingSlot::new();
        slot.complete();
        tokio::time::timeout(std::time::Duration::from_secs(5), slot.wait_async())
            .await
            .expect("a slot completed before the first poll must resolve at once");
    }

    /// `complete` drains a `Vec` of wakers, so every task parked on one slot has to
    /// be woken — a single-waiter test would pass against a `notify_one`-shaped
    /// implementation that strands the rest.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_task_awaiting_a_slot_is_woken() {
        let slot = PendingSlot::new();
        let waiters: Vec<_> = (0..4)
            .map(|_| {
                let slot = slot.clone();
                tokio::spawn(async move { slot.wait_async().await })
            })
            .collect();

        // Give them a chance to register before the completion lands.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        slot.complete();

        for w in waiters {
            tokio::time::timeout(std::time::Duration::from_secs(5), w)
                .await
                .expect("every waiter on a completed slot must be woken")
                .expect("waiter task");
        }
    }

    fn queued_write(cache: &LocalCacheSQLite, addr: &Addr, name: &str, bytes: &[u8]) {
        let mut w = cache.writer(addr, "h", name).expect("writer");
        w.write_all(bytes).expect("write");
        drop(w);
    }

    fn committed(e: Existence) -> bool {
        match e {
            Existence::Committed(found) => found,
            Existence::Queued(_) => panic!("expected a committed answer, got a queued one"),
        }
    }

    fn queued(e: Existence) -> PendingWrite {
        match e {
            Existence::Queued(p) => p,
            Existence::Committed(found) => {
                panic!("expected a queued answer, got Committed({found})")
            }
        }
    }

    /// `existence` is the read path's probe now, and it must clear a saturated pool
    /// for the same reason `exists` must: it is one indexed lookup, and the whole
    /// point of it not waiting on the write queue is lost if it instead waits
    /// [`READ_POOL_TIMEOUT`] for a connection.
    ///
    /// Sibling of `undrained_streaming_reads_do_not_starve_an_indexed_lookup`, over
    /// the method the engine actually calls.
    #[test]
    fn undrained_streaming_reads_do_not_starve_existence() -> Result<()> {
        let dir = tempdir()?;
        let cache = Arc::new(LocalCacheSQLite::with_pool_config(
            dir.path().join("cache.db"),
            0,
            HELD_PIPES,
            read_pool_headroom(HELD_PIPES),
            READ_POOL_TIMEOUT,
        )?);

        let addr = make_addr("pkg", "t");
        let _held = saturate_pipes(&cache, &addr)?;

        let found = answers_promptly(
            "existence",
            enclose::enclose!((cache, addr) move || cache
                .existence(&addr, "h", "blob0")
                .map(committed)),
        )?;
        assert!(found, "the blob was written, so existence must report it");

        Ok(())
    }

    /// The settled states: `existence` must agree with `exists` whenever nothing is
    /// in flight, or every caller of `exists_local` gets a different answer to the
    /// probe it replaced.
    #[test]
    fn existence_matches_exists_when_nothing_is_queued() -> Result<()> {
        let dir = tempdir()?;
        let cache = LocalCacheSQLite::with_pipe_limit(
            dir.path().join("cache.db"),
            16 * 1024,
            DEFAULT_MAX_CONCURRENT_PIPES,
        )?;
        let addr = make_addr("pkg", "settled");

        assert!(!committed(cache.existence(&addr, "h", "blob")?));
        queued_write(&cache, &addr, "blob", b"bytes");
        assert!(cache.exists(&addr, "h", "blob")?); // barrier: commit has landed
        assert!(committed(cache.existence(&addr, "h", "blob")?));
        Ok(())
    }

    /// A queued write has no settled answer, so `existence` reports the queue
    /// rather than guessing — and once the gate opens, the awaited answer is the
    /// write that landed.
    #[tokio::test]
    async fn existence_of_a_queued_write_is_queued_then_present() -> Result<()> {
        let dir = tempdir()?;
        let cache = LocalCacheSQLite::with_pipe_limit(
            dir.path().join("cache.db"),
            16 * 1024,
            DEFAULT_MAX_CONCURRENT_PIPES,
        )?;
        let addr = make_addr("pkg", "queued");

        cache.gate.close();
        queued_write(&cache, &addr, "blob", b"bytes");

        let pending = queued(cache.existence(&addr, "h", "blob")?);
        cache.gate.open();
        tokio::time::timeout(std::time::Duration::from_secs(5), pending)
            .await
            .expect("the queued write must land");

        assert!(
            committed(cache.existence(&addr, "h", "blob")?),
            "after the queue drains the key is present"
        );
        Ok(())
    }

    /// The case a "queued means present" shortcut would get wrong: `delete`
    /// registers a slot too, so a key mid-GC reports `Queued` and then resolves to
    /// *absent*.
    #[tokio::test]
    async fn existence_of_a_queued_delete_is_queued_then_absent() -> Result<()> {
        let dir = tempdir()?;
        let cache = Arc::new(LocalCacheSQLite::with_pipe_limit(
            dir.path().join("cache.db"),
            16 * 1024,
            DEFAULT_MAX_CONCURRENT_PIPES,
        )?);
        let addr = make_addr("pkg", "doomed");

        queued_write(&cache, &addr, "blob", b"bytes");
        assert!(cache.exists(&addr, "h", "blob")?); // barrier

        cache.gate.close();
        // `delete` parks until its commit lands, so it has to run off this thread
        // while the gate is shut.
        let deleter = std::thread::spawn({
            let (cache, addr) = (cache.clone(), addr.clone());
            move || cache.delete(&addr, "h", "blob")
        });
        // Let the delete reach the queue before probing.
        while cache
            .pending
            .pending(&LocalCacheSQLite::key(&addr), "h", "blob")
            .is_none()
        {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }

        let pending = queued(cache.existence(&addr, "h", "blob")?);
        cache.gate.open();
        tokio::time::timeout(std::time::Duration::from_secs(5), pending)
            .await
            .expect("the queued delete must land");
        deleter.join().expect("deleter thread").expect("delete");

        assert!(
            !committed(cache.existence(&addr, "h", "blob")?),
            "a queued delete resolves to absent, not present"
        );
        Ok(())
    }

    /// The property the tracker exists for, end to end. Two writers on one key are
    /// opened in one order and queued in the *reverse* order; the reader must
    /// observe the last-enqueued bytes.
    ///
    /// Registering at writer-open would map the key to the slot of the
    /// last-*opened* writer — `second` here — which the writer thread completes
    /// first, so the reader would wake early and read "second" just before "first"
    /// overwrote it.
    #[test]
    fn two_queued_writes_to_one_key_read_back_the_last_enqueued() -> Result<()> {
        let dir = tempdir()?;
        let cache = LocalCacheSQLite::with_pipe_limit(
            dir.path().join("cache.db"),
            16 * 1024,
            DEFAULT_MAX_CONCURRENT_PIPES,
        )?;
        let addr = make_addr("pkg", "superseded");

        cache.gate.close();
        let mut first = cache.writer(&addr, "h", "blob")?;
        first.write_all(b"first")?;
        let mut second = cache.writer(&addr, "h", "blob")?;
        second.write_all(b"second")?;

        // Reverse of the open order, so last-opened != last-enqueued.
        drop(second);
        drop(first);
        cache.gate.open();

        let mut got = String::new();
        cache
            .reader(&addr, "h", "blob")?
            .reader
            .read_to_string(&mut got)?;
        assert_eq!(
            got, "first",
            "the read must observe the last write enqueued"
        );
        Ok(())
    }

    /// A leaked map entry is invisible and fatal: `existence` would report `Queued`
    /// forever and `exists_local` would wait out its retries on every probe of that
    /// key for the rest of the process.
    #[test]
    fn the_pending_map_drains_once_every_write_commits() -> Result<()> {
        let dir = tempdir()?;
        let cache = LocalCacheSQLite::with_pipe_limit(
            dir.path().join("cache.db"),
            16 * 1024,
            DEFAULT_MAX_CONCURRENT_PIPES,
        )?;
        let addr = make_addr("pkg", "drains");

        cache.gate.close();
        for i in 0..32 {
            queued_write(&cache, &addr, &format!("blob{i}"), b"bytes");
        }
        // Same key twice, so a superseded entry is in the mix.
        queued_write(&cache, &addr, "blob0", b"again");
        cache.gate.open();

        // Barrier on the last key written; FIFO means everything before it landed.
        assert!(cache.exists(&addr, "h", "blob0")?);
        assert!(
            cache.pending.map.lock().expect("tracker").is_empty(),
            "every committed write must remove its registration"
        );
        Ok(())
    }

    /// A waiter that goes away mid-wait leaves its `Waker` in the slot until
    /// `complete` drains it. Waking a dropped task must be a harmless no-op, and it
    /// must not consume the completion that the surviving waiter needs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_one_awaiter_does_not_strand_the_others() {
        let slot = PendingSlot::new();
        let doomed = tokio::spawn({
            let slot = slot.clone();
            async move { slot.wait_async().await }
        });
        let survivor = tokio::spawn({
            let slot = slot.clone();
            async move { slot.wait_async().await }
        });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        doomed.abort();
        slot.complete();

        tokio::time::timeout(std::time::Duration::from_secs(5), survivor)
            .await
            .expect("an aborted sibling must not strand the surviving waiter")
            .expect("survivor task");
    }

    #[test]
    fn test_seekable_reader_pread_in_middle() -> Result<()> {
        use io::{Read, Seek, SeekFrom};
        let dir = tempdir()?;
        let cache = LocalCacheSQLite::with_pipe_limit(
            dir.path().join("cache.db"),
            16 * 1024,
            DEFAULT_MAX_CONCURRENT_PIPES,
        )?;

        let addr = make_addr("pkg", "t");
        let payload: Vec<u8> = (0..1024u16).map(|i| (i & 0xff) as u8).collect();
        let mut w = cache.writer(&addr, "h", "blob")?;
        w.write_all(&payload)?;
        drop(w);

        let mut r = cache
            .seekable_reader(&addr, "h", "blob")?
            .expect("sqlite must support seekable_reader");
        r.seek(SeekFrom::Start(100))?;
        let mut buf = vec![0u8; 50];
        r.read_exact(&mut buf)?;
        assert_eq!(buf, payload[100..150]);

        r.seek(SeekFrom::Start(0))?;
        let mut head = vec![0u8; 4];
        r.read_exact(&mut head)?;
        assert_eq!(head, payload[..4]);
        Ok(())
    }

    #[test]
    fn test_seekable_reader_missing_returns_not_found() {
        let dir = tempdir().expect("tempdir");
        let cache = LocalCacheSQLite::with_pipe_limit(
            dir.path().join("cache.db"),
            16 * 1024,
            DEFAULT_MAX_CONCURRENT_PIPES,
        )
        .expect("cache");
        let addr = make_addr("pkg", "t");
        let err = match cache.seekable_reader(&addr, "missing", "blob") {
            Ok(_) => panic!("must error"),
            Err(e) => e,
        };
        assert!(err.is::<NotFoundError>(), "{err:#}");
    }

    #[test]
    fn test_local_cache_sqlite_concurrent_readers() -> Result<()> {
        use std::sync::Arc;

        let dir = tempdir()?;
        let cache = Arc::new(LocalCacheSQLite::with_pipe_limit(
            dir.path().join("cache.db"),
            16 * 1024,
            DEFAULT_MAX_CONCURRENT_PIPES,
        )?);

        let addr = make_addr("test_pkg", "concurrent");
        let hashin = "hashcon";
        let name = "data.bin";

        let mut writer = cache.writer(&addr, hashin, name)?;
        writer.write_all(b"concurrent read data")?;
        drop(writer);

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let c = cache.clone();
                let a = addr.clone();
                std::thread::spawn(move || {
                    let mut reader = c.reader(&a, hashin, name).expect("reader").reader;
                    let mut buf = String::new();
                    reader.read_to_string(&mut buf).expect("read");
                    assert_eq!(buf, "concurrent read data")
                })
            })
            .collect();

        for h in handles {
            h.join().expect("thread panicked");
        }

        Ok(())
    }

    #[test]
    fn test_list_targets_and_entries() -> Result<()> {
        let dir = tempdir()?;
        let cache = LocalCacheSQLite::with_pipe_limit(
            dir.path().join("cache.db"),
            16 * 1024,
            DEFAULT_MAX_CONCURRENT_PIPES,
        )?;
        let a = make_addr("pkg", "t");
        let b = make_addr("pkg", "u");
        for (addr, h) in [(&a, "h1"), (&a, "h2"), (&b, "h9")] {
            let mut w = cache.writer(addr, h, "out.tar")?;
            w.write_all(b"x")?;
            drop(w);
            assert!(cache.exists(addr, h, "out.tar")?); // barrier
        }

        let mut targets = cache.list_targets()?.collect::<Result<Vec<_>>>()?;
        targets.sort();
        assert_eq!(targets, vec![a.format(), b.format()]);

        let mut a_entries = cache.list_target_entries(&a)?;
        a_entries.sort();
        assert_eq!(a_entries, vec!["h1".to_string(), "h2".to_string()]);
        assert_eq!(cache.list_target_entries(&b)?, vec!["h9".to_string()]);
        Ok(())
    }

    #[test]
    fn test_local_cache_sqlite_read_after_pending_write() -> Result<()> {
        use std::sync::Arc;

        // Reader started before the writer Drop returns from enqueue must still observe
        // the write once it lands. This exercises the PendingTracker wait path.
        let dir = tempdir()?;
        let cache = Arc::new(LocalCacheSQLite::with_pipe_limit(
            dir.path().join("cache.db"),
            16 * 1024,
            DEFAULT_MAX_CONCURRENT_PIPES,
        )?);
        let addr = make_addr("pkg", "tgt");
        let hashin = "h1";
        let name = "out.bin";

        for i in 0..16 {
            let mut writer = cache.writer(&addr, hashin, name)?;
            writer.write_all(format!("iter-{i}").as_bytes())?;
            drop(writer);

            // Right after drop, the write is enqueued but may not be persisted yet.
            // exists() must wait until the bg thread completes the slot.
            assert!(cache.exists(&addr, hashin, name)?);

            let mut reader = cache.reader(&addr, hashin, name)?.reader;
            let mut got = String::new();
            reader.read_to_string(&mut got)?;
            assert_eq!(got, format!("iter-{i}"));
        }

        Ok(())
    }
}
