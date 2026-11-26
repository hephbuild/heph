//! The sampler: its parsers and formulas on fixtures (every host), the host's
//! own sources, and the thread under an injected clock.

use super::mach::{MachCpu, MachVm, mach_busy, mach_result, sum_cpu_load, used_mach};
use super::procfs::*;
use super::*;
use crate::tui::session::recorder;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

/// A `cpu` line's user, nice, system, idle, iowait, irq, softirq and steal.
fn stat(fields: [u64; 8]) -> StatCpu {
    let [user, nice, system, idle, iowait, irq, softirq, steal] = fields;
    StatCpu {
        user,
        nice,
        system,
        idle,
        iowait,
        irq,
        softirq,
        steal,
    }
}

/// A Linux root under a temp dir: each `(path, contents)` written beneath it.
fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for (path, contents) in files {
        let path = dir.path().join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, contents).expect("write");
    }
    dir
}

const MEMINFO: &str = "MemTotal:       16000000 kB\nMemFree:         1000000 kB\nMemAvailable:    4000000 kB\nBuffers:          100 kB\n";

fn proc_tree(stat: &str, meminfo: &str, statm: &str) -> tempfile::TempDir {
    tree(&[
        ("proc/stat", stat),
        ("proc/meminfo", meminfo),
        ("proc/self/statm", statm),
    ])
}

// ---- sampler_reads_every_source ----

#[test]
fn sampler_reads_every_source() {
    let mut sources = host_sources();
    let start = Instant::now();
    let (mut core, info) = Core::start(&mut sources, start);
    let count = info.cpu_count.expect("cpu count");
    assert!(count >= 1);
    let limit = info.cpu_limit.expect("cpu limit");
    assert!(
        limit.cpus > 0.0 && limit.cpus <= f64::from(count),
        "{limit:?} of {count}"
    );
    let ram = info.memory_limit.expect("memory limit");
    // Physical RAM by another road: sysconf's page count.
    // SAFETY: sysconf takes no pointer and has no precondition.
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    // SAFETY: as above.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let phys = u64::try_from(pages).unwrap() * u64::try_from(page).unwrap();
    assert!(
        ram.abs_diff(phys) <= phys / 64,
        "memory limit {ram} vs physical {phys}"
    );

    // Spin so the CPU counters and heph's own time both move.
    let spin = |d: Duration| {
        let until = Instant::now() + d;
        let mut x = 0u64;
        while Instant::now() < until {
            x = std::hint::black_box(x.wrapping_add(1));
        }
    };
    // A window in which the host's ticks did not move has no busy figure by
    // design. The spin moves them (they count at the scheduler's clock), so
    // one window should do; sample again, for at most ATTEMPTS windows, only
    // so a spin descheduled on a loaded runner does not fail the test. The
    // stale ticks this test once saw on macOS were host_statistics64's
    // machine-wide rate limit answering with a cached copy, which the CPU
    // source no longer calls (host_processor_info instead).
    const ATTEMPTS: u32 = 10;
    let mut attempt = 0;
    let (first, busy) = loop {
        attempt += 1;
        spin(Duration::from_millis(300));
        let sample = core.sample(Instant::now());
        assert!(sample.failed.is_empty(), "{:?}", sample.failed);
        if let Some(busy) = sample.cpu_busy {
            break (sample, busy);
        }
        assert!(
            attempt < ATTEMPTS,
            "no busy figure in {ATTEMPTS} windows of 300 ms: the host's CPU ticks never moved"
        );
    };
    assert!((0.0..=1.0).contains(&busy), "busy {busy}");
    let used = first.memory_used.expect("used");
    assert!(used > 0 && used < ram, "used {used} of {ram}");
    let resident = first.process_resident.expect("resident");
    assert!(resident > 0);
    let cpu1 = first.process_cpu.expect("process cpu");
    spin(Duration::from_millis(50));
    let second = core.sample(Instant::now());
    assert!(second.process_cpu.expect("process cpu") >= cpu1);
}

/// The host's sources through a `&mut`, so a test keeps them.
impl<S: Sources> Sources for &mut S {
    type Cpu = S::Cpu;
    fn info(&mut self) -> MachineInfo {
        (**self).info()
    }
    fn cpu(&mut self) -> Result<S::Cpu, SourceError> {
        (**self).cpu()
    }
    fn busy(&self, prev: &S::Cpu, cur: &S::Cpu, interval: Duration) -> Option<f64> {
        (**self).busy(prev, cur, interval)
    }
    fn memory_used(&mut self) -> Result<u64, SourceError> {
        (**self).memory_used()
    }
    fn process_cpu(&mut self) -> Result<Duration, SourceError> {
        (**self).process_cpu()
    }
    fn process_resident(&mut self) -> Result<u64, SourceError> {
        (**self).process_resident()
    }
}

// ---- sampler_parses_cpu_fixtures ----

#[test]
fn sampler_parses_cpu_fixtures() {
    // guest (9) and guest_nice (10) are inside user and nice: not added.
    let line = "cpu  100 20 30 400 50 6 7 8 9 10\ncpu0 1 2 3 4 5 6 7 8 9 10\n";
    assert_eq!(
        parse_proc_stat_cpu(line.as_bytes()),
        Some(StatCpu {
            user: 100,
            nice: 20,
            system: 30,
            idle: 400,
            iowait: 50,
            irq: 6,
            softirq: 7,
            steal: 8,
        })
    );
    // An old kernel's four fields: the rest read 0.
    assert_eq!(
        parse_proc_stat_cpu(b"cpu 1 2 3 4\n"),
        Some(StatCpu {
            user: 1,
            nice: 2,
            system: 3,
            idle: 4,
            ..StatCpu::default()
        })
    );
    for bad in [
        "",
        "cpu0 1 2 3 4\n",
        "cpu 1 2 x 4\n",
        "cpu 1 2 3\n",
        "intr 1 2 3 4\ncpu 1 2 3 4\n",
    ] {
        assert_eq!(parse_proc_stat_cpu(bad.as_bytes()), None, "{bad:?}");
    }

    // busy = user+nice+system+irq+softirq+steal, idle = idle+iowait; steal counts busy.
    let a = stat([100, 20, 30, 400, 50, 6, 7, 8]);
    let b = stat([160, 20, 50, 460, 70, 6, 7, 28]);
    // busy 60+0+20+0+0+20 = 100, idle 60+20 = 80.
    let busy = stat_busy(&a, &b).unwrap();
    assert!(close(busy, 100.0 / 180.0), "{busy}");

    // iowait going down (it can, per CPU going idle): saturating, not skipped.
    let c = stat([160, 20, 50, 520, 40, 6, 7, 28]);
    let busy = stat_busy(&b, &c).unwrap();
    assert!(close(busy, 0.0), "{busy}");
    let d = stat([220, 20, 50, 520, 30, 6, 7, 28]);
    assert!(close(stat_busy(&c, &d).unwrap(), 1.0));

    // Nothing moved: skipped, no NaN.
    assert_eq!(stat_busy(&b, &b), None);

    // macOS ticks wrap at u32: the delta is still right.
    let m = |user, system, idle, nice| MachCpu {
        user,
        system,
        idle,
        nice,
    };
    let before = m(u32::MAX - 9, 100, u32::MAX - 29, 0);
    let after = m(20, 110, 39, 0);
    // user 30, system 10 busy; idle 69.
    let busy = mach_busy(&before, &after).unwrap();
    assert!(close(busy, 40.0 / 109.0), "{busy}");
    assert_eq!(mach_busy(&after, &after), None);

    // macOS ticks come per CPU and are summed per state; the sums wrap like
    // the ticks, and busy over the sums is busy over all CPUs.
    let cpu0_before = [u32::MAX - 9, 100, 1000, 0];
    let cpu1_before = [500, u32::MAX, 2000, 7];
    let cpu0_after = [20, 110, 1039, 0]; // user 30, system 10, idle 39
    let cpu1_after = [500, 9, 2030, 7]; // system 10, idle 30
    let sum = |a: [u32; 4], b: [u32; 4]| {
        let units: Vec<u32> = a.into_iter().chain(b).collect();
        sum_cpu_load(2, &units).unwrap()
    };
    let before = sum(cpu0_before, cpu1_before);
    assert_eq!(before, m(u32::MAX.wrapping_add(491), 99, 3000, 7));
    let busy = mach_busy(&before, &sum(cpu0_after, cpu1_after)).unwrap();
    assert!(close(busy, 50.0 / 119.0), "{busy}");
    // The buffer must be exactly `cpus` rows of four, and at least one.
    let one = [1, 2, 3, 4];
    assert_eq!(sum_cpu_load(1, &one), Some(m(1, 2, 3, 4)));
    assert_eq!(sum_cpu_load(2, &one), None);
    assert_eq!(sum_cpu_load(1, &one[..3]), None);
    assert_eq!(sum_cpu_load(1, &[1, 2, 3, 4, 5]), None);
    assert_eq!(sum_cpu_load(0, &[]), None);
}

#[test]
fn proc_stat_reads_only_the_first_line() {
    // Its `intr` line passes the buffer on a large machine.
    let intr = format!("intr {}\n", "123456 ".repeat(2000));
    let file = format!("cpu  10 0 10 80 0 0 0 0 0 0\n{intr}ctxt 1\n");
    let dir = proc_tree(&file, MEMINFO, "100 25 0 0 0 0 0\n");
    let mut sources = ProcSources::open(dir.path(), Ok(4096), Ok(4));
    assert_eq!(
        sources.cpu(),
        Ok(ProcCpu::Stat(stat([10, 0, 10, 80, 0, 0, 0, 0])))
    );

    // A first line longer than the buffer is malformed, not a partial parse.
    let long = format!("cpu  1 2 3 4{}\n", " 0".repeat(BUF_LEN));
    let dir = proc_tree(&long, MEMINFO, "100 25 0 0 0 0 0\n");
    let mut sources = ProcSources::open(dir.path(), Ok(4096), Ok(4));
    assert_eq!(sources.cpu().map_err(|e| e.code), Err(ErrorCode::Malformed));
}

// ---- sampler_parses_memory_fixtures ----

#[test]
fn sampler_parses_memory_fixtures() {
    let info = parse_meminfo(MEMINFO.as_bytes());
    assert_eq!(info.total, Some(16_000_000 * 1024));
    assert_eq!(info.available, Some(4_000_000 * 1024));
    assert_eq!(used_linux(info), Some(12_000_000 * 1024));

    // A kernel before 3.14 has no MemAvailable: the source is disabled.
    let old = "MemTotal:       16000000 kB\nMemFree:         1000000 kB\n";
    assert_eq!(used_linux(parse_meminfo(old.as_bytes())), None);
    let dir = proc_tree("cpu 1 1 1 1\n", old, "100 25 0 0 0 0 0\n");
    let (mut core, info) = Core::start(
        ProcSources::open(dir.path(), Ok(4096), Ok(4)),
        Instant::now(),
    );
    assert_eq!(info.memory_limit, Ok(16_000_000 * 1024));
    let first = core.sample(Instant::now() + TICK);
    assert_eq!(first.memory_used, None);
    assert_eq!(
        first.failed.get(Source::MemoryUsed).map(|e| e.code),
        Some(ErrorCode::Missing)
    );
    core.delivered();
    let second = core.sample(Instant::now() + TICK * 2);
    assert_eq!(second.memory_used, None);
    assert!(second.failed.is_empty(), "named once: {:?}", second.failed);

    // macOS: (internal − purgeable + wired + compressed) pages.
    let vm = MachVm {
        internal: 1000,
        purgeable: 200,
        wired: 300,
        compressed: 100,
    };
    for page in [4096, 16384, 65536] {
        assert_eq!(used_mach(vm, page), 1200 * page);
    }
    // Purgeable above internal saturates rather than wrapping.
    let odd = MachVm {
        internal: 10,
        purgeable: 20,
        ..MachVm::default()
    };
    assert_eq!(used_mach(odd, 4096), 0);

    // A Mach count is what the kernel wrote, at most the buffer handed in:
    // a kernel older than libc's struct writes fewer units (40 of 56 seen on
    // a CI runner), which is fine while it covers every field read.
    const W: &str = "host_statistics64(HOST_VM_INFO64)";
    assert_eq!(mach_result(W, 0, 40, 56, 36), Ok(()));
    assert_eq!(mach_result(W, 0, 56, 56, 36), Ok(()));
    assert_eq!(mach_result(W, 0, 36, 56, 36), Ok(()));
    assert_eq!(
        mach_result(W, 0, 35, 56, 36),
        Err(SourceError::new(W, ErrorCode::Count(35)))
    );
    assert_eq!(
        mach_result(W, 0, 57, 56, 36),
        Err(SourceError::new(W, ErrorCode::Count(57)))
    );
    assert_eq!(
        mach_result(W, 5, 56, 56, 36),
        Err(SourceError::new(W, ErrorCode::Kern(5)))
    );

    // Linux resident: statm's second field × the page size, never 4096 assumed.
    for page in [4096, 16384, 65536] {
        let dir = proc_tree("cpu 1 1 1 1\n", MEMINFO, "100 25 3 1 0 20 0\n");
        let mut sources = ProcSources::open(dir.path(), Ok(page), Ok(4));
        assert_eq!(sources.process_resident(), Ok(25 * page));
    }
    // No page size: the resident source fails, the others do not.
    let dir = proc_tree("cpu 1 1 1 1\n", MEMINFO, "100 25 3 1 0 20 0\n");
    let no_page = SourceError::new("sysconf(_SC_PAGESIZE)", ErrorCode::Malformed);
    let mut sources = ProcSources::open(dir.path(), Err(no_page), Ok(4));
    assert_eq!(sources.process_resident(), Err(no_page));
    assert_eq!(sources.memory_used(), Ok(12_000_000 * 1024));
}

// ---- sampler_reads_cgroup_quota ----

#[test]
fn sampler_reads_cgroup_quota() {
    let leaf = "sys/fs/cgroup/a/b";
    let with = |files: &[(&str, &str)]| {
        let mut all = vec![("proc/self/cgroup", "0::/a/b\n")];
        all.extend_from_slice(files);
        tree(&all)
    };

    // "max": no quota.
    let dir = with(&[(&format!("{leaf}/cpu.max"), "max 100000\n")]);
    assert_eq!(find_quota(dir.path()), None);

    // 200000/100000: 2 CPUs.
    let dir = with(&[(&format!("{leaf}/cpu.max"), "200000 100000\n")]);
    let quota = find_quota(dir.path()).expect("quota");
    assert!(close(quota.cpus, 2.0));
    assert_eq!(quota.stat, dir.path().join(leaf).join("cpu.stat"));

    // A tighter quota on an ancestor wins, and its cpu.stat is read.
    let dir = with(&[
        (&format!("{leaf}/cpu.max"), "200000 100000\n"),
        ("sys/fs/cgroup/a/cpu.max", "50000 100000\n"),
        ("sys/fs/cgroup/cpu.max", "max 100000\n"),
    ]);
    let quota = find_quota(dir.path()).expect("quota");
    assert!(close(quota.cpus, 0.5));
    assert_eq!(quota.stat, dir.path().join("sys/fs/cgroup/a/cpu.stat"));

    // A namespaced cgroup ("0::/") reads the mount's own cpu.max.
    let dir = tree(&[
        ("proc/self/cgroup", "0::/\n"),
        ("sys/fs/cgroup/cpu.max", "150000 100000\n"),
    ]);
    assert!(close(find_quota(dir.path()).expect("quota").cpus, 1.5));

    // cgroup v1, no cgroup file, no cpu.max: the host.
    let v1 = tree(&[
        (
            "proc/self/cgroup",
            "12:cpu,cpuacct:/a/b\n1:name=systemd:/a/b\n",
        ),
        (&format!("{leaf}/cpu.max"), "200000 100000\n"),
    ]);
    assert_eq!(find_quota(v1.path()), None);
    assert_eq!(find_quota(tree(&[]).path()), None);
    assert_eq!(find_quota(with(&[]).path()), None);

    // Malformed cpu.max lines are not a quota.
    for bad in ["", "max", "200000 0", "x 100000", "200000 x"] {
        assert_eq!(parse_cpu_max(bad.as_bytes()), None, "{bad:?}");
    }

    // The limit: the quota only when tighter than the count.
    let host = cpu_limit(Ok(4), Some(200.0)).unwrap();
    assert_eq!(host.from, CpuLimitFrom::Host);
    assert!(close(host.cpus, 4.0));
    assert_eq!(
        cpu_limit(Ok(4), Some(2.0)).unwrap().from,
        CpuLimitFrom::Quota
    );

    // Under a quota, busy comes from usage_usec deltas over limit × interval, clamped.
    assert!(close(
        quota_busy(1_000_000, 2_500_000, TICK, 2.0).unwrap(),
        0.75
    ));
    assert!(close(quota_busy(0, 5_000_000, TICK, 2.0).unwrap(), 1.0));
    assert!(close(
        quota_busy(5_000_000, 1_000_000, TICK, 2.0).unwrap(),
        0.0
    ));
    assert_eq!(quota_busy(0, 1, Duration::ZERO, 2.0), None);

    // End to end over the tree: the quota cgroup's cpu.stat feeds busy.
    let dir = with(&[
        (&format!("{leaf}/cpu.max"), "200000 100000\n"),
        (
            &format!("{leaf}/cpu.stat"),
            "usage_usec 1000000\nuser_usec 1\nsystem_usec 1\n",
        ),
        ("proc/stat", "cpu 1 1 1 1\n"),
        ("proc/meminfo", MEMINFO),
        ("proc/self/statm", "1 1\n"),
    ]);
    let start = Instant::now();
    let (mut core, info) = Core::start(ProcSources::open(dir.path(), Ok(4096), Ok(8)), start);
    assert_eq!(info.cpu_limit.map(|l| l.from), Ok(CpuLimitFrom::Quota));
    std::fs::write(
        dir.path().join(leaf).join("cpu.stat"),
        "usage_usec 2500000\nuser_usec 1\n",
    )
    .unwrap();
    let busy = core.sample(start + TICK).cpu_busy.expect("busy");
    assert!(close(busy, 0.75), "{busy}");
}

// ---- the thread ----

/// Sources a test scripts: busy 0.5 each tick, and faults on demand.
#[derive(Default, Clone)]
pub(crate) struct Fake {
    calls: Arc<[AtomicU32; 4]>,
    /// Fail the memory source from this call on (1-based).
    fail_memory_at: Option<u32>,
    /// Block the CPU source's call this many calls in until the gate opens,
    /// signalling `entered` first.
    block: Option<(u32, Arc<AtomicBool>, std::sync::mpsc::SyncSender<()>)>,
}

impl Fake {
    fn call(&self, source: Source) -> u32 {
        self.calls[source.index()].fetch_add(1, Ordering::SeqCst) + 1
    }
    /// Sources whose memory source fails from its `at`th call on (1-based).
    pub(crate) fn failing_memory_at(at: u32) -> Self {
        Self {
            fail_memory_at: Some(at),
            ..Self::default()
        }
    }
    fn calls(&self, source: Source) -> u32 {
        self.calls[source.index()].load(Ordering::SeqCst)
    }
}

/// A recorder's sampler for the recorder's own tests: `fake`'s sources
/// under a clock that ticks at once, `waits` times, so the sampler sends its
/// info and `waits` samples (one 1 s interval each) and exits. With a
/// `gate`, its sources are built, and so anything read, only once the gate
/// opens. The receiver hears once per sampler started, after its last
/// sample is sent.
pub(crate) fn fake_sampler(
    fake: Fake,
    waits: u32,
    gate: Option<recorder::Gate>,
) -> (recorder::SpawnSampler, Receiver<()>) {
    let (exhausted, sent) = std::sync::mpsc::sync_channel(8);
    let spawn = recorder::SpawnSampler(Arc::new(move |latched| {
        let (fake, gate) = (fake.clone(), gate.clone());
        let clock = FakeClock {
            exhausted: Some(exhausted.clone()),
            ..FakeClock::new(Instant::now(), &[], Some(waits))
        };
        Sampler::spawn_with(
            move || {
                if let Some(gate) = &gate {
                    recorder::wait_for(gate);
                }
                fake
            },
            clock,
            TICK,
            latched,
        )
    }));
    (spawn, sent)
}

/// A recorder's sampler on the real clock with `fake`'s sources, one sample
/// each `tick` until it is stopped: its stamps are real instants, so they
/// fall within the recording.
pub(crate) fn paced_sampler(fake: Fake, tick: Duration) -> recorder::SpawnSampler {
    recorder::SpawnSampler(Arc::new(move |latched| {
        let fake = fake.clone();
        Sampler::spawn_with(move || fake, super::RealClock, tick, latched)
    }))
}

impl Fake {
    /// How many times the CPU source was read: the start's baseline, then
    /// once a sample.
    pub(crate) fn cpu_reads(&self) -> u32 {
        self.calls(Source::CpuBusy)
    }
}

impl Sources for Fake {
    type Cpu = StatCpu;
    fn busy(&self, prev: &StatCpu, cur: &StatCpu, _interval: Duration) -> Option<f64> {
        stat_busy(prev, cur)
    }
    fn info(&mut self) -> MachineInfo {
        MachineInfo {
            cpu_count: Ok(4),
            cpu_limit: Ok(CpuLimit {
                cpus: 4.0,
                from: CpuLimitFrom::Host,
            }),
            memory_limit: Ok(8 << 30),
        }
    }
    fn cpu(&mut self) -> Result<StatCpu, SourceError> {
        let n = self.call(Source::CpuBusy);
        if let Some((at, open, entered)) = &self.block
            && n == *at
        {
            if entered.send(()).is_err() {
                return Err(SourceError::new("fake: test gone", ErrorCode::Missing));
            }
            while !open.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let n = u64::from(n);
        Ok(stat([n * 50, 0, 0, n * 50, 0, 0, 0, 0]))
    }
    fn memory_used(&mut self) -> Result<u64, SourceError> {
        let n = self.call(Source::MemoryUsed);
        match self.fail_memory_at {
            Some(at) if n >= at => Err(SourceError::new("reading fake memory", ErrorCode::Os(5))),
            _ => Ok(1 << 30),
        }
    }
    fn process_cpu(&mut self) -> Result<Duration, SourceError> {
        Ok(Duration::from_millis(u64::from(
            self.call(Source::ProcessCpu),
        )))
    }
    fn process_resident(&mut self) -> Result<u64, SourceError> {
        self.call(Source::ProcessResident);
        Ok(1 << 20)
    }
}

/// Virtual time: each wait lands on its deadline plus the next scripted
/// stall, and the wait after `waits` returns `Stop` (signalling `exhausted`,
/// when set: every sample the loop will send is sent).
struct FakeClock {
    now: Mutex<Instant>,
    stalls: Mutex<VecDeque<Duration>>,
    waits: Mutex<Option<u32>>,
    exhausted: Option<std::sync::mpsc::SyncSender<()>>,
}

impl FakeClock {
    fn new(start: Instant, stalls: &[Duration], waits: Option<u32>) -> Self {
        Self {
            now: Mutex::new(start),
            stalls: Mutex::new(stalls.iter().copied().collect()),
            waits: Mutex::new(waits),
            exhausted: None,
        }
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        *self.now.lock().unwrap()
    }
    fn wait_until(&self, stop: &Receiver<()>, until: Instant) -> Wake {
        if !matches!(stop.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)) {
            return Wake::Stop;
        }
        let mut waits = self.waits.lock().unwrap();
        if let Some(left) = waits.as_mut() {
            if *left == 0 {
                if let Some(exhausted) = &self.exhausted {
                    // Full: the test already knows. Disconnected: it is gone.
                    match exhausted.try_send(()) {
                        Ok(()) | Err(_) => {}
                    }
                }
                return Wake::Stop;
            }
            *left -= 1;
        }
        let stall = self.stalls.lock().unwrap().pop_front().unwrap_or_default();
        *self.now.lock().unwrap() = until + stall;
        Wake::Deadline
    }
}

/// A clock that never advances: a wait ends only on a stop.
struct StillClock(Instant);

impl Clock for StillClock {
    fn now(&self) -> Instant {
        self.0
    }
    fn wait_until(&self, stop: &Receiver<()>, _until: Instant) -> Wake {
        // Ok or Disconnected: either way a stop.
        match stop.recv() {
            Ok(()) | Err(_) => Wake::Stop,
        }
    }
}

/// Run the loop on this thread to its end; every reading it sent.
fn run_to_end(sources: Fake, clock: &impl Clock, cap: usize) -> Vec<Reading> {
    let (out, readings) = std::sync::mpsc::sync_channel(cap);
    let (_stop, stop_rx) = std::sync::mpsc::sync_channel(1);
    run(sources, clock, &stop_rx, &out, TICK, || false);
    drop(out);
    readings.into_iter().collect()
}

fn samples(readings: &[Reading]) -> Vec<Sample> {
    readings
        .iter()
        .filter_map(|r| match r {
            Reading::Sample(s) => Some(*s),
            Reading::Info { .. } => None,
        })
        .collect()
}

#[test]
fn sampler_ticks_at_fixed_rate() {
    let t0 = Instant::now();
    let s = Duration::from_secs;
    let ms = Duration::from_millis;
    // The third wait wakes 2.5 s late: 3.5 s after the sample before it.
    let clock = FakeClock::new(t0, &[ms(0), ms(0), ms(2500)], Some(5));
    let readings = run_to_end(Fake::default(), &clock, 16);
    assert!(matches!(readings.first(), Some(Reading::Info { .. })));
    let got = samples(&readings);
    let at: Vec<Duration> = got.iter().map(|x| x.at.duration_since(t0)).collect();
    let interval: Vec<Duration> = got.iter().map(|x| x.interval).collect();
    // Ticks every second; the missed ticks at 3, 4 and 5 are skipped, and the
    // grid resumes at 6 rather than catching up.
    assert_eq!(at, [s(1), s(2), ms(5500), s(6), s(7)]);
    assert_eq!(interval, [s(1), s(1), ms(3500), ms(500), s(1)]);
    // The first busy figure lands one tick after start: start only read the baseline.
    assert!(
        got.iter()
            .all(|x| x.cpu_busy.is_some_and(|b| close(b, 0.5)))
    );

    // The grid itself.
    assert_eq!(next_tick(t0 + s(3), t0 + ms(5500), TICK), t0 + s(6));
    assert_eq!(next_tick(t0 + s(3), t0 + s(5), TICK), t0 + s(6));
    assert_eq!(next_tick(t0 + s(3), t0 + s(3), TICK), t0 + s(4));
    assert_eq!(next_tick(t0 + s(3), t0 + s(2), TICK), t0 + s(3));
}

#[test]
fn sampler_stops_without_waiting() {
    // End returns with no tick elapsed: the stop ends the wait.
    let (sampler, readings) =
        Sampler::spawn_with(Fake::default, StillClock(Instant::now()), TICK, || false).unwrap();
    assert!(matches!(readings.recv().unwrap(), Reading::Info { .. }));
    let asked = Instant::now();
    assert_eq!(sampler.stop(STOP_WAIT), SamplerExit::Exited);
    assert!(asked.elapsed() < STOP_WAIT * 5);
    assert!(readings.iter().next().is_none(), "no sample taken");
    // Idempotent.
    assert_eq!(sampler.stop(STOP_WAIT), SamplerExit::Exited);

    // A source that blocks does not hang the exit: the wait times out, the
    // thread is detached.
    let open = Arc::new(AtomicBool::new(false));
    let (entered_tx, entered) = std::sync::mpsc::sync_channel(1);
    let fake = Fake {
        block: Some((2, Arc::clone(&open), entered_tx)),
        ..Fake::default()
    };
    let clock = FakeClock::new(Instant::now(), &[], None);
    let (sampler, readings) = Sampler::spawn_with(move || fake, clock, TICK, || false).unwrap();
    entered.recv().unwrap();
    let asked = Instant::now();
    assert_eq!(sampler.stop(STOP_WAIT), SamplerExit::Detached);
    assert!(asked.elapsed() < STOP_WAIT * 5);
    // Once the source returns, the pending stop ends the thread.
    open.store(true, Ordering::SeqCst);
    assert!(readings.iter().count() <= 2, "info, and the blocked tick");

    // Disconnected: the owner dropped without stopping it.
    let (sampler, readings) =
        Sampler::spawn_with(Fake::default, StillClock(Instant::now()), TICK, || false).unwrap();
    assert!(matches!(readings.recv().unwrap(), Reading::Info { .. }));
    drop(sampler);
    assert_eq!(
        readings.recv_timeout(Duration::from_secs(5)),
        Err(RecvTimeoutError::Disconnected)
    );

    // A latched recorder stops it at its next wake.
    let latched = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&latched);
    let clock = FakeClock::new(Instant::now(), &[], None);
    let (sampler, readings) = Sampler::spawn_with(Fake::default, clock, TICK, move || {
        flag.load(Ordering::SeqCst)
    })
    .unwrap();
    assert!(matches!(readings.recv().unwrap(), Reading::Info { .. }));
    assert!(matches!(readings.recv().unwrap(), Reading::Sample(_)));
    latched.store(true, Ordering::SeqCst);
    // Drain what was queued before the latch; then the channel disconnects.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match readings.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(_) => {}
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => panic!("latched sampler kept running"),
        }
    }
    drop(sampler);

    // The readings' receiver gone (the recorder thread exited): it stops.
    let clock = FakeClock::new(Instant::now(), &[], None);
    let (sampler, readings) = Sampler::spawn_with(Fake::default, clock, TICK, || false).unwrap();
    drop(readings);
    let deadline = Instant::now() + Duration::from_secs(5);
    while sampler.done.try_recv() != Err(std::sync::mpsc::TryRecvError::Disconnected) {
        assert!(Instant::now() < deadline, "sampler outlived its receiver");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn sampler_source_failure_stops_only_that_source() {
    let fake = Fake {
        fail_memory_at: Some(2),
        ..Fake::default()
    };
    let clock = FakeClock::new(Instant::now(), &[], Some(5));
    let got = samples(&run_to_end(fake.clone(), &clock, 16));
    assert_eq!(got.len(), 5);
    // Named once, in the sample that saw it fail.
    let named: Vec<usize> = got
        .iter()
        .enumerate()
        .filter(|(_, s)| !s.failed.is_empty())
        .map(|(i, _)| i)
        .collect();
    assert_eq!(named, [1]);
    let failures: Vec<_> = got[1].failed.iter().collect();
    assert_eq!(
        failures,
        [(
            Source::MemoryUsed,
            SourceError::new("reading fake memory", ErrorCode::Os(5))
        )]
    );
    // Then it writes nothing and is never read again.
    assert_eq!(
        got.iter()
            .map(|s| s.memory_used.is_some())
            .collect::<Vec<_>>(),
        [true, false, false, false, false]
    );
    assert_eq!(fake.calls(Source::MemoryUsed), 2);
    // The others keep writing.
    for s in &got {
        assert!(s.cpu_busy.is_some() && s.process_cpu.is_some() && s.process_resident.is_some());
    }
    assert_eq!(fake.calls(Source::ProcessResident), 5);
}

#[test]
fn a_dropped_sample_passes_its_failure_and_count_on() {
    let fake = Fake {
        fail_memory_at: Some(1),
        ..Fake::default()
    };
    let start = Instant::now();
    let (mut core, _) = Core::start(fake, start);
    let first = core.sample(start + TICK);
    assert!(first.failed.get(Source::MemoryUsed).is_some());
    core.missed();
    let second = core.sample(start + TICK * 2);
    assert_eq!(second.dropped, 1);
    assert_eq!(second.failed, first.failed, "still undelivered");
    core.delivered();
    let third = core.sample(start + TICK * 3);
    assert_eq!(third.dropped, 0);
    assert!(third.failed.is_empty());

    // Through the loop: a full channel drops rather than blocks.
    let clock = FakeClock::new(Instant::now(), &[], Some(3));
    let (out, readings) = std::sync::mpsc::sync_channel(2);
    let (_stop, stop_rx) = std::sync::mpsc::sync_channel(1);
    run(Fake::default(), &clock, &stop_rx, &out, TICK, || false);
    // Info and tick 1 queued; ticks 2 and 3 dropped.
    let queued: Vec<Reading> = readings.try_iter().collect();
    assert_eq!(queued.len(), 2);
}
