#![expect(
    clippy::panic_in_result_fn,
    clippy::panic,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! End-to-end coverage for the `oci_*` drivers against the **real** `docker`.
//!
//! The sibling `oci.rs` suite drives a fake binary: it proves the driver builds
//! the argv it means to and that the engine wires the outputs, and it runs
//! everywhere. What it cannot prove is that the argv is one BuildKit accepts,
//! that the archive is the format the driver claims, or that a `COPY` written
//! against heph's context layout resolves. That is what this file is for — every
//! assertion here is about something only a real builder can answer.
//!
//! Every image is `FROM scratch`, so no base image is ever pulled. The suite
//! skips (it does not fail) when there is no usable `docker` — notably macOS
//! CI, which has no daemon.
//!
//! The registry and daemon halves (`oci_push` / `oci_pull` / `oci_load`) are
//! covered too, against a throwaway `registry:2` container. Those tests need the
//! network once, to pull the registry image, and skip if they cannot get it.
//! They no longer need any host tool beyond docker itself: push and pull speak
//! the registry protocol in-process and load goes through the daemon API.

mod common;

use hplugin_oci::pluginoci;
use htestkit::WorkspaceBuilder;
use std::process::Command;
use std::sync::OnceLock;

/// How long a probe may take before the host counts as having no usable docker.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Run a command with a deadline, killing it if it overruns.
///
/// Every probe here talks to the daemon, and a half-dead daemon does not fail —
/// it *hangs*. `docker info` against a Docker Desktop whose socket has stopped
/// answering blocks forever, which would wedge the whole test binary on a
/// machine where the honest answer is "skip". A bounded wait turns that into a
/// skip.
fn probe(args: &[&str]) -> bool {
    probe_bin("docker", args, PROBE_TIMEOUT)
}

fn probe_for(args: &[&str], timeout: std::time::Duration) -> bool {
    probe_bin("docker", args, timeout)
}

fn probe_bin(bin: &str, args: &[&str], timeout: std::time::Duration) -> bool {
    probe_status(bin, args, timeout) == Some(true)
}

/// `Some(success)` when the command ran to completion inside the deadline,
/// `None` when it could not be spawned or had to be killed.
///
/// The distinction matters for probes where *failing* is a fine answer and only
/// hanging is disqualifying — asking a daemon about an image that does not exist,
/// say.
fn probe_status(bin: &str, args: &[&str], timeout: std::time::Duration) -> Option<bool> {
    let Ok(mut child) = Command::new(bin)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return None;
    };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status.success()),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    drop(child.kill());
                    drop(child.wait());
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

/// A usable docker: the CLI, the `buildx` plugin, and a daemon that answers.
///
/// Probed once per test binary — each check is a process spawn, and every test
/// in the file needs the answer.
fn docker_available() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    // `buildx version` proves the plugin is installed; `buildx inspect
    // --bootstrap` proves a builder is actually reachable — and it is the exact
    // call `docker_build::parse` makes, so if it works the driver works.
    //
    // Deliberately not `docker info`: it walks every configured builder, so one
    // stale context (a Docker Desktop entry left behind after switching to
    // another runtime) makes it hang even though builds run fine.
    *OK.get_or_init(|| {
        probe(&["buildx", "version"]) && probe(&["buildx", "inspect", "--bootstrap"])
    })
}

/// Whether the *default* buildx builder can write an image archive to a file.
///
/// It very often cannot. The plain `docker` driver — what a stock Docker Engine
/// gives you — has no file exporters at all: it can load or push into the
/// daemon and nothing else, so `--output type=oci,dest=…` *and*
/// `type=docker,dest=…` both fail. A `docker-container` builder, or the daemon's
/// containerd image store, is what `docker_build` actually needs.
///
/// There is no cheap way to ask (`buildx inspect` reports the driver but not the
/// image store), so the probe is a real one-platform build.
fn default_builder_can_export() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        if !docker_available() {
            return false;
        }
        let Ok(dir) = tempfile::tempdir() else {
            return false;
        };
        let ctx = dir.path();
        if std::fs::write(ctx.join("Dockerfile"), "FROM scratch\n").is_err() {
            return false;
        }
        probe_for(
            &[
                "buildx",
                "build",
                "--output",
                &format!("type=oci,dest={}", ctx.join("probe.tar").display()),
                &ctx.display().to_string(),
            ],
            std::time::Duration::from_secs(120),
        )
    })
}

/// Skip rather than fail when the host has no docker: this suite is additive
/// coverage on top of `oci.rs`, which runs everywhere.
macro_rules! require_docker {
    () => {
        if !docker_available() {
            eprintln!("skipping: no usable docker (needs buildx and a running daemon)");
            return Ok(());
        }
    };
}

/// A builder every archive-producing test can use.
///
/// `None` means "the default one is fine" — a containerd-backed daemon, or
/// something like OrbStack. Otherwise a throwaway `docker-container` builder is
/// created for the test and removed when it drops, which is exactly what a user
/// on a stock Docker Engine has to do: `docker_build` writes an archive, and the
/// plain `docker` driver cannot write one at all.
///
/// `Err` when neither is possible — the caller skips.
fn test_builder() -> Result<Option<ContainerBuilder>, ()> {
    if default_builder_can_export() {
        return Ok(None);
    }
    ContainerBuilder::create().map(Some).ok_or(())
}

/// The `builder = "…"` line to splice into a BUILD file, if one is needed.
fn builder_attr(b: &Option<ContainerBuilder>) -> String {
    b.as_ref()
        .map_or_else(String::new, |b| format!("builder = \"{}\",", b.name))
}

/// Resolve a builder for the test, or skip.
macro_rules! require_builder {
    () => {
        match test_builder() {
            Ok(b) => b,
            Err(()) => {
                eprintln!(
                    "skipping: the default buildx builder cannot export an image archive and a \
                     docker-container builder could not be created"
                );
                return Ok(());
            }
        }
    };
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
        .with_managed_driver(Box::new(pluginoci::docker_build::Driver::new()))
        .with_provider(|_| Box::new(pluginoci::platform::Provider))
        .with_managed_driver(Box::new(pluginoci::platform::Driver::new()))
        .with_managed_driver(Box::new(pluginoci::load::Driver::new()))
        .with_managed_driver(Box::new(pluginoci::push::Driver::new()))
        .with_managed_driver(Box::new(pluginoci::pull::Driver::new()))
        .build()
        .expect("build workspace")
}

/// A throwaway `registry:2` on a random host port, removed on drop.
///
/// `oci_push` and `oci_pull` only speak `docker://`, so a registry is the only
/// way to exercise them at all. A local one keeps the test off the network for
/// everything except the one-time pull of the registry image itself.
struct Registry {
    id: String,
    port: u16,
}

impl Registry {
    /// `None` when the registry image cannot be pulled or the container never
    /// starts listening — a skip, not a failure.
    fn start() -> Option<Self> {
        if !probe_for(&["pull", "registry:2"], std::time::Duration::from_secs(120)) {
            return None;
        }
        // Port 0 lets the daemon pick, so parallel tests (and anything else on
        // the machine) cannot collide on a fixed port.
        let out = Command::new("docker")
            .args(["run", "-d", "-p", "127.0.0.1:0:5000", "registry:2"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let port = Self::host_port(&id)?;
        let reg = Registry { id, port };
        reg.wait_ready().then_some(reg)
    }

    fn host_port(id: &str) -> Option<u16> {
        let out = Command::new("docker")
            .args(["port", id, "5000/tcp"])
            .output()
            .ok()?;
        // `127.0.0.1:49154` (possibly several lines, one per family).
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|l| l.rsplit_once(':').and_then(|(_, p)| p.trim().parse().ok()))
    }

    /// The container is "created" long before the registry binds, so connect
    /// until it answers.
    fn wait_ready(&self) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            if std::net::TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        false
    }

    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        drop(Command::new("docker").args(["rm", "-f", &self.id]).output());
    }
}

/// The entry names inside the built archive. What distinguishes an OCI layout
/// archive from a docker one is exactly this listing, so it is how a test can
/// tell whether `format` produced what it promised.
///
/// Takes the archive bytes rather than a path: the artifact is content in the
/// cache, and its on-disk location is the engine's business.
fn archive_entries(tar_bytes: &[u8]) -> Vec<String> {
    tar::Archive::new(std::io::Cursor::new(tar_bytes))
        .entries()
        .expect("read archive")
        .map(|e| {
            e.expect("archive entry")
                .path()
                .expect("entry path")
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// Read one file out of the built archive.
fn archive_file(tar_bytes: &[u8], name: &str) -> String {
    use std::io::Read as _;
    for entry in tar::Archive::new(std::io::Cursor::new(tar_bytes))
        .entries()
        .expect("read archive")
    {
        let mut entry = entry.expect("archive entry");
        if entry.path().expect("entry path").to_string_lossy() == name {
            let mut s = String::new();
            entry.read_to_string(&mut s).expect("read entry");
            return s;
        }
    }
    panic!("{name} not found in the archive");
}

/// Read one file out of a directory-shaped artifact (an OCI layout).
fn artifact_file(result: &heph::engine::EResult, name: &str) -> String {
    use heph::hartifactcontent::WalkEntryKind;
    use std::io::Read as _;
    for artifact in &result.artifacts {
        for entry in artifact.walk().expect("walk artifacts") {
            let entry = entry.expect("artifact entry");
            if !entry.path.ends_with(name) {
                continue;
            }
            if let WalkEntryKind::File { mut data, .. } = entry.kind {
                let mut s = String::new();
                data.read_to_string(&mut s).expect("read artifact entry");
                return s;
            }
        }
    }
    panic!("{name} not found in the artifact");
}

/// The first `sha256:<hex>` digest in an OCI JSON document.
///
/// Hand-rolled rather than pulled through a JSON parser: the only thing these
/// tests need out of the index is the one digest to follow.
fn digest_of(json: &str) -> String {
    json.split_once("\"digest\":\"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(digest, _)| digest.to_string())
        .unwrap_or_else(|| panic!("no digest in: {json}"))
}

/// The in-archive path of a blob named by an `algo:hex` digest.
fn blob_path(digest: &str) -> String {
    let (algo, hex) = digest.split_once(':').expect("algo:hex digest");
    format!("blobs/{algo}/{hex}")
}

/// The image archive a `docker_build` target produced, as bytes.
///
/// Restricted to the default output group: a bare run would also return the
/// `digest` group, and the two would concatenate into something that is not a
/// tar.
async fn archive_of(ws: &htestkit::Workspace, addr: &str) -> anyhow::Result<Vec<u8>> {
    let result = ws.run_addr_outputs(addr, &[""]).await?;
    Ok(common::artifact_bytes(&result))
}

/// A real BuildKit build through the driver: the argv is one buildx accepts, the
/// archive is a real OCI layout, and the digest output is the image's own digest
/// rather than whatever the metadata file happened to contain.
#[tokio::test]
async fn test_real_docker_builds_an_oci_archive_and_a_real_digest() -> anyhow::Result<()> {
    require_docker!();
    let builder = require_builder!();
    let ws = workspace();
    ws.write_build_file(
        "app",
        &format!(
            r#"
target(name = "payload", driver = "bash", run = "echo payload > $OUT", out = "payload.txt")
target(
    name = "dockerfile",
    driver = "bash",
    run = "printf 'FROM scratch\nCOPY app/payload.txt /payload.txt\n' > $OUT",
    out = "Dockerfile",
)
target(
    name = "img",
    driver = "docker_build",
    context = [":dockerfile", ":payload"],
    {builder}
)
"#,
            builder = builder_attr(&builder)
        ),
    );

    let entries = archive_entries(&archive_of(&ws, "//app:img").await?);
    assert!(
        entries.iter().any(|e| e == "oci-layout"),
        "an oci-format archive must carry an oci-layout marker, got: {entries:?}"
    );
    assert!(
        entries.iter().any(|e| e == "index.json"),
        "an oci-format archive must carry an index.json, got: {entries:?}"
    );

    // The digest group must be the image's real digest, not a placeholder.
    let digest = ws.run_addr_outputs("//app:img", &["digest"]).await?;
    let digest = common::artifact_string(&digest).trim().to_string();
    let hex = digest
        .strip_prefix("sha256:")
        .unwrap_or_else(|| panic!("digest must be sha256-prefixed, got: {digest}"));
    assert!(
        hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()),
        "digest must be a full sha256, got: {digest}"
    );
    Ok(())
}

/// `format = "docker"` must produce an archive `docker load` can read — and it
/// must reach it the same way an OCI build does. The producer and the consumer
/// restate the format independently, so a mismatch there is the failure mode
/// that costs a user a whole build.
///
/// The Dockerfile `COPY`s a cross-package dep through `SRC_BIN`, so this also
/// pins what only shares a code path today: `format` selects the exporter and
/// nothing else, and the build context is the workspace root under both formats.
/// A package-rooted context would fail the build outright here.
#[tokio::test]
async fn test_real_docker_format_produces_a_docker_archive() -> anyhow::Result<()> {
    require_docker!();
    let builder = require_builder!();
    let ws = workspace();
    ws.write_build_file(
        "cmd/server",
        r#"
target(name = "bin", driver = "bash", run = "echo server-binary > $OUT", out = "server")
"#,
    );
    ws.write_build_file(
        "app",
        &format!(
            r#"
target(
    name = "dockerfile",
    driver = "bash",
    run = "printf 'FROM scratch\nARG SRC_BIN\nCOPY ${{SRC_BIN}} /usr/bin/server\n' > $OUT",
    out = "Dockerfile",
)
target(
    name = "img",
    driver = "docker_build",
    format = "docker",
    context = {{"": [":dockerfile"], "bin": ["//cmd/server:bin"]}},
    {builder}
)
"#,
            builder = builder_attr(&builder)
        ),
    );

    let entries = archive_entries(&archive_of(&ws, "//app:img").await?);
    // `manifest.json` is what `docker load` reads, and it is what an OCI-format
    // archive does *not* have. Modern BuildKit also ships an `index.json` /
    // `oci-layout` in the same tar, so its presence proves nothing either way —
    // the docker manifest is the discriminator.
    assert!(
        entries.iter().any(|e| e == "manifest.json"),
        "a docker-format archive is identified by manifest.json, got: {entries:?}"
    );
    Ok(())
}

/// The whole `context` design in one build: deps land at their
/// workspace-relative paths, the workspace root is the build context, and each
/// group is exported as `SRC_<GROUP>`. A Dockerfile that `COPY`s through the
/// build arg only resolves if all three hold — a wrong context root or a wrong
/// arg value fails the build outright.
#[tokio::test]
async fn test_real_docker_copies_a_cross_package_dep_through_the_src_arg() -> anyhow::Result<()> {
    require_docker!();
    let builder = require_builder!();
    let ws = workspace();
    ws.write_build_file(
        "cmd/server",
        r#"
target(name = "bin", driver = "bash", run = "echo server-binary > $OUT", out = "server")
"#,
    );
    ws.write_build_file(
        "app",
        &format!(
            r#"
target(
    name = "dockerfile",
    driver = "bash",
    run = "printf 'FROM scratch\nARG SRC_BIN\nCOPY ${{SRC_BIN}} /usr/bin/server\n' > $OUT",
    out = "Dockerfile",
)
target(
    name = "img",
    driver = "docker_build",
    context = {{"": [":dockerfile"], "bin": ["//cmd/server:bin"]}},
    {builder}
)
"#,
            builder = builder_attr(&builder)
        ),
    );

    // The assertion is that this succeeds: BuildKit resolves `COPY ${SRC_BIN}`
    // against the context heph handed it, and errors if the path is not there.
    let entries = archive_entries(&archive_of(&ws, "//app:img").await?);
    assert!(
        !entries.is_empty(),
        "the build must have produced a real archive"
    );
    Ok(())
}

/// `stage` must really narrow the build to one stage's DAG. The unselected stage
/// here cannot build at all, so a driver that dropped `--target` — or passed it
/// where buildx ignores it — fails instead of quietly building everything.
#[tokio::test]
async fn test_real_docker_builds_only_the_selected_stage() -> anyhow::Result<()> {
    require_docker!();
    let builder = require_builder!();
    let ws = workspace();
    ws.write_build_file(
        "app",
        &format!(
            r#"
target(
    name = "dockerfile",
    driver = "bash",
    run = "printf 'FROM scratch AS good\nCOPY app/payload.txt /ok\n\nFROM scratch AS bad\nCOPY app/does-not-exist /bad\n' > $OUT",
    out = "Dockerfile",
)
target(name = "payload", driver = "bash", run = "echo payload > $OUT", out = "payload.txt")
target(
    name = "good",
    driver = "docker_build",
    stage = "good",
    out = "good.tar",
    context = [":dockerfile", ":payload"],
    {builder}
)
target(
    name = "bad",
    driver = "docker_build",
    stage = "bad",
    out = "bad.tar",
    context = [":dockerfile", ":payload"],
    {builder}
)
"#,
            builder = builder_attr(&builder)
        ),
    );

    ws.run("//app:good").await?;

    let err = ws
        .run("//app:bad")
        .await
        .err()
        .expect("the stage that COPYs a missing file must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("does-not-exist") || msg.contains("failed to compute cache key"),
        "the builder's own error must survive, got: {msg}"
    );
    Ok(())
}

/// A multi-platform build produces an index with an entry per platform. Only a
/// real build can show this: the manifest list is BuildKit's output, not
/// something the driver assembles.
#[tokio::test]
async fn test_real_docker_multi_arch_build_indexes_both_platforms() -> anyhow::Result<()> {
    require_docker!();
    let builder = require_builder!();
    let ws = workspace();
    ws.write_build_file(
        "app",
        &format!(
            r#"
target(name = "payload", driver = "bash", run = "echo payload > $OUT", out = "payload.txt")
target(
    name = "dockerfile",
    driver = "bash",
    run = "printf 'FROM scratch\nCOPY app/payload.txt /payload.txt\n' > $OUT",
    out = "Dockerfile",
)
target(
    name = "img",
    driver = "docker_build",
    context = [":dockerfile", ":payload"],
    platforms = ["linux/amd64", "linux/arm64"],
    {builder}
)
"#,
            builder = builder_attr(&builder)
        ),
    );

    let tar = archive_of(&ws, "//app:img").await?;
    // The archive's top-level index.json points at *another* index (the manifest
    // list); the per-platform entries live in that blob, so follow the digest.
    let root = archive_file(&tar, "index.json");
    let manifest_list = archive_file(&tar, &blob_path(&digest_of(&root)));
    let manifest_list: String = manifest_list
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    for arch in ["amd64", "arm64"] {
        assert!(
            manifest_list.contains(&format!("\"architecture\":\"{arch}\"")),
            "the manifest list must carry a {arch} entry, got: {manifest_list}"
        );
    }
    Ok(())
}

/// `oci_load` must put a runnable image in the daemon under the requested tag.
/// The archive-format/tool pairing and the post-`docker load` tagging are both
/// invisible to a fake — the daemon is the only thing that can confirm them.
#[tokio::test]
async fn test_real_docker_load_tags_the_image_in_the_daemon() -> anyhow::Result<()> {
    require_docker!();
    let builder = require_builder!();
    // Unique per process so parallel runs (and a leftover from a killed run) do
    // not collide in the shared daemon.
    let tag = format!("heph-e2e-oci-load:{}", std::process::id());

    let ws = workspace();
    ws.write_build_file(
        "app",
        &format!(
            r#"
target(name = "payload", driver = "bash", run = "echo payload > $OUT", out = "payload.txt")
target(
    name = "dockerfile",
    driver = "bash",
    run = "printf 'FROM scratch\nCOPY app/payload.txt /payload.txt\n' > $OUT",
    out = "Dockerfile",
)
target(
    name = "img",
    driver = "docker_build",
    format = "docker",
    context = [":dockerfile", ":payload"],
    {builder}
)
target(name = "load", driver = "oci_load", image = ":img", tag = "{tag}")
"#,
            builder = builder_attr(&builder),
        ),
    );

    let loaded = ws.run("//app:load").await;
    // Clean up before asserting: the daemon is shared state, and a failed
    // assertion must not leave an image behind.
    let inspect = Command::new("docker")
        .args(["image", "inspect", &tag])
        .output();
    drop(Command::new("docker").args(["rmi", "-f", &tag]).output());

    loaded?;
    assert!(
        inspect.is_ok_and(|o| o.status.success()),
        "the tag {tag} must exist in the daemon after oci_load"
    );
    Ok(())
}

/// The default `format = "oci"` path end to end: buildx writes an OCI archive
/// and `skopeo` copies it into the daemon under the requested tag. Nothing in
/// the argv-level tests can show that skopeo accepts the transport, the
/// `--override-os/--override-arch` pair and the `docker-daemon:` destination in
/// the order the driver emits them.
#[tokio::test]
async fn test_real_skopeo_loads_an_oci_archive_into_the_daemon() -> anyhow::Result<()> {
    require_docker!();
    let builder = require_builder!();
    let tag = format!("heph-e2e-skopeo-load:{}", std::process::id());

    let ws = workspace();
    ws.write_build_file(
        "app",
        &format!(
            r#"
target(name = "payload", driver = "bash", run = "echo payload > $OUT", out = "payload.txt")
target(
    name = "dockerfile",
    driver = "bash",
    run = "printf 'FROM scratch\nCOPY app/payload.txt /payload.txt\n' > $OUT",
    out = "Dockerfile",
)
target(
    name = "img",
    driver = "docker_build",
    context = [":dockerfile", ":payload"],
    {builder}
)
target(name = "load", driver = "oci_load", image = ":img", tag = "{tag}")
"#,
            builder = builder_attr(&builder),
        ),
    );

    let loaded = ws.run("//app:load").await;
    let inspect = Command::new("docker")
        .args(["image", "inspect", &tag])
        .output();
    drop(Command::new("docker").args(["rmi", "-f", &tag]).output());

    loaded?;
    assert!(
        inspect.is_ok_and(|o| o.status.success()),
        "the tag {tag} must exist in the daemon after a skopeo oci_load"
    );
    Ok(())
}

/// The multi-arch story end to end, against a real registry: a two-platform
/// image is pushed with the whole manifest list intact, pulled back as an OCI
/// layout that still holds both platforms, and used as the `bases` entry of
/// another two-platform build.
///
/// Each step is the one that would silently do the wrong thing on its own —
/// `skopeo copy` without `--multi-arch all` pushes a single instance, a pull
/// without `all_platforms` produces a layout that has no manifest for half the
/// build, and `FROM base` only resolves if the `oci-layout://` build context is
/// wired correctly. Only a registry plus a real builder can tell.
#[tokio::test]
async fn test_real_multi_arch_push_pull_and_build_from_the_pulled_base() -> anyhow::Result<()> {
    require_docker!();
    let builder = require_builder!();
    let Some(registry) = Registry::start() else {
        eprintln!("skipping: could not start a local registry:2 (no network, or no daemon)");
        return Ok(());
    };

    let ws = workspace();
    ws.write_build_file(
        "app",
        &format!(
            r#"
target(name = "payload", driver = "bash", run = "echo payload > $OUT", out = "payload.txt")
target(
    name = "dockerfile",
    driver = "bash",
    run = "printf 'FROM scratch\nCOPY app/payload.txt /payload.txt\n' > $OUT",
    out = "Dockerfile",
)
target(
    name = "img",
    driver = "docker_build",
    context = [":dockerfile", ":payload"],
    platforms = ["linux/amd64", "linux/arm64"],
    {builder}
)
target(
    name = "push",
    driver = "oci_push",
    image = ":img",
    ref = "{host}/heph-e2e/app:multi",
    insecure = True,
)
target(
    name = "base",
    driver = "oci_pull",
    ref = "{host}/heph-e2e/app:multi",
    layout = True,
    all_platforms = True,
    insecure = True,
)
target(
    name = "derived_dockerfile",
    driver = "bash",
    run = "printf 'FROM base\nCOPY app/payload.txt /again.txt\n' > $OUT",
    out = "Dockerfile.derived",
)
target(
    name = "derived",
    driver = "docker_build",
    dockerfile = "Dockerfile.derived",
    out = "derived.tar",
    context = [":derived_dockerfile", ":payload"],
    bases = {{"base": [":base"]}},
    platforms = ["linux/amd64", "linux/arm64"],
    {builder}
)
"#,
            host = registry.host(),
            builder = builder_attr(&builder),
        ),
    );

    ws.run("//app:push").await?;

    // The pulled layout must still cover both platforms — that is what pushing
    // the whole manifest list and pulling with `all_platforms` are for, and what
    // the derived build below needs.
    //
    // `index.json` is the tagged entry point buildx resolves `oci-layout://`
    // against; the per-platform entries live in the index it points at.
    let pulled = ws.run("//app:base").await?;
    let layout_files = common::artifact_paths(&pulled);
    assert!(
        layout_files.iter().any(|p| p.ends_with("index.json")),
        "an OCI layout must have an index.json, got: {layout_files:?}"
    );
    let index = artifact_file(&pulled, "index.json");
    assert!(
        index.contains("org.opencontainers.image.ref.name"),
        "the layout needs a tag for buildx to resolve `FROM base`, got: {index}"
    );
    let manifest_list: String = artifact_file(&pulled, &blob_path(&digest_of(&index)))
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    for arch in ["amd64", "arm64"] {
        assert!(
            manifest_list.contains(&format!("\"architecture\":\"{arch}\"")),
            "the pulled layout must keep the {arch} instance, got: {manifest_list}"
        );
    }

    // `FROM base` against the pulled layout, for both platforms: fails outright
    // if the build context is not wired or the base is single-instance.
    let derived = archive_entries(&archive_of(&ws, "//app:derived").await?);
    assert!(
        derived.iter().any(|e| e == "index.json"),
        "the derived image must be a real OCI archive, got: {derived:?}"
    );
    Ok(())
}

/// A `docker-container` buildx builder, removed on drop.
///
/// Creating one needs the network the first time (BuildKit's own image), so a
/// failure here is a skip.
struct ContainerBuilder {
    name: String,
}

impl ContainerBuilder {
    fn create() -> Option<Self> {
        // Unique per instance, not just per process: tests run in parallel and
        // each one removes its builder on drop, so a shared name means one test
        // tearing the builder out from under another mid-build.
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let name = format!(
            "heph-e2e-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        probe_for(
            &[
                "buildx",
                "create",
                "--name",
                &name,
                "--driver",
                "docker-container",
                "--bootstrap",
            ],
            std::time::Duration::from_secs(180),
        )
        .then_some(ContainerBuilder { name })
    }
}

impl Drop for ContainerBuilder {
    fn drop(&mut self) {
        drop(
            Command::new("docker")
                .args(["buildx", "rm", "-f", &self.name])
                .output(),
        );
    }
}

/// `builder` is what makes a multi-platform build possible on a host whose
/// current builder is the plain daemon one — the case every `docker buildx`
/// default hits, and the reason the attribute exists at all.
///
/// It also has to reach the *probe*: with `platforms` unset the cache key
/// carries the builder's default platform, and asking the wrong builder would
/// key the target on a platform it never built.
#[tokio::test]
async fn test_real_docker_builds_multi_arch_on_a_named_builder() -> anyhow::Result<()> {
    require_docker!();
    let Some(builder) = ContainerBuilder::create() else {
        eprintln!("skipping: could not create a docker-container builder (no network?)");
        return Ok(());
    };

    let ws = workspace();
    ws.write_build_file(
        "app",
        &format!(
            r#"
target(name = "payload", driver = "bash", run = "echo payload > $OUT", out = "payload.txt")
target(
    name = "dockerfile",
    driver = "bash",
    run = "printf 'FROM scratch\nCOPY app/payload.txt /payload.txt\n' > $OUT",
    out = "Dockerfile",
)
target(
    name = "img",
    driver = "docker_build",
    context = [":dockerfile", ":payload"],
    platforms = ["linux/amd64", "linux/arm64"],
    builder = "{name}",
)
"#,
            name = builder.name
        ),
    );

    let tar = archive_of(&ws, "//app:img").await?;
    let manifest_list: String = archive_file(
        &tar,
        &blob_path(&digest_of(&archive_file(&tar, "index.json"))),
    )
    .chars()
    .filter(|c| !c.is_whitespace())
    .collect();
    for arch in ["amd64", "arm64"] {
        assert!(
            manifest_list.contains(&format!("\"architecture\":\"{arch}\"")),
            "the named builder must have produced a {arch} entry, got: {manifest_list}"
        );
    }
    Ok(())
}

/// On a stock Docker Engine the default builder cannot write an image archive
/// at all — the `docker` driver has no file exporters — so *every* `docker_build`
/// build fails there, whatever `format` says. BuildKit's own message names the
/// exporter but not the remedy; heph has to supply it.
///
/// Runs only where that is actually the situation: on a containerd-backed
/// daemon there is nothing to diagnose.
#[tokio::test]
async fn test_real_docker_default_builder_without_exporters_is_diagnosable() -> anyhow::Result<()> {
    require_docker!();
    if default_builder_can_export() {
        eprintln!("skipping: this host's default builder can export archives, nothing to diagnose");
        return Ok(());
    }

    let ws = workspace();
    ws.write_build_file(
        "app",
        r#"
target(name = "payload", driver = "bash", run = "echo payload > $OUT", out = "payload.txt")
target(
    name = "dockerfile",
    driver = "bash",
    run = "printf 'FROM scratch\nCOPY app/payload.txt /payload.txt\n' > $OUT",
    out = "Dockerfile",
)
target(name = "img", driver = "docker_build", context = [":dockerfile", ":payload"])
"#,
    );

    let err = ws
        .run("//app:img")
        .await
        .err()
        .expect("a builder with no file exporters cannot build a docker_build target");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("docker-container") && msg.contains("builder ="),
        "the error must say how to fix it, got: {msg}"
    );
    // BuildKit's own diagnosis survives underneath.
    assert!(msg.contains("exporter is not supported"), "got: {msg}");
    Ok(())
}
