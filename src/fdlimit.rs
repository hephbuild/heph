//! Raise the process's open-file (`RLIMIT_NOFILE`) soft limit at startup.
//!
//! heph holds an advisory `flock(2)` fd per in-use cached artifact (the read
//! lock guarding the cache entry while a consumer reads it — see
//! [`crate::engine::result_lock`]). A wide build keeps many cache entries live
//! at once, so the default soft limit (just **256** on macOS) is exhausted
//! almost immediately with `Too many open files`. Raise the soft limit to the
//! hard limit, clamped on macOS to the per-process kernel cap.

use libc::rlim_t;

/// Fallback target when the hard limit is unbounded and no per-process cap is
/// known. 1,048,576 is the common Linux `fs.nr_open` default; if the kernel
/// rejects it, [`raise_open_file_limit`] steps down rather than staying stuck.
const UNBOUNDED_FALLBACK: rlim_t = 1_048_576;

/// Decide the new soft limit given the current soft (`cur`) and hard (`max`)
/// limits and, on macOS, the `kern.maxfilesperproc` cap (`per_proc`). Returns
/// `Some(new_soft)` when a raise is worthwhile, or `None` to leave it as-is.
///
/// Pure so it can be tested without mutating process-global rlimits (which would
/// flake parallel tests that open files).
fn compute_target(cur: rlim_t, max: rlim_t, per_proc: Option<rlim_t>) -> Option<rlim_t> {
    let mut target = max;
    match per_proc {
        // macOS rejects a soft limit above `kern.maxfilesperproc`.
        Some(cap) if cap > 0 => target = target.min(cap),
        // No usable cap and an unbounded hard limit: don't ask for infinity.
        _ if max == libc::RLIM_INFINITY => target = UNBOUNDED_FALLBACK,
        _ => {}
    }
    (target > cur).then_some(target)
}

/// macOS per-process open-file cap (`kern.maxfilesperproc`), if readable.
#[cfg(target_os = "macos")]
fn macos_per_proc_cap() -> Option<rlim_t> {
    let mut per_proc: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    // SAFETY: reads a single c_int sysctl into a stack local.
    let ok = unsafe {
        libc::sysctlbyname(
            c"kern.maxfilesperproc".as_ptr(),
            &mut per_proc as *mut _ as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } == 0;
    if !ok {
        return None;
    }
    rlim_t::try_from(per_proc).ok().filter(|&n| n > 0)
}

#[cfg(not(target_os = "macos"))]
fn macos_per_proc_cap() -> Option<rlim_t> {
    None
}

/// Raise the open-file soft limit toward the hard limit. Best-effort: any
/// failure is logged at debug and otherwise ignored — the caller proceeds with
/// whatever limit is in effect.
pub fn raise_open_file_limit() {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes the current limits into our stack `rlimit`.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        tracing::debug!(error = %std::io::Error::last_os_error(), "getrlimit(RLIMIT_NOFILE)");
        return;
    }

    let orig = lim.rlim_cur;
    let Some(target) = compute_target(orig, lim.rlim_max, macos_per_proc_cap()) else {
        return;
    };

    // Try the target, then halve on rejection (the kernel caps an individual
    // process's fds below the hard limit — e.g. `fs.nr_open` on Linux — and a
    // too-high request fails outright). Stepping down guarantees we never leave
    // the soft limit stuck at its low original.
    let mut attempt = target;
    while attempt > orig {
        lim.rlim_cur = attempt;
        // SAFETY: setrlimit reads the limits from our stack `rlimit`.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) } == 0 {
            return;
        }
        attempt /= 2;
    }
    tracing::debug!(
        error = %std::io::Error::last_os_error(),
        target,
        "setrlimit(RLIMIT_NOFILE) could not raise soft limit",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raises_low_soft_to_hard() {
        assert_eq!(compute_target(256, 4096, None), Some(4096));
    }

    #[test]
    fn macos_clamps_to_per_proc_cap() {
        // Hard limit unbounded, but the kernel per-process cap bounds the raise.
        assert_eq!(
            compute_target(256, libc::RLIM_INFINITY, Some(122_880)),
            Some(122_880)
        );
    }

    #[test]
    fn unbounded_hard_without_cap_uses_fallback() {
        assert_eq!(
            compute_target(256, libc::RLIM_INFINITY, None),
            Some(UNBOUNDED_FALLBACK)
        );
    }

    #[test]
    fn no_raise_when_already_at_or_above_target() {
        // Soft already at the per-proc cap → nothing to do.
        assert_eq!(
            compute_target(122_880, libc::RLIM_INFINITY, Some(122_880)),
            None
        );
        // Soft already above the hard limit (macOS can report this).
        assert_eq!(compute_target(1_048_576, 4096, None), None);
    }
}
