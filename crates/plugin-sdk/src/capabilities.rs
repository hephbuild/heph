//! What the loading host understands, as it told this plugin at create.
//!
//! Per-plugin state: a cdylib links its own copy of the SDK, so this static is
//! this plugin's alone. It is written once, by [`create_config_from_bytes`] in
//! the plugin's `heph_plugin_create`, before the host makes any other call, and
//! read on every `parse`. Until it is written, every capability is "no" — the
//! same answer an older host gives by never sending the bits.

use plugin_abi::convert::{self, CreateConfig, HostCapabilities};
use std::sync::atomic::{AtomicBool, Ordering};

/// [`HostCapabilities::entrypoints`] as the host reported it.
static HOST_ENTRYPOINTS: AtomicBool = AtomicBool::new(false);

/// Decode the config the host hands `heph_plugin_create`, and record its host
/// capabilities for this plugin. `root`, `home` and `name` are required; the
/// `options` map is the plugin's `options:`, which authors read via
/// [`hplugin::config::decode_opt`], exactly as in-process plugins do. The wire
/// form stays inside the SDK.
pub fn create_config_from_bytes(bytes: &[u8]) -> anyhow::Result<CreateConfig> {
    let cfg = convert::create_config_from_bytes(bytes)?;
    record(cfg.host_capabilities);
    Ok(cfg)
}

fn record(caps: HostCapabilities) {
    // Release pairs with the Acquire in `host_capabilities`: the host may run
    // the first `parse` on another thread than the one that ran create.
    HOST_ENTRYPOINTS.store(caps.entrypoints, Ordering::Release);
}

/// The capabilities the host reported at create; none before create.
pub fn host_capabilities() -> HostCapabilities {
    HostCapabilities::default().with_entrypoints(HOST_ENTRYPOINTS.load(Ordering::Acquire))
}

/// Refuse to declare an entrypoint on a host that does not honour one.
///
/// A host before 0.17.1 drops `TargetDef.entrypoint` on the floor: it would
/// build the target in a sandbox and cache it, which is exactly what an
/// entrypoint must never be. The SDK runs this on every def a driver's `parse`
/// returns, so a driver need not call it; a driver may call it earlier, when it
/// reads the option, to fail before doing the rest of its parse.
pub fn require_entrypoint_support(addr: &hmodel::htaddr::Addr) -> anyhow::Result<()> {
    check(host_capabilities(), addr)
}

pub(crate) fn check(caps: HostCapabilities, addr: &hmodel::htaddr::Addr) -> anyhow::Result<()> {
    if !caps.entrypoints {
        anyhow::bail!(
            "{} declares an entrypoint, but the heph loading this plugin predates entrypoints \
             (plugin ABI {}); upgrade heph to run it, or drop `entrypoint`",
            addr.format(),
            plugin_abi::ABI_SEMVER,
        );
    }
    Ok(())
}

/// The def's entrypoint, refused unless the host advertised support. Run on
/// every def a guest driver's `parse` or `apply_transitive` returns.
pub(crate) fn check_def(
    caps: HostCapabilities,
    def: &hplugin::driver::targetdef::TargetDef,
) -> anyhow::Result<()> {
    if def.entrypoint.is_some() {
        check(caps, &def.addr)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hplugin::driver::targetdef::{Entrypoint, TargetDef, Workdir};

    fn def(entrypoint: bool) -> TargetDef {
        TargetDef {
            addr: hmodel::htaddr::parse_addr("//tools:migrate").expect("addr"),
            entrypoint: entrypoint.then(|| Entrypoint::new(true, Workdir::Caller)),
            ..Default::default()
        }
    }

    /// R3: without the host's bit, declaring an entrypoint is a parse error
    /// naming the target, not a target the old host would build and cache.
    #[test]
    fn plugin_refuses_entrypoint_on_old_host() {
        let old = HostCapabilities::default();
        let err = check_def(old, &def(true)).expect_err("refused on an old host");
        assert!(
            format!("{err:#}").contains("//tools:migrate declares an entrypoint"),
            "{err:#}"
        );
        // An ordinary def is untouched on any host.
        check_def(old, &def(false)).expect("a build is fine on an old host");
        // On a host with the bit, the entrypoint goes through.
        check_def(HostCapabilities::current(), &def(true)).expect("new host");
    }

    /// The bit is read from the create config, and nothing is assumed before it.
    #[test]
    fn create_records_the_host_capabilities() {
        use buffa::Message as _;
        let new_host = CreateConfig::new("/ws", "/ws/.heph", "p", Default::default());
        let old_host = new_host
            .clone()
            .with_host_capabilities(HostCapabilities::default());
        for (cfg, want) in [(&old_host, false), (&new_host, true)] {
            let bytes = convert::create_config_to_pb(cfg).encode_to_vec();
            create_config_from_bytes(&bytes).expect("decode");
            assert_eq!(host_capabilities().entrypoints, want);
        }
    }
}
