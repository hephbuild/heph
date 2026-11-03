//! What the host lets this process use: the memory capacity the engine's
//! `memory` pool defaults to.
//!
//! One meaning on every platform — the memory this process may use — reached by
//! two reads:
//!
//! - **Linux**: the tightest cgroup memory limit on this process's cgroup path
//!   (v2 `memory.max`, v1 `memory.limit_in_bytes`), capped by physical memory;
//!   physical memory when no limit is set. A container's limit is what an OOM
//!   kill enforces, so it, not the host's RAM, is what fits.
//! - **macOS**: physical memory (`sysctl hw.memsize`); there are no cgroups.
//!
//! The cgroup half is a pure function over the text of `/proc/self/cgroup` and a
//! filesystem root ([`cgroup_memory_limit`]), so it is tested from fixtures on
//! every platform rather than only on a Linux box that happens to be limited.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Where cgroup filesystems are mounted on Linux.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// The CPUs this process may use: the engine's default `parallelism`, and so
/// the `cpu` pool's default capacity, and what `heph.core.resource("cpu")`
/// reports. `available_parallelism` (cgroup- and affinity-aware on Linux),
/// falling back to 1 when it cannot be read.
///
/// The host's number, never a run's: `-j` and `--resource cpu=` set the pool's
/// capacity over it and leave this alone.
pub fn host_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// The memory this process may use, in bytes: the `memory` pool's default
/// capacity. Read once per process and cached.
///
/// 0 only when neither a cgroup limit nor physical memory could be read, which
/// the pool treats as "must be set" (an explicit capacity is then required for
/// a target that declares memory) rather than inventing a number.
pub fn host_memory_bytes() -> u64 {
    static CACHED: OnceLock<u64> = OnceLock::new();
    *CACHED.get_or_init(read_host_memory_bytes)
}

fn read_host_memory_bytes() -> u64 {
    let physical = physical_memory_bytes();
    let cgroup = linux_cgroup_limit();
    let capacity = memory_capacity(physical, cgroup);
    if capacity == 0 {
        tracing::warn!(
            "could not read this host's memory; the memory pool has no default capacity"
        );
    }
    capacity
}

#[cfg(target_os = "linux")]
fn linux_cgroup_limit() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    cgroup_memory_limit(&text, Path::new(CGROUP_ROOT))
}

#[cfg(not(target_os = "linux"))]
fn linux_cgroup_limit() -> Option<u64> {
    None
}

/// Combine the two reads: the cgroup limit caps physical memory; either alone
/// is the answer when the other is unknown; 0 when both are.
pub fn memory_capacity(physical: Option<u64>, cgroup_limit: Option<u64>) -> u64 {
    match (physical, cgroup_limit) {
        (Some(p), Some(c)) => p.min(c),
        (Some(n), None) | (None, Some(n)) => n,
        (None, None) => 0,
    }
}

/// The tightest memory limit set on the cgroup path described by
/// `proc_self_cgroup` (the text of `/proc/self/cgroup`), with cgroup
/// filesystems mounted under `root` (`/sys/fs/cgroup` on a real host).
///
/// Every directory from the process's cgroup up to the hierarchy's root is
/// read, because a limit on an ancestor binds its descendants, and because
/// inside a container without a cgroup namespace the path names the host's
/// hierarchy while `root` is the container's own cgroup — the walk then
/// reaches the container's limit at the top.
///
/// - v2 (`0::<path>`): `<root>/<path>/memory.max`; `max` means unset.
/// - v1 (`<n>:<controllers incl. memory>:<path>`):
///   `<root>/<controllers>/<path>/memory.limit_in_bytes`. v1 has no "unset"
///   spelling: an unlimited cgroup reports a huge sentinel, which the caller's
///   `min` with physical memory discards.
///
/// Unreadable files, unparsable values, a `0` limit and malformed lines are
/// skipped: a garbage read means "no limit known", never a capacity of 0.
pub fn cgroup_memory_limit(proc_self_cgroup: &str, root: &Path) -> Option<u64> {
    let mut tightest: Option<u64> = None;
    for line in proc_self_cgroup.lines() {
        let mut fields = line.splitn(3, ':');
        let (Some(_id), Some(controllers), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (base, file) = if controllers.is_empty() {
            (root.to_path_buf(), "memory.max")
        } else if controllers.split(',').any(|c| c == "memory") {
            (root.join(controllers), "memory.limit_in_bytes")
        } else {
            continue;
        };
        let Some(dirs) = cgroup_dirs(&base, path) else {
            continue;
        };
        for dir in dirs {
            if let Some(limit) = read_limit(&dir.join(file)) {
                tightest = Some(tightest.map_or(limit, |t| t.min(limit)));
            }
        }
    }
    tightest
}

/// `base/<path>` and each of its ancestors up to `base`, deepest first. `None`
/// for a path that is not absolute or that tries to climb (`..`).
fn cgroup_dirs(base: &Path, path: &str) -> Option<Vec<PathBuf>> {
    let rel = path.strip_prefix('/')?;
    let mut dir = base.to_path_buf();
    let mut dirs = vec![dir.clone()];
    for component in rel.split('/').filter(|c| !c.is_empty()) {
        if component == "." || component == ".." {
            return None;
        }
        dir.push(component);
        dirs.push(dir.clone());
    }
    dirs.reverse();
    Some(dirs)
}

fn read_limit(file: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(file).ok()?;
    let value = text.trim();
    if value == "max" {
        return None;
    }
    value.parse::<u64>().ok().filter(|n| *n > 0)
}

/// Physical memory in bytes, `None` when the host will not say.
#[cfg(target_os = "linux")]
pub fn physical_memory_bytes() -> Option<u64> {
    // SAFETY: `sysconf` reads a system constant; no pointers are involved.
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    // SAFETY: as above.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let pages = u64::try_from(pages).ok()?;
    let page_size = u64::try_from(page_size).ok()?;
    pages.checked_mul(page_size).filter(|n| *n > 0)
}

/// Physical memory in bytes, `None` when the host will not say.
#[cfg(target_os = "macos")]
pub fn physical_memory_bytes() -> Option<u64> {
    let mut value: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: the name is a NUL-terminated literal, `value` is a live u64 and
    // `len` holds its exact size, which is what `hw.memsize` writes.
    let rc = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&raw mut value).cast::<libc::c_void>(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && len == std::mem::size_of::<u64>() && value > 0).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn write(root: &Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn memory_capacity_is_positive() {
        assert!(host_memory_bytes() > 0);
        assert!(physical_memory_bytes().is_some_and(|n| n > 0));
    }

    #[test]
    fn cgroup_v2_limit_caps_physical() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "user.slice/memory.max",
            &format!("{}\n", 2 * GIB),
        );
        let limit = cgroup_memory_limit("0::/user.slice\n", dir.path());
        assert_eq!(limit, Some(2 * GIB));
        assert_eq!(memory_capacity(Some(16 * GIB), limit), 2 * GIB);
        // A limit above physical memory does not raise the capacity.
        assert_eq!(memory_capacity(Some(GIB), limit), GIB);
    }

    #[test]
    fn cgroup_v2_max_means_physical() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "user.slice/memory.max", "max\n");
        write(dir.path(), "memory.max", "max\n");
        let limit = cgroup_memory_limit("0::/user.slice\n", dir.path());
        assert_eq!(limit, None);
        assert_eq!(memory_capacity(Some(16 * GIB), limit), 16 * GIB);
    }

    #[test]
    fn cgroup_v1_sentinel_means_physical() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "memory/docker/abc/memory.limit_in_bytes",
            "9223372036854771712\n",
        );
        let text = "12:cpu,cpuacct:/docker/abc\n4:memory:/docker/abc\n";
        let limit = cgroup_memory_limit(text, dir.path());
        assert_eq!(memory_capacity(Some(16 * GIB), limit), 16 * GIB);

        // And a real v1 limit is honoured.
        write(
            dir.path(),
            "memory/docker/abc/memory.limit_in_bytes",
            &format!("{}\n", 3 * GIB),
        );
        let limit = cgroup_memory_limit(text, dir.path());
        assert_eq!(memory_capacity(Some(16 * GIB), limit), 3 * GIB);
    }

    #[test]
    fn garbage_cgroup_file_means_physical() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a/memory.max", "lots\n");
        write(dir.path(), "memory.max", "0\n");
        for text in [
            "0::/a\n",
            "",
            "garbage",
            "0::relative/path\n",
            "0::/../../etc\n",
            "\u{0}\u{1}:\n",
            "0::/does/not/exist\n",
        ] {
            let limit = cgroup_memory_limit(text, dir.path());
            assert_eq!(limit, None, "{text:?}");
            assert_eq!(memory_capacity(Some(16 * GIB), limit), 16 * GIB, "{text:?}");
        }
        assert_eq!(memory_capacity(None, None), 0);
        assert_eq!(memory_capacity(None, Some(GIB)), GIB);
    }

    #[test]
    fn nested_cgroup_path_is_resolved() {
        let dir = tempfile::tempdir().unwrap();
        // The leaf's own limit is read from the nested path...
        write(dir.path(), "a/b/c/memory.max", &format!("{}\n", 4 * GIB));
        write(dir.path(), "a/b/memory.max", "max\n");
        let text = "0::/a/b/c\n";
        assert_eq!(cgroup_memory_limit(text, dir.path()), Some(4 * GIB));
        // ...and a tighter ancestor binds it.
        write(dir.path(), "a/memory.max", &format!("{}\n", GIB));
        assert_eq!(cgroup_memory_limit(text, dir.path()), Some(GIB));
        // A path the mount does not have (a container without a cgroup
        // namespace) still reaches the limit at the mount's top.
        write(dir.path(), "memory.max", &format!("{}\n", 512 << 20));
        assert_eq!(
            cgroup_memory_limit("0::/host/scope/deep\n", dir.path()),
            Some(512 << 20)
        );
    }
}
