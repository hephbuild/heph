#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! `credentials` on the in-process registry drivers, through the real engine.
//!
//! What is under test is the wiring the unit tests in `pluginoci::auth` cannot
//! see: that a `credentials` reference on `oci.pull` resolves, is acquired by the
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
        .with_managed_driver(Box::new(heph::pluginexec::Driver::new_bash()))
        .with_plugin(pluginoci::PLUGIN_NAME, |_| {
            Ok(heph::engine::PluginParts::default()
                .with_provider(Box::new(pluginoci::platform::Provider))
                .with_managed_driver(Box::new(pluginoci::layer::Driver::new()))
                .with_managed_driver(Box::new(pluginoci::image::Driver::new()))
                .with_managed_driver(Box::new(pluginoci::push::Driver::new()))
                .with_managed_driver(Box::new(pluginoci::pull::Driver::new())))
        })
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
        r#"target(name = "ghcr", driver = "auth.credential",
       sources = [heph.auth.env(["HEPH_E2E_OCI_PULL_TOKEN"])],
       present = heph.auth.docker(["ghcr.io"]))"#,
    );
    ws.write_build_file(
        "img",
        &format!(
            r#"target(name = "app", driver = "oci.pull", ref = "{PINNED}",
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

/// The same for a push, which is the case that matters more: a push that fell
/// back to the host's own login would publish as the developer. The image is
/// built offline so the only thing standing between it and the network is the
/// credential check.
#[tokio::test]
async fn a_push_whose_credentials_do_not_cover_the_registry_fails_before_the_network()
-> anyhow::Result<()> {
    let _guard = EnvVar::set("HEPH_E2E_OCI_PUSH_TOKEN", "oci-push-material");
    let ws = workspace();
    ws.write_build_file(
        "auth",
        r#"target(name = "ghcr", driver = "auth.credential",
       sources = [heph.auth.env(["HEPH_E2E_OCI_PUSH_TOKEN"])],
       present = heph.auth.docker(["ghcr.io"]))"#,
    );
    ws.write_build_file(
        "img",
        r#"
target(name = "conf", driver = "bash", run = "echo k=v > $OUT", out = "app.conf")
target(name = "etc", driver = "oci.layer", srcs = [":conf"], prefix = "/etc")
target(name = "img", driver = "oci.image", layers = [":etc"], platforms = ["linux/amd64"])
target(name = "push", driver = "oci.push", image = ":img", ref = "quay.io/acme/app:1",
       credentials = ["//auth:ghcr"])
"#,
    );
    let err = match ws.run("//img:push").await {
        Ok(_) => panic!("an uncovered registry must fail"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("no declared credential covers"), "{err}");
    assert!(err.contains("quay.io"), "{err}");
    Ok(())
}

/// `ref` reaches the push resolved: the repository from a `${read://…}`
/// producer, the tag from `${image_hashout}`. Docker-free, through the same
/// offline image as above — the uncovered credential stops it before the
/// network, and the error names the reference it would have pushed.
#[tokio::test]
async fn a_push_resolves_its_ref_before_the_network() -> anyhow::Result<()> {
    let _guard = EnvVar::set("HEPH_E2E_OCI_PUSH_REF_TOKEN", "oci-push-material");
    let ws = workspace();
    ws.write_build_file(
        "auth",
        r#"target(name = "ghcr", driver = "auth.credential",
       sources = [heph.auth.env(["HEPH_E2E_OCI_PUSH_REF_TOKEN"])],
       present = heph.auth.docker(["ghcr.io"]))"#,
    );
    ws.write_build_file(
        "img",
        r#"
target(name = "conf", driver = "bash", run = "echo k=v > $OUT", out = "app.conf")
target(name = "etc", driver = "oci.layer", srcs = [":conf"], prefix = "/etc")
target(name = "img", driver = "oci.image", layers = [":etc"], platforms = ["linux/amd64"])
target(name = "repo", driver = "bash", run = "echo quay.io/acme/app > $OUT", out = "repo.txt")
target(name = "push", driver = "oci.push", image = ":img",
       ref = "${read://img:repo}:${image_hashout}", credentials = ["//auth:ghcr"])
"#,
    );
    let err = match ws.run("//img:push").await {
        Ok(_) => panic!("an uncovered registry must fail"),
        Err(e) => format!("{e:#}"),
    };
    let img = ws.run_addr_outputs("//img:img", &[""]).await?;
    let [archive] = img.artifacts.as_slice() else {
        panic!("the image has one archive artifact");
    };
    let hashout = archive.hashout()?;
    assert!(!hashout.is_empty(), "the archive must carry a hashout");
    assert!(
        err.contains(&format!("push quay.io/acme/app:{hashout} ")),
        "the error must name the resolved reference: {err}"
    );
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
            r#"target(name = "app", driver = "oci.pull", ref = "{PINNED}",
       credentials = ["//auth:nope"], cache = False)"#
        ),
    );
    let err = match ws.run("//img:app").await {
        Ok(_) => panic!("a missing credential must fail"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("target not found: //auth:nope"), "{err}");
    Ok(())
}
