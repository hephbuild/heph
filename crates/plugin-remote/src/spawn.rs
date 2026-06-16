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

/// The split tokio halves of a spawned plugin's protocol socket.
pub type PluginStreams = (
    tokio::net::unix::OwnedReadHalf,
    tokio::net::unix::OwnedWriteHalf,
);

/// Spawn `program` with `args` + `env`, passing the protocol socket on fd 3.
/// Returns the tokio half-streams + the child handle. Higher-level helpers
/// ([`spawn_plugin`], [`crate::RemotePlugin::spawn`]) build adapters on top.
pub fn spawn_streams(
    program: &Path,
    args: &[String],
    env: &[(String, String)],
) -> anyhow::Result<(PluginStreams, Child)> {
    let (parent, child_end) = std::os::unix::net::UnixStream::pair()?;
    parent.set_nonblocking(true)?;
    let child_fd = child_end.as_raw_fd();

    let mut cmd = Command::new(program);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
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
    Ok((tokio_parent.into_split(), child))
}

/// Env var carrying the iceoryx2 service-id base for the shm data transport.
/// When set, the plugin moves its protocol onto shared memory and keeps fd 3
/// (the UDS) only as a liveness channel (its EOF means the host went away).
#[cfg(feature = "shm")]
pub const PLUGIN_SHM_ENV: &str = "HEPH_PLUGIN_SHM";

/// Spawn `program` and connect over the **shm** transport: the protocol runs on
/// an iceoryx2 byte-pipe (no per-frame syscalls), while the inherited fd 3 (UDS)
/// is kept open purely as a bidirectional liveness signal — either side's EOF
/// (process exit/crash) closes the connection. Returns a [`RemotePlugin`].
#[cfg(feature = "shm")]
pub fn spawn_shm(
    program: &Path,
    args: &[String],
    env: &[(String, String)],
    shm_id: &str,
) -> anyhow::Result<crate::RemotePlugin> {
    use std::io::Read;
    use tokio::io::AsyncReadExt;

    let h2g = format!("{shm_id}_h2g");
    let g2h = format!("{shm_id}_g2h");
    // Host subscribes g2h before the guest ever publishes it (guest only
    // publishes in response to our requests), so this direction can't race.
    let (hr, hw) = plugin_abi::shm::connect(&h2g, &g2h)
        .map_err(|e| anyhow::anyhow!("host shm connect: {e}"))?;

    // fd 3 UDS: a one-byte readiness handshake (the guest signals once its shm
    // subscriber is up — iceoryx2 pub/sub drops messages sent before a subscriber
    // connects), then a bidirectional liveness channel (EOF = peer gone).
    let (parent, child_end) = std::os::unix::net::UnixStream::pair()?;
    let child_fd = child_end.as_raw_fd();

    let mut env = env.to_vec();
    env.push((PLUGIN_SHM_ENV.to_string(), shm_id.to_string()));
    let mut cmd = Command::new(program);
    cmd.args(args);
    for (k, v) in &env {
        cmd.env(k, v);
    }
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
    drop(child_end);

    // Block (briefly) until the guest's shm subscriber is live. Bounded so a
    // plugin that fails to start surfaces as an error (caller falls back to proto)
    // instead of hanging.
    let mut parent = parent;
    parent.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    let mut ready = [0u8; 1];
    parent
        .read_exact(&mut ready)
        .map_err(|e| anyhow::anyhow!("plugin shm readiness handshake: {e}"))?;

    parent.set_nonblocking(true)?;
    let tokio_parent = tokio::net::UnixStream::from_std(parent)?;
    let (mut uds_r, uds_w) = tokio_parent.into_split();

    let plugin = crate::RemotePlugin::connect(hr, hw);

    // Liveness watcher: hold the UDS write half + child alive; when the guest's
    // end closes (exit/crash) our read half hits EOF → close the connection so
    // pending calls fail instead of hanging on a dead peer.
    let mux = plugin.mux_handle();
    tokio::spawn(async move {
        let mut byte = [0u8; 1];
        let _ = uds_r.read(&mut byte).await; // resolves on EOF (no more data sent)
        mux.close();
        drop(uds_w);
        drop(child);
    });
    Ok(plugin)
}

/// Spawn `program` as a single-provider plugin and connect over proto. Returns
/// the host adapter plus the child handle (kill/wait it to control its lifetime).
pub fn spawn_plugin(
    program: &Path,
    args: &[String],
    name: impl Into<String>,
) -> anyhow::Result<(RemoteProvider, Child)> {
    let ((r, w), child) = spawn_streams(program, args, &[])?;
    Ok((RemoteProvider::connect(r, w, name), child))
}
