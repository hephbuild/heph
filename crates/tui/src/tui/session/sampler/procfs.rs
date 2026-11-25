//! Linux's sources: `/proc` and, under a cgroup v2 CPU quota, the quota
//! cgroup's `cpu.stat`. Compiled on every host under `cfg(test)`, over a
//! fixture root, so the parsers and formulas are tested everywhere.

use super::{CpuLimit, CpuLimitFrom, ErrorCode, MachineInfo, SourceError, Sources, cpu_limit};
use std::fs::File;
use std::io::{self, Read as _, Seek as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The one read buffer. The first line of `/proc/stat` and the head of
/// `/proc/meminfo` fit with room to spare.
pub(crate) const BUF_LEN: usize = 4096;

/// `/proc/stat`'s `cpu` line, in USER_HZ ticks. `guest` and `guest_nice`
/// are already inside `user` and `nice`, so they are not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct StatCpu {
    pub(crate) user: u64,
    pub(crate) nice: u64,
    pub(crate) system: u64,
    pub(crate) idle: u64,
    pub(crate) iowait: u64,
    pub(crate) irq: u64,
    pub(crate) softirq: u64,
    pub(crate) steal: u64,
}

/// One read of the CPU counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcCpu {
    /// `/proc/stat`, with no quota.
    Stat(StatCpu),
    /// The quota cgroup's `cpu.stat` `usage_usec`.
    Usage(u64),
}

/// `/proc/meminfo`'s two figures, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct MemInfo {
    pub(crate) total: Option<u64>,
    pub(crate) available: Option<u64>,
}

fn fields(line: &[u8]) -> impl Iterator<Item = &[u8]> {
    line.split(u8::is_ascii_whitespace)
        .filter(|f| !f.is_empty())
}

fn parse_u64(s: &[u8]) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    s.iter().try_fold(0u64, |acc, &b| {
        let digit = b.checked_sub(b'0').filter(|d| *d <= 9)?;
        acc.checked_mul(10)?.checked_add(u64::from(digit))
    })
}

fn first_line(buf: &[u8]) -> &[u8] {
    buf.split(|&b| b == b'\n').next().unwrap_or_default()
}

/// The aggregate `cpu` line: the first of `/proc/stat`. Kernels before 2.6.11
/// lack the trailing fields, which read as 0.
pub(crate) fn parse_proc_stat_cpu(buf: &[u8]) -> Option<StatCpu> {
    let mut f = fields(first_line(buf));
    if f.next()? != b"cpu" {
        return None;
    }
    let mut required = || f.next().and_then(parse_u64);
    let user = required()?;
    let nice = required()?;
    let system = required()?;
    let idle = required()?;
    let mut optional = || match f.next() {
        None => Some(0),
        Some(v) => parse_u64(v),
    };
    Some(StatCpu {
        user,
        nice,
        system,
        idle,
        iowait: optional()?,
        irq: optional()?,
        softirq: optional()?,
        steal: optional()?,
    })
}

/// `MemTotal` and `MemAvailable` from `/proc/meminfo`, kB to bytes.
pub(crate) fn parse_meminfo(buf: &[u8]) -> MemInfo {
    let mut info = MemInfo::default();
    for line in buf.split(|&b| b == b'\n') {
        let mut f = fields(line);
        let slot = match f.next() {
            Some(b"MemTotal:") => &mut info.total,
            Some(b"MemAvailable:") => &mut info.available,
            _ => continue,
        };
        *slot = f
            .next()
            .and_then(parse_u64)
            .and_then(|kb| kb.checked_mul(1024));
        if info.total.is_some() && info.available.is_some() {
            break;
        }
    }
    info
}

/// The resident set, in pages: `/proc/self/statm`'s second field.
pub(crate) fn parse_statm(buf: &[u8]) -> Option<u64> {
    fields(first_line(buf)).nth(1).and_then(parse_u64)
}

/// The process's cgroup v2 path, from `/proc/self/cgroup`'s `0::` line. A
/// cgroup v1 host has no such line.
pub(crate) fn parse_cgroup_v2(buf: &[u8]) -> Option<&str> {
    buf.split(|&b| b == b'\n')
        .find_map(|line| line.strip_prefix(b"0::"))
        .and_then(|path| std::str::from_utf8(path).ok())
        .map(str::trim)
}

/// A `cpu.max`: `Some(None)` for `max` (no quota), `Some(Some(cpus))` for a
/// quota, `None` when malformed.
pub(crate) fn parse_cpu_max(buf: &[u8]) -> Option<Option<f64>> {
    let mut f = fields(first_line(buf));
    let quota = f.next()?;
    let period = f.next().and_then(parse_u64).filter(|p| *p > 0)?;
    if quota == b"max" {
        return Some(None);
    }
    let quota = parse_u64(quota)?;
    Some(Some(quota as f64 / period as f64))
}

/// `usage_usec` from a cgroup's `cpu.stat`.
pub(crate) fn parse_usage_usec(buf: &[u8]) -> Option<u64> {
    buf.split(|&b| b == b'\n').find_map(|line| {
        let mut f = fields(line);
        if f.next()? != b"usage_usec" {
            return None;
        }
        f.next().and_then(parse_u64)
    })
}

/// Busy = user + nice + system + irq + softirq + steal over that plus idle +
/// iowait (iowait counts as idle, as on macOS), from saturating per-field
/// deltas: a field going down, as iowait can, counts 0. `None` when nothing
/// moved, so no NaN is written.
pub(crate) fn stat_busy(a: &StatCpu, b: &StatCpu) -> Option<f64> {
    let d = |x: u64, y: u64| y.saturating_sub(x);
    let busy = [
        d(a.user, b.user),
        d(a.nice, b.nice),
        d(a.system, b.system),
        d(a.irq, b.irq),
        d(a.softirq, b.softirq),
        d(a.steal, b.steal),
    ]
    .into_iter()
    .fold(0u64, u64::saturating_add);
    let total = busy
        .saturating_add(d(a.idle, b.idle))
        .saturating_add(d(a.iowait, b.iowait));
    (total > 0).then(|| (busy as f64 / total as f64).clamp(0.0, 1.0))
}

/// Under a quota: the cgroup's CPU time over `limit` CPUs × the interval,
/// clamped to 0–1.
pub(crate) fn quota_busy(
    prev_usec: u64,
    cur_usec: u64,
    interval: Duration,
    limit: f64,
) -> Option<f64> {
    let capacity = limit * interval.as_micros() as f64;
    (capacity > 0.0).then(|| (cur_usec.saturating_sub(prev_usec) as f64 / capacity).clamp(0.0, 1.0))
}

/// Used memory: `MemTotal − MemAvailable`.
pub(crate) fn used_linux(info: MemInfo) -> Option<u64> {
    Some(info.total?.saturating_sub(info.available?))
}

/// The tightest cgroup v2 CPU quota on the path from this process's cgroup to
/// the root, and the `cpu.stat` of the cgroup that sets it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Quota {
    pub(crate) cpus: f64,
    pub(crate) stat: PathBuf,
}

/// The quota under `root` (`/` outside tests). Cgroup v1, or no `cpu.max` on
/// the path, is no quota.
pub(crate) fn find_quota(root: &Path) -> Option<Quota> {
    let cgroup = std::fs::read(root.join("proc/self/cgroup")).ok()?;
    let rel = parse_cgroup_v2(&cgroup)?;
    let base = root.join("sys/fs/cgroup");
    let mut dir = base.join(rel.trim_start_matches('/'));
    let mut best: Option<Quota> = None;
    loop {
        if let Ok(max) = std::fs::read(dir.join("cpu.max")) {
            match parse_cpu_max(&max) {
                Some(Some(cpus)) if best.as_ref().is_none_or(|b| cpus < b.cpus) => {
                    best = Some(Quota {
                        cpus,
                        stat: dir.join("cpu.stat"),
                    });
                }
                Some(_) => {}
                None => tracing::debug!(path = %dir.display(), "malformed cpu.max; ignored"),
            }
        }
        if dir == base || !dir.pop() || !dir.starts_with(&base) {
            break;
        }
    }
    best
}

/// Read `file` from its start into `buf`, up to end of file, a full buffer
/// or — with `line` — the first newline. What was read.
pub(crate) fn read_head(file: &mut File, buf: &mut [u8], line: bool) -> io::Result<usize> {
    file.rewind()?;
    let mut n = 0;
    while let Some(rest) = buf.get_mut(n..).filter(|r| !r.is_empty()) {
        match file.read(rest) {
            Ok(0) => break,
            Ok(k) => {
                let newline = rest.get(..k).is_some_and(|r| r.contains(&b'\n'));
                n += k;
                if line && newline {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Linux's sources, over files under a root. Each file is opened once and
/// re-read from its start each tick into the one buffer.
pub(crate) struct ProcSources {
    buf: Box<[u8]>,
    /// `/proc/stat`, or under a quota the quota cgroup's `cpu.stat`.
    cpu: Result<File, SourceError>,
    cpu_what: &'static str,
    /// The quota, in CPUs, when busy comes from `cpu.stat`.
    quota: Option<f64>,
    meminfo: Result<File, SourceError>,
    statm: Result<File, SourceError>,
    page_size: Result<u64, SourceError>,
    info: MachineInfo,
}

impl ProcSources {
    const STAT: &str = "reading /proc/stat";
    const CPU_STAT: &str = "reading the quota cgroup's cpu.stat";
    const MEMINFO: &str = "reading /proc/meminfo";
    const STATM: &str = "reading /proc/self/statm";

    /// Open the sources under `root` and read the start-time figures.
    pub(crate) fn open(
        root: &Path,
        page_size: Result<u64, SourceError>,
        cpu_count: Result<u32, SourceError>,
    ) -> Self {
        let open = |path: PathBuf, what| File::open(path).map_err(|e| SourceError::io(what, &e));
        let mut buf = vec![0u8; BUF_LEN].into_boxed_slice();
        let quota = find_quota(root);
        let cpu_limit = cpu_limit(cpu_count, quota.as_ref().map(|q| q.cpus));
        // A quota looser than the CPU count binds nothing: busy is the host's.
        let quota = quota.filter(|_| {
            matches!(
                cpu_limit,
                Ok(CpuLimit {
                    from: CpuLimitFrom::Quota,
                    ..
                })
            )
        });
        let (cpu, cpu_what) = match &quota {
            Some(q) => (open(q.stat.clone(), Self::CPU_STAT), Self::CPU_STAT),
            None => (open(root.join("proc/stat"), Self::STAT), Self::STAT),
        };
        let mut meminfo = open(root.join("proc/meminfo"), Self::MEMINFO);
        let memory_limit = match &mut meminfo {
            Ok(file) => read_head(file, &mut buf, false)
                .map_err(|e| SourceError::io(Self::MEMINFO, &e))
                .and_then(|n| {
                    parse_meminfo(buf.get(..n).unwrap_or_default())
                        .total
                        .ok_or(SourceError::new(Self::MEMINFO, ErrorCode::Missing))
                }),
            Err(e) => Err(*e),
        };
        Self {
            buf,
            cpu,
            cpu_what,
            quota: quota.map(|q| q.cpus),
            meminfo,
            statm: open(root.join("proc/self/statm"), Self::STATM),
            page_size,
            info: MachineInfo {
                cpu_count,
                cpu_limit,
                memory_limit,
            },
        }
    }
}

impl Sources for ProcSources {
    type Cpu = ProcCpu;

    fn info(&mut self) -> MachineInfo {
        self.info
    }

    fn cpu(&mut self) -> Result<ProcCpu, SourceError> {
        let what = self.cpu_what;
        let file = self.cpu.as_mut().map_err(|e| *e)?;
        // `/proc/stat`'s first line only: its `intr` line passes 4 KiB on
        // large machines.
        let first_line_only = self.quota.is_none();
        let n = read_head(file, &mut self.buf, first_line_only)
            .map_err(|e| SourceError::io(what, &e))?;
        let head = self.buf.get(..n).unwrap_or_default();
        let reading = if !first_line_only {
            parse_usage_usec(head).map(ProcCpu::Usage)
        } else if n == self.buf.len() && !head.contains(&b'\n') {
            // The line did not fit: a partial line would parse to wrong figures.
            None
        } else {
            parse_proc_stat_cpu(head).map(ProcCpu::Stat)
        };
        reading.ok_or(SourceError::new(what, ErrorCode::Malformed))
    }

    fn busy(&self, prev: &ProcCpu, cur: &ProcCpu, interval: Duration) -> Option<f64> {
        match (prev, cur) {
            (ProcCpu::Stat(a), ProcCpu::Stat(b)) => stat_busy(a, b),
            (ProcCpu::Usage(a), ProcCpu::Usage(b)) => quota_busy(*a, *b, interval, self.quota?),
            _ => None,
        }
    }

    fn memory_used(&mut self) -> Result<u64, SourceError> {
        let file = self.meminfo.as_mut().map_err(|e| *e)?;
        let n = read_head(file, &mut self.buf, false)
            .map_err(|e| SourceError::io(Self::MEMINFO, &e))?;
        used_linux(parse_meminfo(self.buf.get(..n).unwrap_or_default()))
            .ok_or(SourceError::new(Self::MEMINFO, ErrorCode::Missing))
    }

    fn process_cpu(&mut self) -> Result<Duration, SourceError> {
        super::process_cpu()
    }

    fn process_resident(&mut self) -> Result<u64, SourceError> {
        let page_size = self.page_size?;
        let file = self.statm.as_mut().map_err(|e| *e)?;
        let n =
            read_head(file, &mut self.buf, true).map_err(|e| SourceError::io(Self::STATM, &e))?;
        parse_statm(self.buf.get(..n).unwrap_or_default())
            .map(|pages| pages.saturating_mul(page_size))
            .ok_or(SourceError::new(Self::STATM, ErrorCode::Malformed))
    }
}

/// The host's sources: `/`, the page size from `sysconf(_SC_PAGESIZE)`.
#[cfg(target_os = "linux")]
pub(crate) fn host_sources() -> ProcSources {
    ProcSources::open(
        Path::new("/"),
        super::sysconf(libc::_SC_PAGESIZE, "sysconf(_SC_PAGESIZE)"),
        super::cpu_count(),
    )
}
