//! Off-tokio subprocess pipeline.
//!
//! On macOS, every step of the subprocess lifecycle (spawn, stdin pump,
//! stdout/stderr drain, wait, kill) runs on `std::process` + `std::thread` +
//! `std::sync::mpsc`. The only point that touches the tokio runtime is the
//! final boundary, where the calling task synchronously parks via
//! `block_in_place(|| std_rx.recv())` on a kernel condvar. This bypasses
//! tokio's cross-thread waker (`mio::Waker` → `EVFILT_USER`), which is
//! observed to silently drop wake-ups on macOS under heavy concurrent load.
//! See `RCA_MACOS_WAKER.md`.
//!
//! On Linux, the entire bug class is absent (`epoll` + `pidfd`, no
//! `EVFILT_USER`), so we use `tokio::process` directly with no workarounds.
//!
//! Public surface:
//! - [`Spec`] — declarative description of a child to spawn.
//! - [`output`] — batch: spawn, capture stdout/stderr, wait, return `Output`.
//! - [`spawn`] — low-level: returns a [`Handle`] for streaming + stdin pump.

use heph_core::hasync::Cancellable;
use std::ffi::OsString;
use std::io;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::process::{Output, Stdio};

#[cfg(target_os = "macos")]
mod imp_macos;
#[cfg(target_os = "macos")]
use imp_macos as imp;

#[cfg(not(target_os = "macos"))]
mod imp_linux;
#[cfg(not(target_os = "macos"))]
use imp_linux as imp;

/// Standard chunk size for pipe drains in streaming mode. Matches the
/// previous `tee_stream` buffer size for byte-for-byte compatibility.
pub const CHUNK_SIZE: usize = 8192;

/// Grace window granted to a child after a cancellation `SIGINT` before we
/// escalate to `SIGKILL`. Mirrors a terminal Ctrl-C: well-behaved children
/// (and their descendants) get a chance to unwind and exit cleanly; anything
/// still alive after this is hard-killed so the runtime can't be parked
/// waiting on a child that ignores the interrupt.
pub const CANCEL_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Stdio configuration variants supported by [`Spec`]. Mirrors
/// `std::process::Stdio` but is `Clone` so a `Spec` can be inspected /
/// retried without consuming inherited fds.
pub enum StdioSpec {
    Null,
    Inherit,
    Piped,
    /// Take ownership of an existing fd (used for PTY slave inheritance).
    Fd(OwnedFd),
}

impl StdioSpec {
    fn into_stdio(self) -> Stdio {
        match self {
            StdioSpec::Null => Stdio::null(),
            StdioSpec::Inherit => Stdio::inherit(),
            StdioSpec::Piped => Stdio::piped(),
            StdioSpec::Fd(fd) => Stdio::from(fd),
        }
    }
}

/// Declarative spec for spawning a child process.
pub struct Spec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Cleared environment (`env_clear`) populated from this list. The driver
    /// is responsible for selecting which host env vars to pass through.
    pub env: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
    pub stdin: StdioSpec,
    pub stdout: StdioSpec,
    pub stderr: StdioSpec,
    /// If true, `pre_exec` calls `setsid()` so the child becomes session
    /// leader (pgid == pid). Required for PTY ctty assignment and for the
    /// supervisor's `killpg` to reap the whole tree.
    pub setsid: bool,
    /// If true, `pre_exec` calls `ioctl(0, TIOCSCTTY, 0)` to make the child's
    /// controlling terminal point at the inherited stdin fd. Only meaningful
    /// when `stdin` was set to a PTY slave fd.
    pub ctty: bool,
}

/// Batch run: spawn, capture stdout/stderr to `Vec<u8>`, wait, return.
///
/// `cancel` aborts the wait by sending `SIGKILL` to the child (and its pgid
/// if `spec.setsid` is set). The function still waits for the kernel to
/// confirm the exit before returning the cancel error.
pub async fn output(spec: Spec, cancel: &(dyn Cancellable + Send + Sync)) -> io::Result<Output> {
    imp::output(spec, cancel).await
}

/// Low-level spawn returning a [`Handle`] with per-stream chunked readers
/// and an optional stdin pump. Used for streaming output (pluginexec) where
/// the caller wants chunks delivered to a TUI as the child writes them.
pub fn spawn(spec: Spec) -> io::Result<Handle> {
    imp::spawn(spec)
}

pub use imp::{ChunkReader, Handle, StdinPump};
