#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! The `oci_runner` driver against a **real** docker daemon.
//!
//! What only a real daemon can answer: that `docker image inspect --format
//! {{.Id}}` returns what the driver parses, and — the point of the whole driver
//! — that the fingerprint is the image's *content* digest rather than the tag it
//! was asked about. A tag is a moving pointer; if the fingerprint tracked the
//! tag, retagging would leave every consumer serving artifacts built in an
//! image that no longer exists under that name.
//!
//! Skips (does not fail) when there is no usable daemon. Note the existing
//! `oci_docker.rs` gates on `docker buildx`, which passes on a host whose daemon
//! is down; this gate asks the daemon a question instead.
//!
//! # Running a target in the container
//!
//! This used to be impossible to test. The plugin emitted a `session` config,
//! which works by launching `heph __runner-agent` *inside* the environment — so
//! the container needed heph's own binary, and on macOS, where much of this is
//! developed, that binary is Darwin while the image is Linux. The suite carried
//! an honest gap instead.
//!
//! Now the plugin implements the runner itself and enters the container with
//! `docker exec`, which needs nothing of heph inside the image. So the test
//! that could never pass is here, and it runs anywhere a daemon does.

mod common;

use hplugin_oci::pluginoci;
use htestkit::WorkspaceBuilder;
use std::process::Command;
use std::sync::OnceLock;

/// The image every fixture points at. Built here from `FROM scratch`, so
/// nothing is ever pulled and the suite stays off the network.
const TEST_IMAGE: &str = "heph-e2e-oci-runner:v1";
const OTHER_IMAGE: &str = "heph-e2e-oci-runner:v2";
/// An image with a shell and a file that exists nowhere else, so "did this run
/// inside?" has an unambiguous answer.
const MARKER_IMAGE: &str = "heph-e2e-oci-runner:marker";

/// A daemon that answers, not merely a `docker` on PATH.
///
/// `docker version --format {{.Server.Version}}` is the direct question: the
/// client always answers, the *server* field only fills in when a daemon is
/// reachable. Deliberately not a string match on an error message — the first
/// version of this gate looked for "cannot connect", OrbStack says "if the
/// daemon is running", and the gate passed on a host with no daemon at all.
/// Every test then ran and one of them passed for the wrong reason.
///
/// Deliberately not `docker info` either: it walks every configured builder, so
/// one stale context makes it hang — the reason the sibling suite avoids it.
fn docker_ready() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        let Ok(out) = Command::new("docker")
            .args(["version", "--format", "{{.Server.Version}}"])
            .output()
        else {
            return false;
        };
        out.status.success() && !String::from_utf8_lossy(&out.stdout).trim().is_empty()
    })
}

macro_rules! skip_unless_docker {
    () => {
        if !docker_ready() {
            eprintln!("skipping: no docker daemon is answering on this host");
            return Ok(());
        }
    };
}

/// Build a trivial image under `tag`, with `marker` as a label so two tags can
/// be given genuinely different content.
fn build_image(tag: &str, marker: &str) -> bool {
    let Ok(dir) = tempfile::tempdir() else {
        return false;
    };
    if std::fs::write(
        dir.path().join("Dockerfile"),
        format!("FROM scratch\nLABEL heph.e2e={marker}\n"),
    )
    .is_err()
    {
        return false;
    }
    Command::new("docker")
        .args(["build", "-t", tag, "."])
        .current_dir(dir.path())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Build the marker image: a real base (the runner needs a shell to hold the
/// container open) plus a file that only exists inside it.
fn build_marker_image() -> bool {
    let Ok(dir) = tempfile::tempdir() else {
        return false;
    };
    if std::fs::write(
        dir.path().join("Dockerfile"),
        "FROM debian:stable-slim\nRUN echo in-the-container > /etc/heph-e2e-marker\n",
    )
    .is_err()
    {
        return false;
    }
    Command::new("docker")
        .args(["build", "-t", MARKER_IMAGE, "."])
        .current_dir(dir.path())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The digest docker itself reports, to compare the driver's answer against.
fn image_id(tag: &str) -> Option<String> {
    let out = Command::new("docker")
        .args(["image", "inspect", "--format", "{{.Id}}", tag])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn workspace() -> htestkit::Workspace {
    WorkspaceBuilder::new()
        .expect("workspace tempdir")
        .with_provider(|init| {
            Box::new(heph::pluginbuildfile::Provider::new(
                init.root.to_path_buf(),
                init.runtime.clone(),
            ))
        })
        .with_managed_driver(Box::new(
            heph::pluginexec::Driver::new_bash().with_host_path(),
        ))
        .with_managed_driver(Box::new(pluginoci::runner::Driver::new()))
        // What the cdylib hands the host through `NamedRunner`; an in-process
        // harness registers it directly.
        .with_exec_runner(std::sync::Arc::new(pluginoci::exec_runner::OciRunner::new()))
        .build()
        .expect("build workspace")
}

fn write_runner(ws: &htestkit::Workspace, image: &str) {
    ws.write_build_file(
        "svc",
        &format!(
            r#"
target(
    name = "runner",
    driver = "oci_runner",
    image = "{image}",
)
"#
        ),
    );
}

/// The fingerprint is the image's content digest, taken from the daemon.
///
/// Asserted against what `docker image inspect` reports rather than against a
/// hardcoded value: the point is that the driver reads the *real* digest, and a
/// test with its own copy of the answer would not notice the driver inventing
/// one.
#[tokio::test]
async fn the_fingerprint_is_the_image_digest() -> anyhow::Result<()> {
    skip_unless_docker!();
    if !build_image(TEST_IMAGE, "one") {
        eprintln!("skipping: could not build the fixture image");
        return Ok(());
    }
    let Some(digest) = image_id(TEST_IMAGE) else {
        eprintln!("skipping: docker did not report a digest for the fixture image");
        return Ok(());
    };

    let ws = workspace();
    write_runner(&ws, TEST_IMAGE);
    let doc = common::artifact_string(&*ws.run("//svc:runner").await?);

    assert!(
        doc.contains(&format!("oci:{digest}")),
        "the fingerprint must be the digest docker reports ({digest}); got {doc}"
    );
    assert!(
        doc.contains("\"runner\": \"oci\""),
        "this plugin implements its own runner rather than naming a builtin — a container's \
         lifecycle is one no builtin expresses; got {doc}"
    );
    Ok(())
}

/// The container is named **by digest**, not by the tag it was configured with
/// — so the container that runs the build is the one the fingerprint describes,
/// even if the tag moves mid-build.
#[tokio::test]
async fn the_config_uses_the_digest_not_the_tag() -> anyhow::Result<()> {
    skip_unless_docker!();
    if !build_image(TEST_IMAGE, "one") {
        eprintln!("skipping: could not build the fixture image");
        return Ok(());
    }
    let Some(digest) = image_id(TEST_IMAGE) else {
        eprintln!("skipping: docker did not report a digest");
        return Ok(());
    };

    let ws = workspace();
    write_runner(&ws, TEST_IMAGE);
    let doc = common::artifact_string(&*ws.run("//svc:runner").await?);

    let config = doc
        .split_once("\"config\"")
        .map(|(_, rest)| rest)
        .unwrap_or_default();
    assert!(
        config.contains(&digest),
        "the config must name the digest; got {doc}"
    );
    assert!(
        !config.contains(TEST_IMAGE),
        "the config must not name the tag — it can move under the build; got {doc}"
    );
    Ok(())
}

/// **The reason the digest is resolved at all.** Two images with different
/// content must produce different runner output, so their consumers do not
/// share a cache entry.
///
/// This is the assertion that would fail if the driver recorded the tag: both
/// runners would emit identical bytes while describing different containers.
#[tokio::test]
async fn two_images_produce_two_fingerprints() -> anyhow::Result<()> {
    skip_unless_docker!();
    if !build_image(TEST_IMAGE, "one") || !build_image(OTHER_IMAGE, "two") {
        eprintln!("skipping: could not build the fixture images");
        return Ok(());
    }
    let (Some(a), Some(b)) = (image_id(TEST_IMAGE), image_id(OTHER_IMAGE)) else {
        eprintln!("skipping: docker did not report both digests");
        return Ok(());
    };
    if a == b {
        eprintln!("skipping: the two fixture images are byte-identical on this daemon");
        return Ok(());
    }

    let ws_a = workspace();
    write_runner(&ws_a, TEST_IMAGE);
    let doc_a = common::artifact_string(&*ws_a.run("//svc:runner").await?);

    let ws_b = workspace();
    write_runner(&ws_b, OTHER_IMAGE);
    let doc_b = common::artifact_string(&*ws_b.run("//svc:runner").await?);

    assert_ne!(
        doc_a, doc_b,
        "two different images must not emit identical runner output, or their consumers \
         would share a cache entry across containers"
    );
    Ok(())
}

/// **The reason this is a runner and not a `session` config.** The old form
/// launched `heph __runner-agent` inside the container, so heph's own binary
/// had to be mounted in and runnable there — which on a macOS host it is not,
/// the binary being Darwin and the image Linux. `docker exec` needs nothing of
/// heph inside the image, and this asserts the mount does not come back.
#[tokio::test]
async fn the_container_does_not_need_the_heph_binary() -> anyhow::Result<()> {
    skip_unless_docker!();
    if !build_image(TEST_IMAGE, "one") {
        eprintln!("skipping: could not build the fixture image");
        return Ok(());
    }
    let ws = workspace();
    write_runner(&ws, TEST_IMAGE);
    let doc = common::artifact_string(&*ws.run("//svc:runner").await?);

    let heph = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "heph".to_string());
    assert!(
        !doc.contains(&heph),
        "the container config must not mount the heph binary; got {doc}"
    );
    Ok(())
}

/// **The whole point: a target runs inside the container.**
///
/// Not the emitted config — the actual execution. The command asserts on
/// something only true *inside* the image: `/etc/heph-e2e-marker` is written by
/// the fixture's Dockerfile and exists on no host. A target that quietly ran
/// locally fails here rather than passing.
#[tokio::test]
async fn a_target_runs_inside_the_container() -> anyhow::Result<()> {
    skip_unless_docker!();
    if !build_marker_image() {
        eprintln!("skipping: could not build the fixture image");
        return Ok(());
    }

    let ws = workspace();
    ws.write_build_file(
        "svc",
        &format!(
            r#"
target(
    name = "runner",
    driver = "oci_runner",
    image = "{MARKER_IMAGE}",
)
target(
    name = "inside",
    driver = "bash",
    runner = ":runner",
    run = "cat /etc/heph-e2e-marker > $OUT",
    out = "where.txt",
)
"#
        ),
    );

    let got = common::artifact_string(&*ws.run("//svc:inside").await?);
    assert!(
        got.contains("in-the-container"),
        "the target must have run inside the image, which is the only place that marker \
         exists; got {got:?}"
    );
    Ok(())
}

/// A tool the target declares must be findable inside the container, and the
/// image's own directories must still be there behind it.
///
/// The regression this pins: a runner that carries the environment out of band
/// — `oci` turns it into `docker exec -e KEY=VALUE` and hands back the *docker
/// client's* environment — used to have the `PATH` prefix composed onto it
/// after `prepare`, i.e. onto the client. The target inside the container got
/// neither its declared tools nor heph's builtins, and silently: the container
/// fell back to the image's own tools, so a recipe kept working with a
/// different binary than its cache key names.
#[tokio::test]
async fn a_declared_tool_is_on_the_path_inside_the_container() -> anyhow::Result<()> {
    skip_unless_docker!();
    if !build_marker_image() {
        eprintln!("skipping: could not build the fixture image");
        return Ok(());
    }

    let ws = workspace();
    ws.write_build_file(
        "svc",
        &format!(
            r#"
target(
    name = "runner",
    driver = "oci_runner",
    image = "{MARKER_IMAGE}",
)
target(
    name = "tool",
    driver = "bash",
    run = "printf '#!/bin/sh\necho from-the-declared-tool\n' > $OUT && chmod +x $OUT",
    out = "heph-e2e-tool",
)
target(
    name = "inside",
    driver = "bash",
    runner = ":runner",
    tools = [":tool"],
    run = "{{ heph-e2e-tool; command -v cat; }} > $OUT",
    out = "where.txt",
)
"#
        ),
    );

    let got = common::artifact_string(&*ws.run("//svc:inside").await?);
    assert!(
        got.contains("from-the-declared-tool"),
        "a declared tool must be on PATH inside the container; got {got:?}"
    );
    assert!(
        got.contains("/bin/cat"),
        "the image's own directories must still be on PATH behind the target's; got {got:?}"
    );
    Ok(())
}

/// An image the daemon has never seen must fail by name, pointing at the
/// `oci_load` that would put it there — not fail somewhere later with a digest
/// nobody can explain.
#[tokio::test]
async fn an_absent_image_is_diagnosable() -> anyhow::Result<()> {
    skip_unless_docker!();
    let ws = workspace();
    write_runner(&ws, "heph-e2e-definitely-absent:0");

    let err = match ws.run("//svc:runner").await {
        Ok(_) => panic!("an image the daemon does not have must fail the build"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("heph-e2e-definitely-absent"), "{err}");
    assert!(
        err.contains("oci_load"),
        "must point at what puts the image in the daemon; got {err}"
    );
    Ok(())
}
