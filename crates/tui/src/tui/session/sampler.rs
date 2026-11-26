//! Machine stats: one thread samples the machine once a second, so a slow
//! build can be read against what the machine was doing — out of CPU, short of
//! memory, or neither.
//!
//! - **Ticks (D22).** Fixed 1 s ticks: the next tick is the previous one plus
//!   [`TICK`]; a missed tick is skipped, never caught up, so a sample covers
//!   the whole interval since the one before it. The first busy figure lands
//!   one tick after start (start only reads the baseline).
//! - **No allocation per sample (I10).** Each source reads into one buffer
//!   allocated at start and parses it in place, with no indexing or slicing
//!   that can panic. A [`Reading`] is `Copy` and goes by value into the
//!   sampler's own preallocated [`channel`] with `try_send`: a full channel
//!   drops the sample and counts it, and the next sample delivered carries
//!   the count ([`Sample::dropped`]) and every failure not yet delivered.
//! - **Stop.** The thread stops on a stop message, on `Disconnected` (its
//!   owner dropped without stopping it, or the readings' receiver gone), and
//!   once its `latched` check says the recorder stopped. [`Sampler::stop`]
//!   waits on a done channel for at most [`STOP_WAIT`] and then detaches the
//!   thread, never a bare join, so a source that blocks cannot hang the exit.
//!   At a stop, a drop count or failure not yet delivered goes out on one
//!   last sample if the channel has room.
//! - **Sources.** Each is independent: one that fails is disabled for the rest
//!   of the run and named once, in the next delivered sample. Nothing is
//!   retried. Linux reads `/proc` and, under a cgroup v2 CPU quota, the quota
//!   cgroup's `cpu.stat` ([`procfs`]); macOS asks Mach ([`mach`]). Both read
//!   `getrusage` and `sysconf`. The parsers and the busy and used-memory
//!   formulas are pure functions compiled on every host under `cfg(test)`, so
//!   their fixtures run everywhere.
//!
//! Nothing here reaches the engine (I10): the sampler holds no engine handle
//! and its figures go only into the recording.

use std::fmt;
use std::io;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use super::format::{MachineInfoRecord, MachineSampleRecord, MachineSourceFailure};

#[cfg(any(target_os = "macos", test))]
mod mach;
#[cfg(any(target_os = "linux", test))]
mod procfs;
#[cfg(test)]
pub(super) mod tests;

#[cfg(target_os = "macos")]
use mach::host_sources;
#[cfg(target_os = "linux")]
use procfs::host_sources;

/// The sampling interval. Not a setting (D22).
pub const TICK: Duration = Duration::from_secs(1);

/// The readings channel's capacity: what the recorder may fall behind by
/// before a sample is dropped.
pub const CHANNEL_CAP: usize = 4;

/// How long [`Sampler::stop`] waits for the thread before detaching it.
pub const STOP_WAIT: Duration = Duration::from_millis(100);

/// A per-tick source, each disabled on its own when it fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The machine's CPU busy fraction (`heph.machine.cpu.busy`).
    CpuBusy,
    /// The machine's used memory (`heph.machine.memory.used`).
    MemoryUsed,
    /// heph's own CPU time (`heph.process.cpu.time`).
    ProcessCpu,
    /// heph's own resident set (`heph.process.memory.resident`).
    ProcessResident,
}

impl Source {
    /// Every source, in [`Failures`] order.
    pub const ALL: [Self; 4] = [
        Self::CpuBusy,
        Self::MemoryUsed,
        Self::ProcessCpu,
        Self::ProcessResident,
    ];

    /// The metric this source feeds.
    pub const fn metric(self) -> &'static str {
        use super::derive::machine;
        match self {
            Self::CpuBusy => machine::CPU_BUSY,
            Self::MemoryUsed => machine::MEMORY_USED,
            Self::ProcessCpu => machine::PROCESS_CPU_TIME,
            Self::ProcessResident => machine::PROCESS_RESIDENT,
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// Why a source failed, without allocating: a static description of what
/// was being done and a code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceError {
    /// What the source was doing, e.g. `reading /proc/meminfo`.
    pub what: &'static str,
    pub code: ErrorCode,
}

/// The code half of a [`SourceError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// An OS error number.
    Os(i32),
    /// An IO error with no OS error number.
    Io(io::ErrorKind),
    /// A Mach call's `kern_return_t`.
    Kern(i32),
    /// A Mach call or `sysctl` returned this many units: fewer than the
    /// fields read, or more than the buffer it was handed.
    Count(u32),
    /// The source's text was not in the expected format.
    Malformed,
    /// The source lacks a field the figure needs.
    Missing,
}

impl SourceError {
    pub(crate) const fn new(what: &'static str, code: ErrorCode) -> Self {
        Self { what, code }
    }

    pub(crate) fn io(what: &'static str, e: &io::Error) -> Self {
        let code = match e.raw_os_error() {
            Some(n) => ErrorCode::Os(n),
            None => ErrorCode::Io(e.kind()),
        };
        Self { what, code }
    }
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = self.what;
        match self.code {
            ErrorCode::Os(n) => write!(f, "{what}: {}", io::Error::from_raw_os_error(n)),
            ErrorCode::Io(kind) => write!(f, "{what}: {kind}"),
            ErrorCode::Kern(kr) => write!(f, "{what}: kern_return_t {kr}"),
            ErrorCode::Count(n) => write!(f, "{what}: returned {n} units, not the struct's size"),
            ErrorCode::Malformed => write!(f, "{what}: unexpected format"),
            ErrorCode::Missing => write!(f, "{what}: field missing"),
        }
    }
}

impl std::error::Error for SourceError {}

/// Where `heph.machine.cpu.limit` came from (D23).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuLimitFrom {
    /// A cgroup v2 `cpu.max` quota tighter than the CPU count (Linux only).
    Quota,
    /// The logical CPUs online.
    Host,
}

/// The CPUs this process may use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuLimit {
    pub cpus: f64,
    pub from: CpuLimitFrom,
}

/// Read once, at start: the `MachineInfo` record.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MachineInfo {
    /// `heph.machine.cpu.count`: logical CPUs online.
    pub cpu_count: Result<u32, SourceError>,
    /// `heph.machine.cpu.limit`.
    pub cpu_limit: Result<CpuLimit, SourceError>,
    /// `heph.machine.memory.limit`: physical RAM (O11).
    pub memory_limit: Result<u64, SourceError>,
}

/// The sources that failed since the last delivered sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Failures([Option<SourceError>; 4]);

impl Failures {
    /// Each failed source with why, in [`Source::ALL`] order.
    pub fn iter(&self) -> impl Iterator<Item = (Source, SourceError)> + '_ {
        Source::ALL
            .into_iter()
            .zip(self.0.iter())
            .filter_map(|(source, e)| e.map(|e| (source, e)))
    }

    pub fn is_empty(&self) -> bool {
        self.0.iter().all(Option::is_none)
    }

    /// Why `source` failed, if it is in here.
    pub fn get(&self, source: Source) -> Option<SourceError> {
        self.0.get(source.index()).copied().flatten()
    }

    fn set(&mut self, source: Source, e: SourceError) {
        if let Some(slot) = self.0.get_mut(source.index()) {
            *slot = Some(e);
        }
    }
}

/// One tick: the `MachineSample` record. A figure is `None` when its source
/// failed or is disabled, and `cpu_busy` also when the CPU counters did not
/// move (no NaN is ever written).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// When it was taken. The recorder stamps it from its anchor plus this
    /// instant's offset, as it stamps spans.
    pub at: Instant,
    /// The interval it covers: since the previous sample taken, or since
    /// start.
    pub interval: Duration,
    /// `heph.machine.cpu.busy`: busy fraction of the CPU limit, 0–1.
    pub cpu_busy: Option<f64>,
    /// `heph.machine.memory.used`, bytes.
    pub memory_used: Option<u64>,
    /// `heph.process.cpu.time`: heph's user + system time, cumulative.
    pub process_cpu: Option<Duration>,
    /// `heph.process.memory.resident`, bytes.
    pub process_resident: Option<u64>,
    /// Sources that failed since the last delivered sample, each named once.
    pub failed: Failures,
    /// Samples dropped on a full channel since the last delivered one.
    pub dropped: u32,
}

/// What the sampler hands the recorder: [`MachineInfo`] first, then one
/// [`Sample`] a tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reading {
    /// The start-time figures, read `at`: stamped as a sample is.
    Info {
        info: MachineInfo,
        at: Instant,
    },
    Sample(Sample),
}

/// The sampler's own preallocated channel.
pub fn channel() -> (SyncSender<Reading>, Receiver<Reading>) {
    mpsc::sync_channel(CHANNEL_CAP)
}

/// The `MachineInfo` record of `info`, read at `time_unix_us`. Runs on the
/// recorder's thread, not the sampler's: it may allocate.
pub(crate) fn info_record(info: &MachineInfo, time_unix_us: u64) -> MachineInfoRecord {
    use super::derive::machine;
    let mut failed = Vec::new();
    let mut ok = |metric: &'static str, e: Option<&SourceError>| {
        if let Some(e) = e {
            failed.push(MachineSourceFailure {
                source: metric.to_owned(),
                error: e.to_string(),
            });
        }
    };
    ok(machine::CPU_COUNT, info.cpu_count.as_ref().err());
    ok(machine::CPU_LIMIT, info.cpu_limit.as_ref().err());
    ok(machine::MEMORY_LIMIT, info.memory_limit.as_ref().err());
    let cpu_limit = info.cpu_limit.as_ref().ok();
    MachineInfoRecord {
        time_unix_us,
        cpu_count: info.cpu_count.ok(),
        cpu_limit_ucpus: cpu_limit.map(|l| ucpus(l.cpus)),
        cpu_limit_source: cpu_limit.map(|l| {
            match l.from {
                CpuLimitFrom::Quota => "quota",
                CpuLimitFrom::Host => "host",
            }
            .to_owned()
        }),
        memory_limit_bytes: info.memory_limit.ok(),
        memory_limit_source: info.memory_limit.is_ok().then(|| "host".to_owned()),
        failed,
    }
}

/// A CPU amount in micro-CPUs, rounded; a negative or non-finite one is 0.
fn ucpus(cpus: f64) -> u64 {
    let v = (cpus * hcore::capacity::MICROS_PER_CPU as f64).round();
    if v.is_finite() && v > 0.0 {
        #[expect(clippy::cast_sign_loss, reason = "positive, and `as` saturates")]
        let n = v as u64;
        n
    } else {
        0
    }
}

/// The `MachineSample` record of `sample`, taken at `time_unix_us`. Runs on
/// the recorder's thread.
pub(crate) fn sample_record(sample: &Sample, time_unix_us: u64) -> MachineSampleRecord {
    MachineSampleRecord {
        time_unix_us,
        interval_us: u64::try_from(sample.interval.as_micros()).unwrap_or(u64::MAX),
        cpu_busy_ppm: sample.cpu_busy.map(|b| {
            let v = (b.clamp(0.0, 1.0) * f64::from(super::derive::PPM)).round();
            #[expect(clippy::cast_sign_loss, reason = "clamped to 0..=10^6")]
            let ppm = v as u32;
            ppm
        }),
        memory_used_bytes: sample.memory_used,
        process_cpu_us: sample
            .process_cpu
            .map(|d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX)),
        process_resident_bytes: sample.process_resident,
        failed: sample
            .failed
            .iter()
            .map(|(source, e)| MachineSourceFailure {
                source: source.metric().to_owned(),
                error: e.to_string(),
            })
            .collect(),
        dropped: sample.dropped,
    }
}

/// `heph.machine.cpu.limit`: the quota when it is tighter than the count,
/// else the count.
pub(crate) fn cpu_limit(
    count: Result<u32, SourceError>,
    quota: Option<f64>,
) -> Result<CpuLimit, SourceError> {
    match (count, quota) {
        (Ok(n), Some(q)) if q < f64::from(n) => Ok(CpuLimit {
            cpus: q,
            from: CpuLimitFrom::Quota,
        }),
        (Ok(n), _) => Ok(CpuLimit {
            cpus: f64::from(n),
            from: CpuLimitFrom::Host,
        }),
        (Err(_), Some(q)) => Ok(CpuLimit {
            cpus: q,
            from: CpuLimitFrom::Quota,
        }),
        (Err(e), None) => Err(e),
    }
}

pub(crate) fn sysconf(name: libc::c_int, what: &'static str) -> Result<u64, SourceError> {
    // SAFETY: sysconf reads a system limit: it takes no pointer and has no
    // precondition.
    let v = unsafe { libc::sysconf(name) };
    if v < 0 {
        return Err(SourceError::io(what, &io::Error::last_os_error()));
    }
    u64::try_from(v)
        .ok()
        .filter(|v| *v > 0)
        .ok_or(SourceError::new(what, ErrorCode::Malformed))
}

/// Logical CPUs online.
pub(crate) fn cpu_count() -> Result<u32, SourceError> {
    const WHAT: &str = "sysconf(_SC_NPROCESSORS_ONLN)";
    let n = sysconf(libc::_SC_NPROCESSORS_ONLN, WHAT)?;
    u32::try_from(n).map_err(|_too_many| SourceError::new(WHAT, ErrorCode::Malformed))
}

/// heph's own user + system time.
pub(crate) fn process_cpu() -> Result<Duration, SourceError> {
    const WHAT: &str = "getrusage(RUSAGE_SELF)";
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: `usage` points to writable storage for one `rusage`, which is
    // what getrusage fills; RUSAGE_SELF is a valid `who`.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if rc != 0 {
        return Err(SourceError::io(WHAT, &io::Error::last_os_error()));
    }
    // SAFETY: getrusage returned 0, so it initialised `usage`.
    let usage = unsafe { usage.assume_init() };
    Ok(timeval(usage.ru_utime).saturating_add(timeval(usage.ru_stime)))
}

fn timeval(t: libc::timeval) -> Duration {
    let secs = u64::try_from(t.tv_sec).unwrap_or(0);
    let micros = u64::try_from(t.tv_usec).unwrap_or(0);
    Duration::from_secs(secs).saturating_add(Duration::from_micros(micros))
}

/// Where a sample's figures come from: the host's, or a test's.
pub(crate) trait Sources {
    /// One read of the CPU counters.
    type Cpu: Copy;
    /// The start-time figures. Called once, before any other method.
    fn info(&mut self) -> MachineInfo;
    fn cpu(&mut self) -> Result<Self::Cpu, SourceError>;
    /// The busy fraction between two CPU readings `interval` apart; `None`
    /// when nothing moved.
    fn busy(&self, prev: &Self::Cpu, cur: &Self::Cpu, interval: Duration) -> Option<f64>;
    fn memory_used(&mut self) -> Result<u64, SourceError>;
    fn process_cpu(&mut self) -> Result<Duration, SourceError>;
    fn process_resident(&mut self) -> Result<u64, SourceError>;
}

/// The sampler's state between ticks: the CPU baseline, which sources are
/// still enabled, and what has not been delivered yet.
pub(crate) struct Core<S: Sources> {
    sources: S,
    enabled: [bool; 4],
    prev_cpu: Option<S::Cpu>,
    prev_at: Instant,
    pending: Failures,
    dropped: u32,
}

impl<S: Sources> Core<S> {
    /// Read the start-time figures and the CPU baseline.
    pub(crate) fn start(mut sources: S, now: Instant) -> (Self, MachineInfo) {
        let info = sources.info();
        let mut core = Self {
            sources,
            enabled: [true; 4],
            prev_cpu: None,
            prev_at: now,
            pending: Failures::default(),
            dropped: 0,
        };
        core.prev_cpu = core.read(Source::CpuBusy, S::cpu);
        (core, info)
    }

    /// Read `source` if it is enabled; disable it on its first failure.
    fn read<T>(
        &mut self,
        source: Source,
        f: impl FnOnce(&mut S) -> Result<T, SourceError>,
    ) -> Option<T> {
        let enabled = self.enabled.get_mut(source.index())?;
        if !*enabled {
            return None;
        }
        match f(&mut self.sources) {
            Ok(v) => Some(v),
            Err(e) => {
                *enabled = false;
                self.pending.set(source, e);
                None
            }
        }
    }

    /// One tick's sample, taken at `now`.
    pub(crate) fn sample(&mut self, now: Instant) -> Sample {
        let interval = now.saturating_duration_since(self.prev_at);
        self.prev_at = now;
        let cpu = self.read(Source::CpuBusy, S::cpu);
        let cpu_busy = match (&self.prev_cpu, &cpu) {
            (Some(prev), Some(cur)) => self.sources.busy(prev, cur, interval),
            _ => None,
        };
        if cpu.is_some() {
            self.prev_cpu = cpu;
        }
        Sample {
            at: now,
            interval,
            cpu_busy,
            memory_used: self.read(Source::MemoryUsed, S::memory_used),
            process_cpu: self.read(Source::ProcessCpu, S::process_cpu),
            process_resident: self.read(Source::ProcessResident, S::process_resident),
            failed: self.pending,
            dropped: self.dropped,
        }
    }

    /// The last sample reached the channel: its failures and drop count are
    /// delivered.
    pub(crate) fn delivered(&mut self) {
        self.pending = Failures::default();
        self.dropped = 0;
    }

    /// Whether a failure or a drop count waits for a sample to carry it.
    pub(crate) fn undelivered(&self) -> bool {
        self.dropped > 0 || !self.pending.is_empty()
    }

    /// The last sample was dropped on a full channel: what it carried rides
    /// on the next one.
    pub(crate) fn missed(&mut self) {
        self.dropped = self.dropped.saturating_add(1);
    }
}

/// The first tick on the grid `next + k × tick` after `now`: missed ticks are
/// skipped, never caught up.
pub(crate) fn next_tick(next: Instant, now: Instant, tick: Duration) -> Instant {
    let Some(behind) = now.checked_duration_since(next) else {
        return next;
    };
    let ticks = behind.as_nanos() / tick.as_nanos().max(1) + 1;
    let ticks = u32::try_from(ticks).unwrap_or(u32::MAX);
    next.checked_add(tick.saturating_mul(ticks))
        .or_else(|| now.checked_add(tick))
        .unwrap_or(now)
}

/// How a wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wake {
    /// The deadline passed: take a sample.
    Deadline,
    /// A stop message, or the owner dropped the stop sender.
    Stop,
}

/// The sampler's time: the real clock, or a test's.
pub(crate) trait Clock {
    fn now(&self) -> Instant;
    /// Wait until `until` or a stop on `stop`, whichever comes first.
    fn wait_until(&self, stop: &Receiver<()>, until: Instant) -> Wake;
}

#[derive(Debug)]
pub(crate) struct RealClock;

impl Clock for RealClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn wait_until(&self, stop: &Receiver<()>, until: Instant) -> Wake {
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Wake::Deadline;
            }
            match stop.recv_timeout(left) {
                Err(RecvTimeoutError::Timeout) => {}
                Ok(()) | Err(RecvTimeoutError::Disconnected) => return Wake::Stop,
            }
        }
    }
}

/// The sampler loop: `MachineInfo`, then a sample each tick until a stop,
/// `Disconnected` or `latched()`. Nothing in the loop allocates.
pub(crate) fn run<S: Sources>(
    sources: S,
    clock: &impl Clock,
    stop: &Receiver<()>,
    out: &SyncSender<Reading>,
    tick: Duration,
    mut latched: impl FnMut() -> bool,
) {
    if latched() {
        return;
    }
    let start = clock.now();
    let (mut core, info) = Core::start(sources, start);
    if let Err(TrySendError::Disconnected(_)) = out.try_send(Reading::Info { info, at: start }) {
        return;
    }
    let Some(mut next) = start.checked_add(tick) else {
        return;
    };
    loop {
        let wake = clock.wait_until(stop, next);
        if latched() {
            return;
        }
        if wake == Wake::Stop {
            // A failure or a drop count that rode on a dropped sample (or a
            // failed baseline read) goes out on one last sample, taken now,
            // if the channel has room: the recorder drains after the stop.
            if core.undelivered() {
                match out.try_send(Reading::Sample(core.sample(clock.now()))) {
                    Ok(()) | Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {}
                }
            }
            return;
        }
        let now = clock.now();
        next = next_tick(next, now, tick);
        match out.try_send(Reading::Sample(core.sample(now))) {
            Ok(()) => core.delivered(),
            Err(TrySendError::Full(_)) => core.missed(),
            Err(TrySendError::Disconnected(_)) => return,
        }
    }
}

/// What came of [`Sampler::stop`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplerExit {
    /// The thread exited within the wait.
    Exited,
    /// It did not (a source blocked): it is left to exit on its own.
    Detached,
}

/// The owner's handle on the sampler thread. Dropping it stops the thread
/// at its next wait (`Disconnected`), without waiting for it.
#[derive(Debug)]
pub struct Sampler {
    stop: SyncSender<()>,
    done: Receiver<()>,
}

impl Sampler {
    /// Start sampling the host. Readings arrive on the returned receiver,
    /// [`Reading::Info`] first. `latched` runs on the sampler thread at each
    /// wake: once it says true, the thread exits.
    pub fn spawn(
        latched: impl FnMut() -> bool + Send + 'static,
    ) -> io::Result<(Self, Receiver<Reading>)> {
        Self::spawn_with(host_sources, RealClock, TICK, latched)
    }

    /// [`Self::spawn`] with its sources, clock and tick injected. The sources
    /// are built on the sampler thread, so a slow start blocks only it.
    pub(crate) fn spawn_with<S: Sources + 'static, C: Clock + Send + 'static>(
        sources: impl FnOnce() -> S + Send + 'static,
        clock: C,
        tick: Duration,
        latched: impl FnMut() -> bool + Send + 'static,
    ) -> io::Result<(Self, Receiver<Reading>)> {
        let (out, readings) = channel();
        let (stop, stop_rx) = mpsc::sync_channel(1);
        let (done_tx, done) = mpsc::sync_channel::<()>(1);
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        std::thread::Builder::new()
            .name("heph-session-sampler".to_owned())
            .spawn(move || {
                let _dispatch = tracing::dispatcher::set_default(&dispatch);
                // Dropped as the thread exits, panicking or not: the owner's
                // done channel disconnects.
                let _done = done_tx;
                run(sources(), &clock, &stop_rx, &out, tick, latched);
            })?;
        #[cfg(any(test, feature = "test-support"))]
        SPAWNED.with(|n| n.set(n.get() + 1));
        Ok((Self { stop, done }, readings))
    }

    /// Stop the thread, waiting at most `wait` for it to exit, then detach
    /// it. Idempotent.
    pub fn stop(&self, wait: Duration) -> SamplerExit {
        match self.stop.try_send(()) {
            // Full: a stop is already pending. Disconnected: the thread is gone.
            Ok(()) | Err(TrySendError::Full(()) | TrySendError::Disconnected(())) => {}
        }
        match self.done.recv_timeout(wait) {
            Err(RecvTimeoutError::Timeout) => SamplerExit::Detached,
            Ok(()) | Err(RecvTimeoutError::Disconnected) => SamplerExit::Exited,
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static SPAWNED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many samplers were started from the calling thread: a recorder starts
/// its sampler on the thread that opens the session, so a test tells a run
/// that samples from one that does not. Per thread, so tests running in
/// parallel in one binary do not count each other's.
#[cfg(any(test, feature = "test-support"))]
pub fn spawned_on_this_thread() -> usize {
    SPAWNED.with(std::cell::Cell::get)
}

/// The sampler loop on the calling thread, with the host's sources, the
/// real clock and `tick` in place of [`TICK`], until `latched` says true: so
/// a test can count what the loop allocates on its own thread.
#[cfg(any(test, feature = "test-support"))]
pub fn run_host_loop(tick: Duration, out: &SyncSender<Reading>, latched: impl FnMut() -> bool) {
    let (_stop, stop_rx) = mpsc::sync_channel(1);
    run(host_sources(), &RealClock, &stop_rx, out, tick, latched);
}
