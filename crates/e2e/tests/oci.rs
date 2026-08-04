#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! End-to-end coverage for the `oci_*` drivers against a fake `docker`.
//!
//! The unit tests in `plugin-oci` drive `run()` directly; these go through the
//! real engine, so they also cover the parts a driver does not own — that the
//! declared outputs are collected as real artifacts a downstream target can
//! consume, that an unchanged context is a cache hit and does not shell out
//! again, and that a `context` dep from *another* package actually reaches the
//! build.
//!
//! No daemon is needed: the drivers are registered pointed at a shell script
//! that records its argv and writes the files buildx would have written.

mod common;

use hplugin_oci::pluginoci;
use htestkit::WorkspaceBuilder;
use std::path::PathBuf;

/// A fake `docker` that answers the builder probe and, on `buildx build`,
/// writes the metadata file and the output archive. Every invocation appends
/// its argv to `calls.log` next to the script, so a test can assert both what
/// the command looked like and how many times it ran.
const FAKE_DOCKER: &str = r#"#!/bin/sh
LOG="$(dirname "$0")/calls.log"
printf 'docker' >> "$LOG"
for a in "$@"; do printf ' %s' "$a" >> "$LOG"; done
printf '\n' >> "$LOG"

case "$2" in
  inspect) echo "Name: default"; echo "Platforms: linux/amd64, linux/arm64"; exit 0 ;;
esac

meta=""; dest=""; ctx=""
while [ $# -gt 0 ]; do
  case "$1" in
    --metadata-file) meta="$2" ;;
    --output) dest="${2#*dest=}" ;;
    *) ctx="$1" ;;
  esac
  shift
done
[ -n "$meta" ] && printf '{"containerimage.digest":"sha256:cafebabe"}' > "$meta"
# The archive stands in for the image: record what the build context contained,
# so a test can prove a dep was actually visible to the build.
[ -n "$dest" ] && (cd "$ctx" && find . -type f | sort) > "$dest"
exit 0
"#;

struct Fake {
    dir: tempfile::TempDir,
    bin: PathBuf,
}

impl Fake {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("docker");
        // Not `fs::write` + `set_permissions`: tests run in parallel, and a
        // sibling test's fork between our create and our exec inherits a
        // writable fd to this file, so the exec fails with `ETXTBSY`.
        hcore::fsutil::write_executable(&bin, FAKE_DOCKER.as_bytes()).expect("write fake docker");
        Fake { dir, bin }
    }

    /// How many builder probes ran. The probe is a locally-cached target, so a
    /// warm run must not add to this.
    fn probes(&self) -> usize {
        self.calls()
            .iter()
            .filter(|c| c.contains("inspect"))
            .count()
    }

    /// How many `buildx build` invocations happened.
    fn builds(&self) -> usize {
        self.calls()
            .iter()
            .filter(|c| c.contains("buildx build"))
            .count()
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

/// A workspace whose `oci_image` driver shells out to `fake` instead of docker.
fn workspace_with_fake(fake: &Fake) -> htestkit::Workspace {
    let bin = fake.bin.to_string_lossy().into_owned();
    WorkspaceBuilder::new()
        .expect("workspace tempdir")
        .with_provider(|init| {
            Box::new(heph::pluginbuildfile::Provider::new(
                init.root.to_path_buf(),
                init.runtime.clone(),
            ))
        })
        .with_managed_driver(Box::new(heph::pluginexec::Driver::new_bash()))
        .with_managed_driver(Box::new(pluginoci::Driver::with_binary(bin.clone())))
        // `oci_image` without explicit `platforms` depends on
        // `//@heph/oci:platform`, so the probe's provider and driver have to be
        // registered for the graph to resolve at all.
        .with_provider(|_| Box::new(pluginoci::platform::Provider))
        .with_managed_driver(Box::new(pluginoci::platform::Driver::with_binary(bin)))
        .build()
        .expect("build workspace")
}

/// The archive and the digest are real target outputs: a downstream `bash`
/// target can depend on either group and read the bytes.
#[tokio::test]
async fn test_oci_image_outputs_are_consumable_by_a_downstream_target() -> anyhow::Result<()> {
    let fake = Fake::new();
    let ws = workspace_with_fake(&fake);
    ws.write_build_file(
        "app",
        r#"
target(name = "srcs", driver = "bash", run = "echo hi > $OUT", out = "hi.txt")
target(
    name = "img",
    driver = "oci_image",
    context = [":srcs", ":dockerfile"],
)
target(
    name = "dockerfile",
    driver = "bash",
    run = "echo 'FROM scratch' > $OUT",
    out = "Dockerfile",
)
target(
    name = "show",
    driver = "bash",
    run = "cat $SRC_DIGEST > $OUT",
    out = "out.txt",
    deps = {"digest": ["//app:img|digest"]},
)
"#,
    );

    let result = ws.run("//app:show").await?;
    assert_eq!(
        common::artifact_string(&result).trim(),
        "sha256:cafebabe",
        "the digest group must carry the built image's digest"
    );
    Ok(())
}

/// An unchanged context is a cache hit: the second run must not shell out to
/// the builder again. This is the whole efficiency claim of the driver, and
/// only the builder can prove it.
#[tokio::test]
async fn test_oci_image_unchanged_context_does_not_rebuild() -> anyhow::Result<()> {
    let fake = Fake::new();
    let ws = workspace_with_fake(&fake);
    ws.write_build_file(
        "app",
        r#"
target(
    name = "dockerfile",
    driver = "bash",
    run = "echo 'FROM scratch' > $OUT",
    out = "Dockerfile",
)
target(name = "img", driver = "oci_image", context = [":dockerfile"])
"#,
    );

    ws.run("//app:img").await?;
    assert_eq!(fake.builds(), 1, "first run builds");

    ws.run("//app:img").await?;
    assert_eq!(
        fake.builds(),
        1,
        "an unchanged context must be a cache hit, not a second build"
    );
    Ok(())
}

/// The builder-platform probe is a locally-cached target, so it runs once and a
/// warm run reuses the answer. That is the whole reason it is a cached target
/// rather than an uncached one — an uncached dep must execute every invocation
/// to produce the hashout its consumer needs.
#[tokio::test]
async fn test_oci_image_probes_the_builder_once_across_runs() -> anyhow::Result<()> {
    let fake = Fake::new();
    let ws = workspace_with_fake(&fake);
    ws.write_build_file(
        "app",
        r#"
target(
    name = "dockerfile",
    driver = "bash",
    run = "echo 'FROM scratch' > $OUT",
    out = "Dockerfile",
)
target(name = "img", driver = "oci_image", context = [":dockerfile"])
"#,
    );

    ws.run("//app:img").await?;
    assert_eq!(fake.probes(), 1, "the first run probes the builder");

    ws.run("//app:img").await?;
    assert_eq!(
        fake.probes(),
        1,
        "a warm run must reuse the cached platform, not re-probe"
    );
    Ok(())
}

/// A `context` dep from another package is visible to the build. The build
/// context is the sandbox workspace root, so the dep's workspace-relative path
/// is reachable — a package-rooted context would leave it outside, and the
/// Dockerfile's `COPY` would fail with the dep still moving the cache key.
#[tokio::test]
async fn test_oci_image_sees_a_context_dep_from_another_package() -> anyhow::Result<()> {
    let fake = Fake::new();
    let ws = workspace_with_fake(&fake);
    ws.write_build_file(
        "cmd/server",
        r#"
target(name = "bin", driver = "bash", run = "echo binary > $OUT", out = "server")
"#,
    );
    ws.write_build_file(
        "app",
        r#"
target(
    name = "dockerfile",
    driver = "bash",
    run = "echo 'FROM scratch' > $OUT",
    out = "Dockerfile",
)
target(
    name = "img",
    driver = "oci_image",
    context = {"": [":dockerfile"], "bin": ["//cmd/server:bin"]},
)
"#,
    );

    let result = ws.run("//app:img").await?;
    // The fake writes the context listing into the archive, so the archive is a
    // record of what the build could actually see.
    let listing = common::artifact_string(&result);
    assert!(
        listing.contains("./cmd/server/server"),
        "the cross-package dep must be inside the build context, got: {listing}"
    );

    // …and the Dockerfile can find it without hardcoding the layout.
    let build = fake
        .calls()
        .into_iter()
        .find(|c| c.contains("buildx build"))
        .expect("a buildx build call");
    assert!(
        build.contains("--build-arg SRC_BIN=cmd/server/server"),
        "the group must be exported as a SRC_* build arg, got: {build}"
    );
    Ok(())
}

/// `dockerfile = ":target"` is the whole point of addressing it: the generated
/// Dockerfile is a dep, so it is staged and hashed without appearing in
/// `context` and without the BUILD file spelling where it lands.
#[tokio::test]
async fn test_oci_image_takes_a_dockerfile_by_address() -> anyhow::Result<()> {
    let fake = Fake::new();
    let ws = workspace_with_fake(&fake);
    ws.write_build_file(
        "app",
        r#"
target(name = "srcs", driver = "bash", run = "echo hi > $OUT", out = "hi.txt")
target(
    name = "gen",
    driver = "bash",
    run = "printf 'FROM scratch\nCOPY app/hi.txt /hi.txt\n' > $OUT",
    out = "generated.Dockerfile",
)
target(
    name = "img",
    driver = "oci_image",
    dockerfile = ":gen",
    context = [":srcs"],
)
"#,
    );

    ws.run("//app:img").await?;
    let build = fake
        .calls()
        .into_iter()
        .find(|c| c.contains("buildx build"))
        .expect("a buildx build call");
    assert!(
        build.contains("--file") && build.contains("generated.Dockerfile"),
        "the build must read the dep's Dockerfile, got: {build}"
    );

    Ok(())
}

/// A multi-stage, multi-arch image: the stage and the whole platform list reach
/// the builder, and two targets that differ only by stage are two cache entries.
/// Collapsing them would serve one stage's image under the other's name.
#[tokio::test]
async fn test_oci_image_multi_stage_multi_arch() -> anyhow::Result<()> {
    let fake = Fake::new();
    let ws = workspace_with_fake(&fake);
    ws.write_build_file(
        "app",
        r#"
target(
    name = "dockerfile",
    driver = "bash",
    run = "printf 'FROM scratch AS build\nFROM scratch AS runtime\n' > $OUT",
    out = "Dockerfile",
)
target(
    name = "runtime",
    driver = "oci_image",
    context = [":dockerfile"],
    stage = "runtime",
    platforms = ["linux/amd64", "linux/arm64"],
    out = "runtime.tar",
)
target(
    name = "build",
    driver = "oci_image",
    context = [":dockerfile"],
    stage = "build",
    platforms = ["linux/amd64", "linux/arm64"],
    out = "build.tar",
)
"#,
    );

    ws.run("//app:runtime").await?;
    let call = fake
        .calls()
        .into_iter()
        .find(|c| c.contains("buildx build"))
        .expect("a buildx build call");
    assert!(
        call.contains("--platform linux/amd64,linux/arm64"),
        "the whole platform list must reach the builder, got: {call}"
    );
    assert!(
        call.contains("--target runtime"),
        "the stage must reach the builder as --target, got: {call}"
    );

    // Same context, same platforms, different stage: a second build, not a hit.
    ws.run("//app:build").await?;
    assert_eq!(
        fake.builds(),
        2,
        "a different stage is a different image and must not hit the first one's entry"
    );
    let stages: Vec<String> = fake
        .calls()
        .into_iter()
        .filter(|c| c.contains("buildx build"))
        .collect();
    assert!(stages[1].contains("--target build"), "got: {}", stages[1]);
    Ok(())
}

/// A failing build fails the target, and the builder's own message survives to
/// the user. The archive never becomes an artifact, so a broken build cannot
/// poison the cache.
#[tokio::test]
async fn test_oci_image_build_failure_surfaces_the_builder_error() -> anyhow::Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("docker");
    hcore::fsutil::write_executable(
        &bin,
        b"#!/bin/sh\ncase \"$2\" in inspect) echo 'Platforms: linux/amd64'; exit 0;; esac\n\
          echo 'ERROR: failed to solve: unknown instruction: FROOM' >&2\nexit 1\n",
    )
    .expect("write fake");

    let ws = WorkspaceBuilder::new()
        .expect("workspace tempdir")
        .with_provider(|init| {
            Box::new(heph::pluginbuildfile::Provider::new(
                init.root.to_path_buf(),
                init.runtime.clone(),
            ))
        })
        .with_managed_driver(Box::new(heph::pluginexec::Driver::new_bash()))
        .with_managed_driver(Box::new(pluginoci::Driver::with_binary(
            bin.to_string_lossy().into_owned(),
        )))
        .with_provider(|_| Box::new(pluginoci::platform::Provider))
        .with_managed_driver(Box::new(pluginoci::platform::Driver::with_binary(
            bin.to_string_lossy().into_owned(),
        )))
        .build()
        .expect("build workspace");

    ws.write_build_file(
        "bad",
        r#"
target(
    name = "dockerfile",
    driver = "bash",
    run = "echo 'FROOM scratch' > $OUT",
    out = "Dockerfile",
)
target(name = "img", driver = "oci_image", context = [":dockerfile"])
"#,
    );

    let err = ws
        .run("//bad:img")
        .await
        .err()
        .expect("a failing build must fail the target");
    let msg = format!("{err:#}");
    assert!(msg.contains("failed to solve"), "got: {msg}");
    Ok(())
}
