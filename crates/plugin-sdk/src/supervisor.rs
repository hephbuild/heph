//! Guest side of process-supervisor forwarding: point this cdylib's copy of the
//! `proc` crate at the host's supervisor client.
//!
//! A loaded cdylib statically links its OWN `proc`, whose `TRACKER` static the
//! host's `process_supervisor::init` never touched. So a plugin that spawns
//! children (plugin-go runs a `go` compile per package) would register none of
//! them with the sidecar: they'd be orphaned if the host is hard-killed, and each
//! spawn logged `child not registered with process supervisor`. The host hands the
//! plugin a [`DynSupervisor`] (via the `heph_plugin_set_supervisor` symbol);
//! [`install_supervisor`] installs a tracker that forwards every `TRACK`/`UNTRACK`
//! across the seam to the host's socket-owning tracker.

use hplugin_stabby::abi::{DynSupervisor, StableSupervisorDyn};
use hproc::process_supervisor::SupervisorSink;
use stabby::string::String as SString;

/// Forwards tracker calls to the host across the stable ABI. The host encodes an
/// error as a non-empty string (empty = success); it is re-inflated here so the
/// plugin-side warning carries the host's real context.
struct HostSink {
    sup: DynSupervisor,
}

impl std::fmt::Debug for HostSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostSink")
    }
}

/// Turn an ABI reply into a `Result`: empty string is success.
fn decode(reply: SString) -> anyhow::Result<()> {
    let msg = reply.to_string();
    if msg.is_empty() {
        return Ok(());
    }
    Err(anyhow::anyhow!(msg))
}

impl SupervisorSink for HostSink {
    fn track(&self, pgid: i32) -> anyhow::Result<()> {
        decode(self.sup.track(pgid))
    }

    fn untrack(&self, pgid: i32) -> anyhow::Result<()> {
        decode(self.sup.untrack(pgid))
    }

    fn register_fuse_root(&self, root: &std::path::Path) -> anyhow::Result<()> {
        decode(
            self.sup
                .register_fuse_root(SString::from(root.to_string_lossy().as_ref())),
        )
    }
}

/// Wrap the host's supervisor handle as a [`SupervisorSink`]. Exposed (rather than
/// only used by [`install_supervisor`]) so the forwarding can be exercised without
/// touching the crate-global tracker, which is set once per process.
pub fn supervisor_sink(sup: DynSupervisor) -> Box<dyn SupervisorSink> {
    Box::new(HostSink { sup })
}

/// Install the host's supervisor as this plugin's process tracker, so every child
/// the plugin spawns is registered with the host's sidecar. Idempotent: only the
/// first call wins.
pub fn install_supervisor(sup: DynSupervisor) {
    hproc::process_supervisor::init_with_sink(supervisor_sink(sup));
}

#[cfg(test)]
mod tests {
    use super::*;
    use hplugin_stabby::abi::StableSupervisor;
    use std::sync::Mutex;

    /// Stands in for the host: records the calls that crossed the seam, and can be
    /// told to answer with an error (the host's tracker failing).
    struct RecordingSupervisor {
        calls: &'static Mutex<Vec<String>>,
        error: Option<&'static str>,
    }

    impl RecordingSupervisor {
        fn reply(&self) -> SString {
            match self.error {
                None => SString::new(),
                Some(e) => SString::from(e),
            }
        }
    }

    impl StableSupervisor for RecordingSupervisor {
        extern "C" fn track(&self, pgid: i32) -> SString {
            self.calls
                .lock()
                .expect("lock")
                .push(format!("track {pgid}"));
            self.reply()
        }

        extern "C" fn untrack(&self, pgid: i32) -> SString {
            self.calls
                .lock()
                .expect("lock")
                .push(format!("untrack {pgid}"));
            self.reply()
        }

        extern "C" fn register_fuse_root(&self, root: SString) -> SString {
            self.calls
                .lock()
                .expect("lock")
                .push(format!("fuse {root}"));
            self.reply()
        }
    }

    fn wrap(sup: RecordingSupervisor) -> DynSupervisor {
        hplugin_stabby::vtable::dynify(stabby::boxed::Box::new(sup))
    }

    #[test]
    fn sink_forwards_calls_to_host() {
        static CALLS: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let sink = supervisor_sink(wrap(RecordingSupervisor {
            calls: &CALLS,
            error: None,
        }));

        sink.track(4242).expect("track");
        sink.untrack(4242).expect("untrack");
        sink.register_fuse_root(std::path::Path::new("/tmp/sandboxfuse-root"))
            .expect("register_fuse_root");

        assert_eq!(
            *CALLS.lock().expect("lock"),
            vec![
                "track 4242".to_owned(),
                "untrack 4242".to_owned(),
                "fuse /tmp/sandboxfuse-root".to_owned(),
            ]
        );
    }

    #[test]
    fn sink_surfaces_host_error() {
        static CALLS: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let sink = supervisor_sink(wrap(RecordingSupervisor {
            calls: &CALLS,
            error: Some("process supervisor unavailable"),
        }));

        let err = sink.track(7).expect_err("host error must surface");
        assert_eq!(err.to_string(), "process supervisor unavailable");
    }
}
