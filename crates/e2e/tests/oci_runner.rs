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
//! # What is deliberately not here
//!
//! Actually *running* a target inside the container. That needs the heph binary
//! to execute inside the image, and on macOS — where much of this is developed —
//! the binary is a Darwin executable and the container is Linux, so the test
//! could never pass there. Covering it would mean a Linux-only test whose
//! failure mode on every other machine is "skipped", which is worse than an
//! honest gap. It is listed as one in `docs/EXEC_RUNNERS.md`.

mod common;

use hplugin_oci::pluginoci;
use htestkit::WorkspaceBuilder;
use std::process::Command;
use std::sync::OnceLock;

/// The image every fixture points at. Built here from `FROM scratch`, so
/// nothing is ever pulled and the suite stays off the network.
const TEST_IMAGE: &str = "heph-e2e-oci-runner:v1";
const OTHER_IMAGE: &str = "heph-e2e-oci-runner:v2";

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
        .with_managed_driver(Box::new(heph::pluginexec::Driver::new_bash()))
        .with_managed_driver(Box::new(pluginoci::runner::Driver::new()))
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
        doc.contains("\"runner\": \"session\""),
        "an oci runner holds one container open rather than running one per exec; got {doc}"
    );
    Ok(())
}

/// The container is launched **by digest**, not by the tag it was configured
/// with — so the container that runs the build is the one the fingerprint
/// describes, even if the tag moves mid-build.
#[tokio::test]
async fn the_launch_uses_the_digest_not_the_tag() -> anyhow::Result<()> {
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

    let launch = doc
        .split_once("\"launch\"")
        .map(|(_, rest)| rest)
        .unwrap_or_default();
    assert!(
        launch.contains(&digest),
        "the launch argv must name the digest; got {doc}"
    );
    assert!(
        !launch.contains(TEST_IMAGE),
        "the launch argv must not name the tag — it can move under the build; got {doc}"
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
