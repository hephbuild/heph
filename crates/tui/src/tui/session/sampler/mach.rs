//! macOS's sources: Mach host and task statistics, `sysctl` and libc. The
//! formulas are compiled on every host under `cfg(test)`, so their fixtures
//! run everywhere; the FFI only on macOS.

use super::{ErrorCode, SourceError};

/// `HOST_CPU_LOAD_INFO`'s ticks, 32-bit and wrapping.
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
    use super::{MachCpu, MachVm, mach_busy, mach_result, used_mach};
    use std::mem::offset_of;
    use std::time::Duration;

    // The order `cpu` destructures `cpu_ticks` in.
    const _: () = assert!(
        libc::CPU_STATE_USER == 0
            && libc::CPU_STATE_SYSTEM == 1
            && libc::CPU_STATE_IDLE == 2
            && libc::CPU_STATE_NICE == 3
            && libc::CPU_STATE_MAX == 4
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

    impl Sources for MachSources {
        type Cpu = MachCpu;

        fn info(&mut self) -> MachineInfo {
            self.info
        }

        fn cpu(&mut self) -> Result<MachCpu, SourceError> {
            const WHAT: &str = "host_statistics64(HOST_CPU_LOAD_INFO)";
            let host = self.host.ok_or(Self::NO_HOST)?;
            let mut load = libc::host_cpu_load_info {
                cpu_ticks: [0; libc::CPU_STATE_MAX as usize],
            };
            let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
            // SAFETY: `load` is libc's host_cpu_load_info and `count` libc's
            // count for it, in integer_t units, so the kernel writes at most
            // the struct; `host` is a live send right to the host port.
            let kr = unsafe {
                libc::host_statistics64(
                    host,
                    libc::HOST_CPU_LOAD_INFO,
                    (&raw mut load).cast(),
                    &raw mut count,
                )
            };
            mach_result(
                WHAT,
                kr,
                count,
                libc::HOST_CPU_LOAD_INFO_COUNT,
                libc::HOST_CPU_LOAD_INFO_COUNT,
            )?;
            let [user, system, idle, nice] = load.cpu_ticks;
            Ok(MachCpu {
                user,
                system,
                idle,
                nice,
            })
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
                units_to(
                    offset_of!(libc::mach_task_basic_info, resident_size) + size_of::<u64>(),
                ),
            )?;
            Ok(info.resident_size)
        }
    }
}
