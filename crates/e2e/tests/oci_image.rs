#![expect(
    clippy::panic_in_result_fn,
    clippy::indexing_slicing,
    reason = "restriction/style lints scoped to production code; tests are exempt. \
              `serde_json::Value`'s Index is infallible — a missing key reads as Null — so \
              indexing a parsed manifest cannot panic the way slicing a Vec can."
)]

//! End-to-end coverage for `oci_layer` + `oci_image`, through the real engine.
//!
//! **Nothing here is gated on docker being installed, and that is the point.**
//! These drivers spawn no process, open no socket and read no host binary — a
//! test that needed a daemon would be testing something else. The `docker_build`
//! suite next door (`oci_docker.rs`) is where a real daemon is required.
//!
//! What only the engine can prove, and so lives here rather than in the
//! plugin's unit tests: that the layer tar really reaches the image target as a
//! collected artifact, that an unchanged input is a cache hit, that changing
//! *only* a config attribute re-keys, and that the layout the driver wrote
//! reads back as an image.

mod common;

use hplugin_oci::pluginoci;
use htestkit::WorkspaceBuilder;
use std::collections::HashMap;

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
        .with_managed_driver(Box::new(pluginoci::image::Driver::new()))
        .with_managed_driver(Box::new(pluginoci::layer::Driver::new()))
        .with_managed_driver(Box::new(pluginoci::index::Driver::new()))
        .build()
        .expect("build workspace")
}

/// Every member of the image's layout tar, by name.
fn layout_members(bytes: &[u8]) -> Vec<String> {
    let mut ar = tar::Archive::new(std::io::Cursor::new(bytes.to_vec()));
    ar.entries()
        .expect("entries")
        .map(|e| {
            e.expect("entry")
                .path()
                .expect("path")
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// The layout's blobs, by digest, read out of the archive.
fn layout_blobs(bytes: &[u8]) -> HashMap<String, Vec<u8>> {
    use std::io::Read as _;
    let mut ar = tar::Archive::new(std::io::Cursor::new(bytes.to_vec()));
    let mut out = HashMap::new();
    for entry in ar.entries().expect("entries") {
        let mut entry = entry.expect("entry");
        let name = entry.path().expect("path").to_string_lossy().into_owned();
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).expect("read");
        if let Some(rest) = name.strip_prefix("blobs/")
            && let Some((algo, hex)) = rest.split_once('/')
        {
            out.insert(format!("{algo}:{hex}"), buf);
        }
    }
    out
}

/// Follow the layout's `index.json` to the one image manifest inside it. The
/// entry point is wrapped in a nested, `ref.name`-annotated index (what buildx's
/// `oci-layout://` needs), so this hops twice.
fn manifest_of(bytes: &[u8]) -> serde_json::Value {
    let blobs = layout_blobs(bytes);
    let mut ar = tar::Archive::new(std::io::Cursor::new(bytes.to_vec()));
    let mut index = None;
    for entry in ar.entries().expect("entries") {
        use std::io::Read as _;
        let mut entry = entry.expect("entry");
        if entry.path().expect("path").to_string_lossy() == "index.json" {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).expect("read");
            index = Some(serde_json::from_slice::<serde_json::Value>(&buf).expect("index"));
        }
    }
    let mut current = index.expect("the layout must have an index.json");
    loop {
        let digest = current["manifests"][0]["digest"]
            .as_str()
            .expect("an index entry")
            .to_string();
        let raw = blobs.get(&digest).expect("the entry's blob");
        let next: serde_json::Value = serde_json::from_slice(raw).expect("json");
        if next.get("layers").is_some() {
            return next;
        }
        current = next;
    }
}

fn config_of(bytes: &[u8]) -> serde_json::Value {
    let manifest = manifest_of(bytes);
    let digest = manifest["config"]["digest"]
        .as_str()
        .expect("config digest");
    serde_json::from_slice(layout_blobs(bytes).get(digest).expect("config blob")).expect("config")
}

const BUILD: &str = r#"
target(name = "bin", driver = "bash", run = "echo elf > $OUT; chmod +x $OUT", out = "server")
target(name = "conf", driver = "bash", run = "echo k=v > $OUT", out = "app.conf")
target(name = "app", driver = "oci_layer", srcs = [":bin"], prefix = "/usr/bin")
target(name = "etc", driver = "oci_layer", srcs = [":conf"], prefix = "/etc")
target(
    name = "img",
    driver = "oci_image",
    layers = [":app", ":etc"],
    platforms = ["linux/amd64"],
    entrypoint = ["/usr/bin/server"],
    env = {"PORT": "8080"},
)
"#;

/// The whole path, with no docker anywhere: two layers assembled into an image
/// whose layout reads back with the config the BUILD file asked for.
#[tokio::test]
async fn test_an_image_is_assembled_from_layers_without_docker() -> anyhow::Result<()> {
    let ws = workspace();
    ws.write_build_file("app", BUILD);

    let result = ws.run("//app:img").await?;
    let bytes = htestkit::artifact_bytes(&result);

    let members = layout_members(&bytes);
    assert!(
        members.contains(&"oci-layout".to_string()) && members.contains(&"index.json".to_string()),
        "an OCI layout needs both markers, got: {members:?}"
    );

    let manifest = manifest_of(&bytes);
    let layers = manifest["layers"].as_array().expect("layers");
    assert_eq!(layers.len(), 2, "one blob per oci_layer target");
    for layer in layers {
        assert_eq!(
            layer["mediaType"], "application/vnd.oci.image.layer.v1.tar",
            "layers are uncompressed, so the digest cannot depend on a deflate backend"
        );
    }

    let config = config_of(&bytes);
    assert_eq!(config["os"], "linux");
    assert_eq!(config["architecture"], "amd64");
    assert_eq!(
        config["config"]["Entrypoint"],
        serde_json::json!(["/usr/bin/server"])
    );
    assert_eq!(config["config"]["Env"], serde_json::json!(["PORT=8080"]));
    assert!(
        config.get("created").is_none(),
        "a wall clock in the config would re-digest the image on every build"
    );

    // Uncompressed layers: the diff_id *is* the layer digest, and nothing else
    // would make that true.
    let diff_ids = config["rootfs"]["diff_ids"].as_array().expect("diff_ids");
    let digests: Vec<&serde_json::Value> = layers.iter().map(|l| &l["digest"]).collect();
    assert_eq!(
        diff_ids.iter().collect::<Vec<_>>(),
        digests,
        "with no compression the diff_id and the layer digest are one value"
    );
    Ok(())
}

/// Same inputs, same bytes. This is the claim the rule is built on, and the one
/// that a wall clock, a map iteration order or a `read_dir` order would break —
/// none of which a single run can catch.
#[tokio::test]
async fn test_the_same_inputs_produce_the_same_image() -> anyhow::Result<()> {
    let first = {
        let ws = workspace();
        ws.write_build_file("app", BUILD);
        let r = ws.run("//app:img").await?;
        htestkit::artifact_bytes(&r)
    };
    // A second workspace: a different absolute path, a different sandbox, a
    // cold cache. Re-running in one workspace would prove almost nothing.
    let second = {
        let ws = workspace();
        ws.write_build_file("app", BUILD);
        let r = ws.run("//app:img").await?;
        htestkit::artifact_bytes(&r)
    };
    assert_eq!(
        first, second,
        "two workspaces, same declared inputs — the image must be byte-identical"
    );
    Ok(())
}

/// A config attribute changes the image even though it touches no input file.
///
/// Paired with the reproducibility test above, this is what pins the def hash
/// from both sides: identical attributes must give identical bytes, and a
/// changed attribute must give different ones. (That `env` reaches the *key* is
/// asserted directly in `plugin-oci`'s def-hash test; here the two workspaces
/// are independent, so a shared cache entry could not hide the difference.)
#[tokio::test]
async fn test_an_env_var_changes_the_image() -> anyhow::Result<()> {
    let image_with = |env: &str| {
        let build = BUILD.replace(r#""8080""#, &format!(r#""{env}""#));
        async move {
            let ws = workspace();
            ws.write_build_file("app", &build);
            let r = ws.run("//app:img").await?;
            anyhow::Ok(htestkit::artifact_bytes(&r))
        }
    };
    let before = image_with("8080").await?;
    let after = image_with("9090").await?;

    assert_eq!(
        config_of(&before)["config"]["Env"],
        serde_json::json!(["PORT=8080"])
    );
    assert_eq!(
        config_of(&after)["config"]["Env"],
        serde_json::json!(["PORT=9090"])
    );
    assert_ne!(before, after, "a changed attribute must change the image");
    Ok(())
}

/// An image built on another image: the base's layer sits underneath, and its
/// config is inherited rather than replaced.
///
/// Dropping a base's `PATH` is the easiest way to ship an image that starts and
/// then cannot find its own entrypoint, so the inheritance is asserted through
/// the real thing — a layout written by one target and read by the next — not
/// only against a hand-built config map.
#[tokio::test]
async fn test_an_image_inherits_its_base() -> anyhow::Result<()> {
    let ws = workspace();
    ws.write_build_file(
        "app",
        r#"
target(name = "libc", driver = "bash", run = "echo so > $OUT", out = "libc.so")
target(name = "base_layer", driver = "oci_layer", srcs = [":libc"], prefix = "/lib")
target(
    name = "base",
    driver = "oci_image",
    layers = [":base_layer"],
    platforms = ["linux/amd64"],
    layout = True,
    env = {"PATH": "/usr/bin", "LANG": "C"},
    cmd = ["--base-arg"],
)

target(name = "bin", driver = "bash", run = "echo elf > $OUT; chmod +x $OUT", out = "server")
target(name = "app", driver = "oci_layer", srcs = [":bin"], prefix = "/usr/bin")
target(
    name = "img",
    driver = "oci_image",
    base = ":base",
    layers = [":app"],
    platforms = ["linux/amd64"],
    entrypoint = ["/usr/bin/server"],
    env = {"PORT": "8080"},
)
"#,
    );

    let r = ws.run("//app:img").await?;
    let bytes = htestkit::artifact_bytes(&r);

    let manifest = manifest_of(&bytes);
    let layers = manifest["layers"].as_array().expect("layers");
    assert_eq!(layers.len(), 2, "the base's layer plus this image's own");

    let config = config_of(&bytes);
    assert_eq!(
        config["config"]["Env"],
        serde_json::json!(["LANG=C", "PATH=/usr/bin", "PORT=8080"]),
        "the base's variables must survive alongside this image's"
    );
    assert_eq!(
        config["config"]["Entrypoint"],
        serde_json::json!(["/usr/bin/server"])
    );
    assert!(
        config["config"].get("Cmd").is_none(),
        "setting entrypoint must clear the base's Cmd: those arguments were \
         meant for a different program, got {config}"
    );
    assert_eq!(
        config["rootfs"]["diff_ids"]
            .as_array()
            .expect("diff_ids")
            .len(),
        2,
        "diff_ids must cover the base's layers as well as this image's"
    );
    Ok(())
}

/// Two platforms sharing a layer store one blob, not two. buildx cannot do this
/// at all — it builds each platform separately — so it is worth freezing.
#[tokio::test]
async fn test_platforms_share_one_blob_for_a_shared_layer() -> anyhow::Result<()> {
    let ws = workspace();
    ws.write_build_file(
        "app",
        r#"
target(name = "conf", driver = "bash", run = "echo k=v > $OUT", out = "app.conf")
target(name = "etc", driver = "oci_layer", srcs = [":conf"], prefix = "/etc")
target(
    name = "img",
    driver = "oci_image",
    layers = [":etc"],
    platforms = ["linux/amd64", "linux/arm64"],
)
"#,
    );

    let r = ws.run("//app:img").await?;
    let bytes = htestkit::artifact_bytes(&r);
    let blobs = layout_blobs(&bytes);
    // Both instances name the same layer digest, so the layer appears once.
    let layer_blobs: Vec<&String> = blobs
        .iter()
        .filter(|(_, v)| v.starts_with(b"etc/app.conf") || v.len() > 512)
        .map(|(k, _)| k)
        .collect();
    assert!(
        !layer_blobs.is_empty(),
        "the layer blob must be in the layout"
    );

    let mut ar = tar::Archive::new(std::io::Cursor::new(bytes.clone()));
    let names: Vec<String> = ar
        .entries()
        .expect("entries")
        .map(|e| {
            e.expect("entry")
                .path()
                .expect("path")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        names.len(),
        sorted.len(),
        "a layout must not carry the same blob twice: {names:?}"
    );
    Ok(())
}

/// `format = "docker"` writes the hybrid shape a daemon's `docker load` takes:
/// the OCI layout plus a `manifest.json` naming the config and layers by their
/// blob paths.
///
/// Not gated on docker, deliberately — the archive's *shape* is what this
/// driver owns, and gating it would leave the docker-format path covered only
/// by a suite that skips on most machines.
#[tokio::test]
async fn test_the_docker_format_archive_has_a_manifest_json() -> anyhow::Result<()> {
    let ws = workspace();
    ws.write_build_file(
        "app",
        r#"
target(name = "conf", driver = "bash", run = "echo k=v > $OUT", out = "app.conf")
target(name = "etc", driver = "oci_layer", srcs = [":conf"], prefix = "/etc")
target(
    name = "img",
    driver = "oci_image",
    layers = [":etc"],
    platforms = ["linux/amd64"],
    format = "docker",
)
"#,
    );

    let r = ws.run("//app:img").await?;
    let bytes = htestkit::artifact_bytes(&r);
    let members = layout_members(&bytes);
    assert!(
        members.contains(&"manifest.json".to_string()),
        "a docker-format archive needs manifest.json, got: {members:?}"
    );

    use std::io::Read as _;
    let mut ar = tar::Archive::new(std::io::Cursor::new(bytes.clone()));
    let mut manifest = None;
    for entry in ar.entries().expect("entries") {
        let mut entry = entry.expect("entry");
        if entry.path().expect("path").to_string_lossy() == "manifest.json" {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).expect("read");
            manifest = Some(serde_json::from_slice::<serde_json::Value>(&buf).expect("json"));
        }
    }
    let manifest = manifest.expect("manifest.json");
    let layers = manifest[0]["Layers"].as_array().expect("Layers");
    assert_eq!(layers.len(), 1);
    let layer_path = layers[0].as_str().expect("layer path");
    assert!(
        members.contains(&layer_path.to_string()),
        "manifest.json names {layer_path}, which must be in the archive: {members:?}"
    );
    assert!(
        members.contains(&manifest[0]["Config"].as_str().expect("Config").to_string()),
        "manifest.json's Config must be in the archive too"
    );
    Ok(())
}

/// A layer whose `strip` matched nothing is the failure this rule is most
/// likely to produce and least likely to be noticed: the image builds, pushes
/// and starts, and dies when something execs the binary that is not there. It
/// has to fail the build, naming what the sources did produce.
#[tokio::test]
async fn test_an_empty_layer_fails_and_names_what_was_produced() {
    let ws = workspace();
    ws.write_build_file(
        "app",
        r#"
target(name = "bin", driver = "bash", run = "echo elf > $OUT", out = "server")
target(
    name = "app",
    driver = "oci_layer",
    srcs = [":bin"],
    prefix = "/usr/bin",
    strip = "nowhere",
)
"#,
    );
    let err = format!(
        "{:#}",
        ws.run("//app:app")
            .await
            .err()
            .expect("empty layer must fail")
    );
    assert!(err.contains("empty"), "got: {err}");
    assert!(
        err.contains("app/server"),
        "the error must name what the srcs produced: {err}"
    );
}

/// `platforms` has no default on purpose: it is a label written into the config
/// and nothing checks it against the layers, so a host-derived default would
/// ship an amd64 binary in an arm64 image on one machine and not the other.
#[tokio::test]
async fn test_platforms_is_required() {
    let ws = workspace();
    ws.write_build_file(
        "app",
        r#"
target(name = "conf", driver = "bash", run = "echo x > $OUT", out = "app.conf")
target(name = "etc", driver = "oci_layer", srcs = [":conf"], prefix = "/etc")
target(name = "img", driver = "oci_image", layers = [":etc"])
"#,
    );
    let err = format!(
        "{:#}",
        ws.run("//app:img")
            .await
            .err()
            .expect("missing platforms must fail")
    );
    assert!(err.contains("`platforms` is required"), "got: {err}");
}

/// The digest group is a real output a downstream target can read without
/// unpacking the archive — parity with `docker_build`, whose consumers rely on
/// it.
#[tokio::test]
async fn test_the_digest_group_is_consumable() -> anyhow::Result<()> {
    let ws = workspace();
    ws.write_build_file(
        "app",
        &format!(
            r#"{BUILD}
target(
    name = "show",
    driver = "bash",
    run = "cat $SRC_DIGEST > $OUT",
    out = "out.txt",
    deps = {{"digest": ["//app:img|digest"]}},
)
"#
        ),
    );
    let r = ws.run("//app:show").await?;
    let digest = htestkit::artifact_string(&r);
    assert!(
        digest.trim().starts_with("sha256:") && digest.trim().len() == 71,
        "the digest group must carry a sha256 digest, got: {digest:?}"
    );

    // …and it is the manifest the layout actually filed.
    let r = ws.run("//app:img").await?;
    let bytes = htestkit::artifact_bytes(&r);
    let manifest_bytes = serde_json::to_vec(&manifest_of(&bytes)).expect("re-encode");
    assert!(
        layout_blobs(&bytes)
            .get(digest.trim())
            .is_some_and(|b| b.len() == manifest_bytes.len()),
        "the reported digest must name a manifest in the layout"
    );
    Ok(())
}

/// The image digest, frozen as a constant.
///
/// CI runs this suite natively on `x86_64-unknown-linux-gnu`,
/// `aarch64-unknown-linux-gnu` and `aarch64-apple-darwin`, so one shared
/// constant turns those three jobs into the cross-platform reproducibility
/// guarantee the rule's docs claim. Two runs on one machine cannot prove that:
/// they share a filesystem, a umask and an architecture, which is exactly where
/// the interesting divergences live.
///
/// If this changes, the emitted bytes changed. That is either a bug or a
/// deliberate format change — and a deliberate one needs `OCI_IMAGE_FORMAT_VERSION`
/// (or `OCI_LAYER_FORMAT_VERSION`) bumped in the same commit, or every cached
/// image in every workspace keeps its old key with new meaning.
#[tokio::test]
async fn test_the_image_digest_is_the_same_on_every_supported_target() -> anyhow::Result<()> {
    let ws = workspace();
    ws.write_build_file("app", BUILD);
    let r = ws.run("//app:img").await?;
    let manifest = manifest_of(&htestkit::artifact_bytes(&r));
    let raw = serde_json::to_vec(&manifest).expect("re-encode");
    let digest = format!("sha256:{:x}", <sha2::Sha256 as sha2::Digest>::digest(&raw));
    assert_eq!(
        digest, "sha256:729f38276dc0b50b996908f034e3c25516cf33d0d12e83946b0a750dca744e2b",
        "the image manifest's bytes changed. If that was deliberate, bump \
         OCI_IMAGE_FORMAT_VERSION (or OCI_LAYER_FORMAT_VERSION) in the same commit — \
         otherwise every cached image keeps its old key with new meaning. If it was not \
         deliberate, something host-dependent reached the bytes."
    );
    Ok(())
}

/// `oci_index` groups images built *separately* into one multi-platform image.
///
/// This is the shape `docker_build` cannot express on its own: one buildx
/// invocation means one Dockerfile for every platform, so platforms needing
/// genuinely different recipes have nowhere to put the difference. Here each
/// platform is its own target with its own layers, and the index makes them one
/// image from the repo's point of view.
///
/// Built out of `oci_image` rather than `docker_build` so it needs no daemon —
/// the grouping is the same either way, and the driver takes any layout.
#[tokio::test]
async fn test_an_index_groups_separately_built_images() -> anyhow::Result<()> {
    let ws = workspace();
    ws.write_build_file(
        "app",
        r#"
target(name = "bin_amd64", driver = "bash", run = "echo amd > $OUT", out = "server-amd64")
target(name = "bin_arm64", driver = "bash", run = "echo arm > $OUT", out = "server-arm64")
target(name = "l_amd64", driver = "oci_layer", srcs = [":bin_amd64"], prefix = "/usr/bin")
target(name = "l_arm64", driver = "oci_layer", srcs = [":bin_arm64"], prefix = "/usr/bin")

# Deliberately different per platform: different layers, different entrypoint,
# different env. One `docker_build` could not produce both.
target(
    name = "amd64",
    driver = "oci_image",
    layers = [":l_amd64"],
    platforms = ["linux/amd64"],
    entrypoint = ["/usr/bin/server-amd64"],
    env = {"ARCH": "amd64"},
)
target(
    name = "arm64",
    driver = "oci_image",
    layers = [":l_arm64"],
    platforms = ["linux/arm64"],
    entrypoint = ["/usr/bin/server-arm64"],
)

target(name = "img", driver = "oci_index", images = [":amd64", ":arm64"])
"#,
    );

    let r = ws.run("//app:img").await?;
    let bytes = htestkit::artifact_bytes(&r);

    // The entry point is a manifest *list* naming both platforms — not the
    // single manifest each input held.
    let blobs = layout_blobs(&bytes);
    let mut ar = tar::Archive::new(std::io::Cursor::new(bytes.clone()));
    let mut index = None;
    for entry in ar.entries().expect("entries") {
        use std::io::Read as _;
        let mut entry = entry.expect("entry");
        if entry.path().expect("path").to_string_lossy() == "index.json" {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).expect("read");
            index = Some(serde_json::from_slice::<serde_json::Value>(&buf).expect("index"));
        }
    }
    // `write_layout_*` wraps a multi-image set in a nested, ref.name-annotated
    // index so buildx can resolve it; the list is one hop in.
    let outer = index.expect("index.json");
    let inner_digest = outer["manifests"][0]["digest"]
        .as_str()
        .expect("entry digest");
    let list: serde_json::Value =
        serde_json::from_slice(blobs.get(inner_digest).expect("list blob")).expect("list");
    let manifests = list["manifests"].as_array().expect("manifests");
    assert_eq!(manifests.len(), 2, "one entry per grouped image");

    let mut seen: Vec<String> = manifests
        .iter()
        .map(|m| {
            format!(
                "{}/{}",
                m["platform"]["os"].as_str().unwrap_or("?"),
                m["platform"]["architecture"].as_str().unwrap_or("?")
            )
        })
        .collect();
    seen.sort();
    assert_eq!(seen, vec!["linux/amd64", "linux/arm64"]);

    // Each entry still points at the image its own target built, config and all
    // — nothing was rebuilt or merged.
    for m in manifests {
        let manifest: serde_json::Value = serde_json::from_slice(
            blobs
                .get(m["digest"].as_str().expect("digest"))
                .expect("blob"),
        )
        .expect("manifest");
        let config: serde_json::Value = serde_json::from_slice(
            blobs
                .get(
                    manifest["config"]["digest"]
                        .as_str()
                        .expect("config digest"),
                )
                .expect("config blob"),
        )
        .expect("config");
        let arch = config["architecture"].as_str().expect("arch");
        assert_eq!(
            config["config"]["Entrypoint"],
            serde_json::json!([format!("/usr/bin/server-{arch}")]),
            "each platform keeps the entrypoint its own target set"
        );
    }
    Ok(())
}

/// Two images claiming one platform would silently shadow each other, and which
/// one shipped would depend on the order `images` happens to be written in.
#[tokio::test]
async fn test_two_images_for_one_platform_is_an_error() {
    let ws = workspace();
    ws.write_build_file(
        "app",
        r#"
target(name = "a", driver = "bash", run = "echo a > $OUT", out = "a.txt")
target(name = "b", driver = "bash", run = "echo b > $OUT", out = "b.txt")
target(name = "la", driver = "oci_layer", srcs = [":a"], prefix = "/etc")
target(name = "lb", driver = "oci_layer", srcs = [":b"], prefix = "/etc")
target(name = "ia", driver = "oci_image", layers = [":la"], platforms = ["linux/amd64"])
target(name = "ib", driver = "oci_image", layers = [":lb"], platforms = ["linux/amd64"])
target(name = "img", driver = "oci_index", images = [":ia", ":ib"])
"#,
    );
    let err = format!(
        "{:#}",
        ws.run("//app:img")
            .await
            .err()
            .expect("duplicate platforms must fail")
    );
    assert!(err.contains("linux/amd64"), "must name the platform: {err}");
    assert!(
        err.contains("//app:ia") && err.contains("//app:ib"),
        "must name both images: {err}"
    );
}

/// The grouped image is consumable downstream exactly like any other: the
/// digest group is there, and the layout reads back as a multi-platform image.
#[tokio::test]
async fn test_a_grouped_image_reads_back_as_one_image() -> anyhow::Result<()> {
    let ws = workspace();
    ws.write_build_file(
        "app",
        r#"
target(name = "a", driver = "bash", run = "echo a > $OUT", out = "a.txt")
target(name = "la", driver = "oci_layer", srcs = [":a"], prefix = "/etc")
target(name = "ia", driver = "oci_image", layers = [":la"], platforms = ["linux/amd64"])
target(name = "ib", driver = "oci_image", layers = [":la"], platforms = ["linux/arm64"])
target(name = "img", driver = "oci_index", images = [":ia", ":ib"], layout = True)
target(
    name = "show",
    driver = "bash",
    run = "cat $SRC_DIGEST > $OUT",
    out = "out.txt",
    deps = {"digest": ["//app:img|digest"]},
)
"#,
    );
    let r = ws.run("//app:show").await?;
    let digest = htestkit::artifact_string(&r);
    assert!(
        digest.trim().starts_with("sha256:") && digest.trim().len() == 71,
        "the digest group must carry the manifest list digest, got: {digest:?}"
    );

    // Both platforms share one layer, so the blob is stored once.
    let r = ws.run("//app:img").await?;
    let paths = htestkit::artifact_paths(&r);
    let layer_blobs: Vec<_> = paths
        .iter()
        .filter(|p| p.to_string_lossy().contains("blobs/sha256/"))
        .collect();
    let unique: std::collections::BTreeSet<_> = layer_blobs.iter().collect();
    assert_eq!(
        layer_blobs.len(),
        unique.len(),
        "a layout must not carry the same blob twice: {layer_blobs:?}"
    );
    Ok(())
}
