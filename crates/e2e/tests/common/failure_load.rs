//! A failure load (I8): many targets in one package, some of them failing
//! with a large log, some failures under deep chains of dependents. What a
//! run holds per failure must not grow with the log or the chain, nor keep a
//! file open: `failure_fd_count_flat`, `failure_memory_slope_bounded` and the
//! `failure_load_20k` instrument read the process around one of these.

use heph::engine::{Engine, OutputMatcher, ResultOptions};
use heph::htmatcher::Matcher;
use heph::htpkg::PkgBuf;
use hplugin::error::TargetFailure;
use std::sync::Arc;

/// The package every load target is in.
pub const PKG: &str = "load";

/// The shape of a load.
#[derive(Debug, Clone, Copy)]
pub struct FailureLoad {
    /// Every target in the package: the failing leaves, the chains above the
    /// first `chains` of them, and passing targets for the rest.
    pub targets: usize,
    /// The leaves that fail, each one culprit.
    pub failures: usize,
    /// What each leaf prints before it exits, in bytes (whole 64 KiB blocks
    /// of 64-byte lines; at least one block).
    pub log_bytes: usize,
    /// How many failing leaves have a chain of dependents above them.
    pub chains: usize,
    /// How deep each chain is.
    pub depth: usize,
    /// Whether the leaves fail; `false` is the green baseline of the same
    /// shape: the same runs, the same logs, every one passing.
    pub fail: bool,
}

/// Bytes of one block a leaf prints: 1024 lines of 64 bytes.
const BLOCK: usize = 64 * 1024;

impl FailureLoad {
    /// The package's BUILD file.
    pub fn build_file(&self) -> String {
        let on_chains = self.chains * self.depth;
        assert!(
            self.failures + on_chains <= self.targets,
            "more failing and chained targets than targets: {self:?}"
        );
        let passing = self.targets - self.failures - on_chains;
        assert!(
            self.chains <= self.failures,
            "a chain sits on a failing leaf: {self:?}"
        );
        let blocks = (self.log_bytes / BLOCK).max(1);
        let end = if self.fail {
            "exit 1"
        } else {
            "echo ok > o.txt"
        };
        // No `seq` in a target's sandbox: bash's own loops and printf.
        let run = format!(
            r#"l=$(printf '%063d' 0); b=$(for ((i = 0; i < 1024; i++)); do printf '%s\n' "$l"; done); for ((i = 0; i < {blocks}; i++)); do printf '%s\n' "$b"; done; {end}"#
        );
        let (failures, chains, depth) = (self.failures, self.chains, self.depth);
        format!(
            r#"
RUN = '''{run}'''
[target(name = "f%d" % i, driver = "bash", out = "o.txt", cache = False, run = RUN) for i in range({failures})]
[
    target(
        name = "c%d_%d" % (c, j),
        driver = "bash",
        out = "o.txt",
        cache = False,
        run = "echo ok > o.txt",
        deps = ["//{PKG}:f%d" % c if j == 0 else "//{PKG}:c%d_%d" % (c, j - 1)],
    )
    for c in range({chains})
    for j in range({depth})
]
[target(name = "ok%d" % i, driver = "bash", out = "o.txt", cache = False, run = "echo ok > o.txt") for i in range({passing})]
"#
        )
    }

    /// A fresh workspace holding the package.
    pub fn workspace(&self) -> super::Workspace {
        let ws = super::Workspace::new();
        ws.write_build_file(PKG, &self.build_file());
        ws
    }

    /// Run every target of the package on one request, without
    /// `--fail-fast` so every leaf runs and fails, and without events (what
    /// a run holds is the subject, not an event buffer). Returns the
    /// request, still holding what it recorded, and its failures.
    pub async fn run(
        &self,
        engine: &Arc<Engine>,
    ) -> (
        Arc<heph::engine::request_state::RequestState>,
        Vec<Arc<TargetFailure>>,
    ) {
        let rs = engine.new_state_with_fail_fast(false);
        let res = Arc::clone(engine)
            .result(
                rs.clone(),
                &Matcher::Package(PkgBuf::from(PKG)),
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await;
        let failures = rs.take_failures();
        if self.fail {
            assert!(
                res.is_err() || res.as_ref().is_ok_and(|b| !b.errors.is_empty()),
                "the load fails"
            );
            assert_eq!(
                failures.len(),
                self.failures,
                "one culprit per failing leaf"
            );
        } else {
            let batch = res.expect("the green load builds");
            assert!(batch.errors.is_empty(), "{:?}", batch.errors);
        }
        (rs, failures)
    }
}

/// The open file descriptors of this process, counted from `/dev/fd` — the
/// same on Linux (`/proc/self/fd`) and macOS (fdesc). The directory's own
/// fd is counted in every reading alike.
pub fn open_fds() -> usize {
    std::fs::read_dir("/dev/fd").expect("list /dev/fd").count()
}

/// The local cache directory of a load's workspace, as an fd's path names it
/// (canonical: macOS's `/var` is `/private/var`).
pub fn cache_dir(ws: &super::Workspace) -> std::path::PathBuf {
    std::fs::canonicalize(ws.dir.path())
        .expect("canonicalize the workspace")
        .join(".heph")
        .join("cache")
}

/// What each open fd of this process is: its path, or its kind when it has
/// none (a pipe, a socket, a kqueue). Sorted, for a diff of two readings.
pub fn open_fd_paths() -> Vec<String> {
    let mut paths: Vec<String> = std::fs::read_dir("/dev/fd")
        .expect("list /dev/fd")
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .filter_map(fd_path)
        .collect();
    paths.sort();
    paths
}

/// The open fds whose path is not under `dir`, each with `root` (the
/// workspace) shown as `<ws>` so two workspaces' readings diff.
pub fn open_fd_paths_outside(dir: &std::path::Path, root: &std::path::Path) -> Vec<String> {
    let (dir, root) = (dir.to_string_lossy(), root.to_string_lossy());
    let mut paths: Vec<String> = open_fd_paths()
        .into_iter()
        .filter(|p| !p.starts_with(dir.as_ref()))
        .map(|p| p.replacen(root.as_ref(), "<ws>", 1))
        .collect();
    paths.sort();
    paths
}

/// `None` when `fd` closed since it was listed (the listing's own fd).
fn fd_path(fd: i32) -> Option<String> {
    #[cfg(target_os = "macos")]
    let path = {
        let mut buf = vec![0u8; usize::try_from(libc::PATH_MAX).unwrap_or(1024)];
        // SAFETY: F_GETPATH writes at most PATH_MAX bytes, NUL-terminated,
        // into `buf`, which holds PATH_MAX.
        let rc = unsafe { libc::fcntl(fd, libc::F_GETPATH, buf.as_mut_ptr()) };
        (rc != -1).then(|| {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            String::from_utf8_lossy(buf.get(..end).unwrap_or_default()).into_owned()
        })
    };
    #[cfg(not(target_os = "macos"))]
    let path = std::fs::read_link(format!("/proc/self/fd/{fd}"))
        .ok()
        .map(|p| p.display().to_string());
    if let Some(path) = path {
        return Some(path);
    }
    let mut st = std::mem::MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: `st` is a valid, writable `stat`.
    if unsafe { libc::fstat(fd, st.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: fstat returned 0, so it filled `st` (zeroed before).
    let kind = match unsafe { st.assume_init() }.st_mode & libc::S_IFMT {
        libc::S_IFIFO => "<pipe>",
        libc::S_IFSOCK => "<socket>",
        libc::S_IFCHR => "<char device>",
        libc::S_IFREG => "<file>",
        libc::S_IFDIR => "<dir>",
        _ => "<other>",
    };
    Some(kind.to_owned())
}

/// The entries of `after` with no counterpart in `before`, as a multiset.
pub fn fd_paths_added(before: &[String], after: &[String]) -> Vec<String> {
    let mut left: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for p in before {
        *left.entry(p).or_default() += 1;
    }
    after
        .iter()
        .filter(|p| match left.get_mut(p.as_str()) {
            Some(n) if *n > 0 => {
                *n -= 1;
                false
            }
            _ => true,
        })
        .cloned()
        .collect()
}

/// This process's peak resident set, in bytes: `ru_maxrss` is KiB on Linux
/// and bytes on macOS.
pub fn peak_rss_bytes() -> u64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` is a valid, writable `rusage`; getrusage fills it.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    assert_eq!(rc, 0, "getrusage: {}", std::io::Error::last_os_error());
    // SAFETY: getrusage returned 0, so it wrote the whole struct (and it
    // was zeroed before).
    let max = unsafe { usage.assume_init() }.ru_maxrss;
    let max = u64::try_from(max).unwrap_or(0);
    if cfg!(target_os = "macos") {
        max
    } else {
        max * 1024
    }
}

/// Samples [`open_fds`] on its own thread every few milliseconds until
/// dropped through [`FdSampler::max`].
pub struct FdSampler {
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: std::thread::JoinHandle<usize>,
}

impl FdSampler {
    pub fn start() -> Self {
        Self::sampling(open_fds)
    }

    /// Counts only the fds whose path is not under `dir`.
    pub fn start_outside(dir: std::path::PathBuf) -> Self {
        let dir = dir.to_string_lossy().into_owned();
        Self::sampling(move || {
            open_fd_paths()
                .iter()
                .filter(|p| !p.starts_with(&dir))
                .count()
        })
    }

    fn sampling(count: impl Fn() -> usize + Send + 'static) -> Self {
        use std::sync::atomic::Ordering;
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let mut max = count();
            while !flag.load(Ordering::Acquire) {
                max = max.max(count());
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            max.max(count())
        });
        Self { stop, thread }
    }

    /// The most fds open at any sample.
    pub fn max(self) -> usize {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        self.thread.join().expect("fd sampler")
    }
}
