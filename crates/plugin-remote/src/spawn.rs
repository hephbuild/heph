//! Spawn a plugin subprocess for the proto transport.
//!
//! The host creates a UDS socketpair, passes one end to the child on a fixed
//! inherited fd (3), and speaks the `Frame` protocol over it. The child reads
//! fd 3 (see `plugin_sdk::serve_inherited`). Stdio is left to the child for its
//! own logging.
//!
//! M1 returns the child handle so the caller manages its lifetime; integrating
//! `hproc::process_supervisor` reaping + the `launch` sandbox modes is M4.

use crate::RemoteProvider;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command};

/// The fd the plugin child inherits the protocol socket on.
pub const PLUGIN_FD: RawFd = 3;

/// Spawn `program` as a plugin and connect to it over proto. Returns the host
/// adapter plus the child handle (kill/wait it to control its lifetime).
pub fn spawn_plugin(
    program: &Path,
    args: &[String],
    name: impl Into<String>,
) -> anyhow::Result<(RemoteProvider, Child)> {
    let (parent, child_end) = std::os::unix::net::UnixStream::pair()?;
    parent.set_nonblocking(true)?;
    let child_fd = child_end.as_raw_fd();

    let mut cmd = Command::new(program);
    cmd.args(args);
    // Runs post-fork, pre-exec. dup2 clears CLOEXEC on the new fd so fd 3
    // survives exec, while the original (CLOEXEC) socketpair fd closes.
    let pre = move || -> std::io::Result<()> {
        // SAFETY: dup2 is async-signal-safe; child_fd is a valid inherited fd.
        let rc = unsafe { libc::dup2(child_fd, PLUGIN_FD) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    };
    // SAFETY: `pre` only calls the async-signal-safe dup2.
    unsafe {
        cmd.pre_exec(pre);
    }
    let child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn plugin {}: {e}", program.display()))?;
    // Parent no longer needs its copy of the child end.
    drop(child_end);

    let tokio_parent = tokio::net::UnixStream::from_std(parent)?;
    let (r, w) = tokio_parent.into_split();
    Ok((RemoteProvider::connect(r, w, name), child))
}
