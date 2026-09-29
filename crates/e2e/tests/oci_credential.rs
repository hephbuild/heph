#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! `credentials` on the in-process registry drivers, through the real engine.
//!
//! What is under test is the wiring the unit tests in `pluginoci::auth` cannot
//! see: that a `credentials` reference on `oci_pull` resolves, is acquired by the
//! host and arrives at the driver as a mount — and that once a target has named
//! credentials, the driver authenticates with those or not at all.
//!
//! The helper round trip itself is covered by those unit tests. Here the shim
//! would call back into `current_exe()`, which in an in-process test is this
//! test binary rather than heph, so the assertions stop at what the driver
//! decides before it would call it.

mod common;

use hplugin_oci::pluginoci;
use htestkit::WorkspaceBuilder;

fn workspace() -> htestkit::Workspace {
    WorkspaceBuilder::new()
        .expect("workspace tempdir")
        .with_provider(|init| {
            Box::new(heph::pluginbuildfile::Provider::new(
                init.root.to_path_buf(),
                init.runtime.clone(),
            ))
        })
        .with_provider(|_| Box::new(pluginoci::platform::Provider))
        .with_managed_driver(Box::new(pluginoci::push::Driver::new()))
        .with_managed_driver(Box::new(pluginoci::pull::Driver::new()))
        .build()
        .expect("build workspace")
}

/// Set a host variable for the duration of one test: an `env` source reads the
/// process environment. Restored on drop.
struct EnvVar(&'static str);

impl EnvVar {
    fn set(name: &'static str, value: &str) -> Self {
        // SAFETY: each test uses its own variable, and the guard restores it.
        unsafe { std::env::set_var(name, value) };
        Self(name)
    }
}

impl Drop for EnvVar {
    fn drop(&mut self) {
        // SAFETY: see `set`.
        unsafe { std::env::remove_var(self.0) };
    }
}

const PINNED: &str =
    "quay.io/acme/app@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A pull naming a credential that covers another registry fails before any
/// request is made, naming the registry — rather than going to `quay.io`
/// anonymously, or as whoever the host's own `docker login` happens to be.
///
/// Reaching that error at all is the proof the mount arrived: without one, the
/// driver would take the ambient path and attempt the network.
#[tokio::test]
async fn a_pull_whose_credentials_do_not_cover_the_registry_fails_before_the_network()
-> anyhow::Result<()> {
    let _guard = EnvVar::set("HEPH_E2E_OCI_PULL_TOKEN", "oci-pull-material");
    let ws = workspace();
    ws.write_build_file(
        "auth",
        r#"target(name = "ghcr", driver = "credential",
       sources = [heph.auth.env(["HEPH_E2E_OCI_PULL_TOKEN"])],
       present = heph.auth.docker(["ghcr.io"]))"#,
    );
    ws.write_build_file(
        "img",
        &format!(
            r#"target(name = "app", driver = "oci_pull", ref = "{PINNED}",
       credentials = ["//auth:ghcr"], cache = False)"#
        ),
    );
    let err = match ws.run("//img:app").await {
        Ok(_) => panic!("an uncovered registry must fail"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("no declared credential covers"), "{err}");
    assert!(err.contains("quay.io"), "{err}");
    Ok(())
}

/// A reference to something that is not a target at all is an ordinary
/// resolution error at the consumer — not a 401 from the registry much later.
#[tokio::test]
async fn a_missing_credential_is_a_graph_error() -> anyhow::Result<()> {
    let ws = workspace();
    ws.write_build_file(
        "img",
        &format!(
            r#"target(name = "app", driver = "oci_pull", ref = "{PINNED}",
       credentials = ["//auth:nope"], cache = False)"#
        ),
    );
    let err = match ws.run("//img:app").await {
        Ok(_) => panic!("a missing credential must fail"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("//auth:nope"), "{err}");
    Ok(())
}
