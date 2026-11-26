//! macOS's sources: Mach host and task statistics, `sysctl` and libc. The
//! formulas are compiled on every host under `cfg(test)`, so their fixtures
//! run everywhere; the FFI only on macOS.

use super::{ErrorCode, SourceError};

/// The host's CPU ticks: `PROCESSOR_CPU_LOAD_INFO`'s per-CPU ticks summed
/// over the CPUs, 32-bit and wrapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct MachCpu {
    pub(crate) user: u32,
    pub(crate) system: u32,
    pub(crate) idle: u32,
    pub(crate) nice: u32,
}

/// The `HOST_VM_INFO64` page counts the used-memory formula reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct MachVm {
    pub(crate) internal: u32,
    pub(crate) purgeable: u32,
    pub(crate) wired: u32,
    pub(crate) compressed: u32,
}

/// Busy = user + system + nice over those plus idle, from wrapping deltas:
/// the ticks are 32-bit and wrap after weeks of uptime. `None` when nothing
/// moved, so no NaN is written.
pub(crate) fn mach_busy(a: &MachCpu, b: &MachCpu) -> Option<f64> {
    let d = |x: u32, y: u32| u64::from(y.wrapping_sub(x));
    let busy = d(a.user, b.user) + d(a.system, b.system) + d(a.nice, b.nice);
    let total = busy + d(a.idle, b.idle);
    (total > 0).then(|| (busy as f64 / total as f64).clamp(0.0, 1.0))
}

/// `PROCESSOR_CPU_LOAD_INFO`'s buffer, `cpus` × (user, system, idle, nice)
/// in that order, summed per state. The sums wrap like the ticks they add:
/// a wrapping delta of sums is the sum of the per-CPU deltas, so `mach_busy`
/// reads them as it would one CPU's. That holds only while the processor
/// count is the same between the two reads: a CPU brought on or offline in
/// between adds or drops its whole cumulative count from one sum, and that
/// interval's figure is wrong (each read checks its buffer against its own
/// count; nothing compares one read's count with the last's). `None`
/// when the buffer is not exactly `cpus` such rows, or there is no CPU.
pub(crate) fn sum_cpu_load(cpus: u32, units: &[u32]) -> Option<MachCpu> {
    let (rows, []) = units.as_chunks::<4>() else {
        return None;
    };
    if rows.is_empty() || u32::try_from(rows.len()).ok() != Some(cpus) {
        return None;
    }
    Some(
        rows.iter()
            .fold(MachCpu::default(), |acc, &[user, system, idle, nice]| {
                MachCpu {
                    user: acc.user.wrapping_add(user),
                    system: acc.system.wrapping_add(system),
                    idle: acc.idle.wrapping_add(idle),
                    nice: acc.nice.wrapping_add(nice),
                }
            }),
    )
}

/// Used memory, Activity Monitor's Memory Used: (internal − purgeable +
/// wired + compressed) pages.
pub(crate) fn used_mach(vm: MachVm, page_size: u64) -> u64 {
    let pages = u64::from(vm.internal.saturating_sub(vm.purgeable))
        + u64::from(vm.wired)
        + u64::from(vm.compressed);
    pages.saturating_mul(page_size)
}

/// A Mach info call's result: `KERN_SUCCESS` (0), and a count that covers
/// the fields read without passing the buffer.
///
/// `passed` is the buffer's size in `integer_t`/`natural_t` units, handed in
/// through the count argument; on return the count is how many units the
/// kernel wrote. A kernel whose struct is older than libc's writes fewer
/// (xnu fills a flavor's revision up to what the caller's count allows and
/// reports that revision's count: `HOST_VM_INFO64` is 38 or 40 units on
/// kernels before libc's tag-storage fields), so equality with libc's count
/// is the wrong test. `needed` is the units up to the end of the last field
/// read; the rest of the buffer stays as it was handed in.
pub(crate) fn mach_result(
    what: &'static str,
    kr: i32,
    count: u32,
    passed: u32,
    needed: u32,
) -> Result<(), SourceError> {
    if kr != 0 {
        return Err(SourceError::new(what, ErrorCode::Kern(kr)));
    }
    if count < needed || count > passed {
        return Err(SourceError::new(what, ErrorCode::Count(count)));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) use ffi::host_sources;

#[cfg(target_os = "macos")]
mod ffi {
    use super::super::{ErrorCode, MachineInfo, SourceError, Sources, cpu_count, cpu_limit};
    use super::{MachCpu, MachVm, mach_busy, mach_result, sum_cpu_load, used_mach};
    use std::mem::offset_of;
    use std::time::Duration;

    // The order and width `sum_cpu_load` reads one CPU's ticks in.
    const _: () = assert!(
        libc::CPU_STATE_USER == 0
            && libc::CPU_STATE_SYSTEM == 1
            && libc::CPU_STATE_IDLE == 2
            && libc::CPU_STATE_NICE == 3
            && libc::CPU_STATE_MAX == 4
            && size_of::<libc::processor_cpu_load_info>() == 4 * size_of::<libc::integer_t>()
            && size_of::<libc::integer_t>() == size_of::<u32>()
    );

    /// The host's sources.
    pub(crate) fn host_sources() -> MachSources {
        MachSources::open()
    }

    const _: () = assert!(libc::KERN_SUCCESS == 0);

    /// The `integer_t` units from a struct's start to the end of a field
    /// ending `end` bytes in: what a Mach count must cover for it to be read.
    const fn units_to(end: usize) -> u32 {
        let units = end.div_ceil(size_of::<libc::integer_t>());
        assert!(units <= u32::MAX as usize);
        units as u32
    }

    /// `HOST_VM_INFO64` up to `internal_page_count`, the last field read.
    const VM_INFO64_NEEDED: u32 = units_to(
        offset_of!(libc::vm_statistics64, internal_page_count) + size_of::<libc::natural_t>(),
    );
    const _: () = assert!(
        offset_of!(libc::vm_statistics64, wire_count)
            < offset_of!(libc::vm_statistics64, internal_page_count)
            && offset_of!(libc::vm_statistics64, purgeable_count)
                < offset_of!(libc::vm_statistics64, internal_page_count)
            && offset_of!(libc::vm_statistics64, compressor_page_count)
                < offset_of!(libc::vm_statistics64, internal_page_count)
    );

    /// Physical RAM.
    fn memsize() -> Result<u64, SourceError> {
        const WHAT: &str = "sysctlbyname(hw.memsize)";
        let mut bytes: u64 = 0;
        let mut len = size_of::<u64>();
        // SAFETY: the name is NUL-terminated; `bytes` is writable for `len`
        // bytes, and sysctl writes at most `len` and stores what it wrote in
        // `len`; no new value is set (null, 0).
        let rc = unsafe {
            libc::sysctlbyname(
                c"hw.memsize".as_ptr(),
                (&raw mut bytes).cast(),
                &raw mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Err(SourceError::io(WHAT, &std::io::Error::last_os_error()));
        }
        if len != size_of::<u64>() {
            return Err(SourceError::new(
                WHAT,
                ErrorCode::Count(u32::try_from(len).unwrap_or(u32::MAX)),
            ));
        }
        Ok(bytes)
    }

    /// macOS's sources. Owns a send right to the host port, taken once at
    /// start and deallocated on drop.
    pub(crate) struct MachSources {
        host: Option<libc::mach_port_t>,
        page_size: Result<u64, SourceError>,
        info: MachineInfo,
    }

    impl MachSources {
        const NO_HOST: SourceError = SourceError::new("mach_host_self", ErrorCode::Missing);

        fn open() -> Self {
            // SAFETY: mach_host_self takes no argument; it returns a send right
            // to the host port (or MACH_PORT_NULL), which `Self` owns and
            // deallocates once, in `drop`.
            let host = unsafe { mach2::mach_init::mach_host_self() };
            let host = (host != mach2::port::MACH_PORT_NULL).then_some(host);
            // SAFETY: libSystem sets `vm_kernel_page_size` before `main` and
            // never writes it again: a plain read of an initialised integer.
            let page_size = unsafe { mach2::vm_page_size::vm_kernel_page_size };
            let page_size =
                u64::try_from(page_size)
                    .ok()
                    .filter(|p| *p > 0)
                    .ok_or(SourceError::new(
                        "vm_kernel_page_size",
                        ErrorCode::Malformed,
                    ));
            let cpu_count = cpu_count();
            Self {
                host,
                page_size,
                info: MachineInfo {
                    cpu_count,
                    cpu_limit: cpu_limit(cpu_count, None),
                    memory_limit: memsize(),
                },
            }
        }
    }

    impl Drop for MachSources {
        fn drop(&mut self) {
            let Some(host) = self.host.take() else {
                return;
            };
            // SAFETY: mach_task_self reads this task's own port name, set at
            // process start; it takes no argument.
            let task = unsafe { mach2::traps::mach_task_self() };
            // SAFETY: `host` is the send right mach_host_self returned, owned
            // by `self` and deallocated only here (taken above).
            let kr = unsafe { mach2::mach_port::mach_port_deallocate(task, host) };
            if kr != mach2::kern_return::KERN_SUCCESS {
                tracing::debug!(kr, "cannot deallocate the host port");
            }
        }
    }

    /// A buffer the kernel mapped into this task for an out-of-line Mach
    /// reply, `units` integer_t long, `vm_deallocate`d on drop. Not a Rust
    /// allocation: the sampler's no-allocation-per-sample promise holds.
    struct KernelBuf {
        addr: libc::processor_info_array_t,
        units: libc::mach_msg_type_number_t,
    }

    impl KernelBuf {
        /// The buffer as unsigned units: `PROCESSOR_CPU_LOAD_INFO`'s ticks
        /// are `natural_t`, which the reply types as `integer_t`.
        fn units(&self) -> &[u32] {
            if self.addr.is_null() {
                return &[];
            }
            // SAFETY: the kernel mapped `units` integer_t at `addr`, page
            // aligned and readable, and nothing unmaps it before `self` drops;
            // u32 has integer_t's size and alignment, and every bit pattern is
            // a valid u32.
            unsafe { std::slice::from_raw_parts(self.addr.cast::<u32>(), self.units as usize) }
        }
    }

    impl Drop for KernelBuf {
        fn drop(&mut self) {
            if self.addr.is_null() {
                return;
            }
            let bytes = (self.units as usize).saturating_mul(size_of::<libc::integer_t>());
            // SAFETY: mach_task_self reads this task's own port name, set at
            // process start; it takes no argument.
            let task = unsafe { mach2::traps::mach_task_self() };
            // SAFETY: `addr` is the kernel's mapping of `bytes` bytes into this
            // task, owned by `self` and deallocated only here; no slice from
            // `units()` outlives `self`.
            let kr = unsafe { libc::vm_deallocate(task, self.addr.addr(), bytes) };
            if kr != libc::KERN_SUCCESS {
                tracing::debug!(kr, "cannot deallocate host_processor_info's buffer");
            }
        }
    }

    impl Sources for MachSources {
        type Cpu = MachCpu;

        fn info(&mut self) -> MachineInfo {
            self.info
        }

        /// The CPU ticks from `host_processor_info`, not
        /// `host_statistics64(HOST_CPU_LOAD_INFO)`: xnu rate-limits
        /// `host_statistics64` per flavor machine-wide for non-platform
        /// processes and answers over the limit with a cached copy and
        /// `KERN_SUCCESS`, so with a few pollers a tick would read stale
        /// ticks. `host_processor_info` is not rate-limited.
        fn cpu(&mut self) -> Result<MachCpu, SourceError> {
            const WHAT: &str = "host_processor_info(PROCESSOR_CPU_LOAD_INFO)";
            let host = self.host.ok_or(Self::NO_HOST)?;
            let mut cpus: libc::natural_t = 0;
            let mut info: libc::processor_info_array_t = std::ptr::null_mut();
            let mut units: libc::mach_msg_type_number_t = 0;
            // SAFETY: the three out-pointers are live locals of the types the
            // call writes; `host` is a live send right to the host port. On
            // success the kernel maps a buffer of `units` integer_t into this
            // task and hands its address in `info`; it is ours to deallocate,
            // which `KernelBuf` does.
            let kr = unsafe {
                libc::host_processor_info(
                    host,
                    libc::PROCESSOR_CPU_LOAD_INFO,
                    &raw mut cpus,
                    &raw mut info,
                    &raw mut units,
                )
            };
            // Owned before anything can return: deallocated on every path. A
            // failed call writes no buffer and `info` stays null.
            let buf = KernelBuf { addr: info, units };
            if kr != libc::KERN_SUCCESS {
                return Err(SourceError::new(WHAT, ErrorCode::Kern(kr)));
            }
            sum_cpu_load(cpus, buf.units()).ok_or(SourceError::new(WHAT, ErrorCode::Count(units)))
        }

        fn busy(&self, prev: &MachCpu, cur: &MachCpu, _interval: Duration) -> Option<f64> {
            mach_busy(prev, cur)
        }

        fn memory_used(&mut self) -> Result<u64, SourceError> {
            const WHAT: &str = "host_statistics64(HOST_VM_INFO64)";
            let host = self.host.ok_or(Self::NO_HOST)?;
            let page_size = self.page_size?;
            // SAFETY: vm_statistics64 is plain integers, for which all-zero
            // bytes are a valid value.
            let mut vm: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
            let mut count = libc::HOST_VM_INFO64_COUNT;
            // SAFETY: `vm` is libc's vm_statistics64 and `count` libc's count
            // for it, in integer_t units, so the kernel writes at most the
            // struct; `host` is a live send right to the host port.
            let kr = unsafe {
                libc::host_statistics64(
                    host,
                    libc::HOST_VM_INFO64,
                    (&raw mut vm).cast(),
                    &raw mut count,
                )
            };
            mach_result(
                WHAT,
                kr,
                count,
                libc::HOST_VM_INFO64_COUNT,
                VM_INFO64_NEEDED,
            )?;
            Ok(used_mach(
                MachVm {
                    internal: vm.internal_page_count,
                    purgeable: vm.purgeable_count,
                    wired: vm.wire_count,
                    compressed: vm.compressor_page_count,
                },
                page_size,
            ))
        }

        fn process_cpu(&mut self) -> Result<Duration, SourceError> {
            super::super::process_cpu()
        }

        fn process_resident(&mut self) -> Result<u64, SourceError> {
            const WHAT: &str = "task_info(MACH_TASK_BASIC_INFO)";
            // SAFETY: mach_task_basic_info is plain integers, for which
            // all-zero bytes are a valid value.
            let mut info: libc::mach_task_basic_info = unsafe { std::mem::zeroed() };
            let mut count = libc::MACH_TASK_BASIC_INFO_COUNT;
            // SAFETY: mach_task_self reads this task's own port name, set at
            // process start; it takes no argument.
            let task = unsafe { mach2::traps::mach_task_self() };
            // SAFETY: `info` is libc's mach_task_basic_info and `count` libc's
            // count for it, in natural_t units, so the kernel writes at most
            // the struct; `task` is this task's own port.
            let kr = unsafe {
                libc::task_info(
                    task,
                    libc::MACH_TASK_BASIC_INFO,
                    (&raw mut info).cast(),
                    &raw mut count,
                )
            };
            mach_result(
                WHAT,
                kr,
                count,
                libc::MACH_TASK_BASIC_INFO_COUNT,
                units_to(offset_of!(libc::mach_task_basic_info, resident_size) + size_of::<u64>()),
            )?;
            Ok(info.resident_size)
        }
    }
}
