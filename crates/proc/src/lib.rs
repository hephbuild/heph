//! Process execution + lifecycle: spawn/reap subprocess groups (`proc_exec`),
//! the out-of-process supervisor sidecar that reaps orphaned groups
//! (`process_supervisor`), and the macOS kqueue-based exit watcher
//! (`process_watcher`). Depends only on `heph-core` (cancellation).

pub mod proc_exec;
pub mod process_supervisor;
#[cfg(target_os = "macos")]
pub mod process_watcher;
