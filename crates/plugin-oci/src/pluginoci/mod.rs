//! The OCI/container rules, and the pieces they share.
//!
//! Five drivers, split by what they need from the host:
//!
//! - [`docker_build`] — builds an image from a **Dockerfile**, by shelling out to
//!   `docker buildx`. Needs a daemon, can `RUN`, and is not fully hermetic (see
//!   its module docs for the list of what is deliberately not an input).
//! - [`platform`] — probes the buildx builder's default platform, so
//!   [`docker_build`] can fold it into the cache key. Exists only for that.
//! - [`pull`] / [`push`] — speak the OCI distribution protocol in-process (see
//!   [`registry`]). No docker, on any host.
//! - [`load`] — hands an image to a local docker daemon, which is the whole
//!   point of it, so it talks to one by definition (through the API, not the CLI).
//!
//! What lives here rather than in one of those: the pieces more than one of them
//! needs — the archive format enum, platform-string handling, and the helpers
//! for reading what a dep materialized into the sandbox.

use anyhow::Context as _;
use hdriver_support::driver_managed::ManagedRunRequest;
use hplugin::driver::TargetAddr;
use std::path::{Path, PathBuf};

pub mod archive;
pub mod docker_build;
pub mod image;
pub mod index;
pub mod layer;
pub mod load;
pub mod platform;
pub mod pull;
pub mod push;
pub mod registry;

/// Archive format an image is built/consumed as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum ImageFormat {
    /// OCI image layout archive (`--output type=oci`). Portable, standard;
    /// pushed and pulled daemonlessly over the registry protocol.
    Oci,
    /// Docker-format image archive (`--output type=docker`). Loadable straight
    /// into a docker daemon with `docker load`.
    Docker,
}

impl ImageFormat {
    /// The BuildKit `--output type=` value.
    fn output_type(self) -> &'static str {
        match self {
            ImageFormat::Oci => "oci",
            ImageFormat::Docker => "docker",
        }
    }

    pub(crate) fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "oci" => Ok(ImageFormat::Oci),
            "docker" => Ok(ImageFormat::Docker),
            other => anyhow::bail!("`format` must be \"oci\" or \"docker\", got {other:?}"),
        }
    }
}
/// Parse the image ref/id `docker load` printed to stdout, e.g.
/// `Loaded image: alpine:latest` or `Loaded image ID: sha256:abc…`. Takes the
/// last such line (a docker archive may load several).
pub(crate) fn parse_docker_load_ref(stdout: &str) -> anyhow::Result<String> {
    for line in stdout.lines().rev() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Loaded image ID:") {
            return Ok(rest.trim().to_string());
        }
        if let Some(rest) = line.strip_prefix("Loaded image:") {
            return Ok(rest.trim().to_string());
        }
    }
    anyhow::bail!("no `Loaded image` line in docker load output: {stdout:?}")
}

/// Pin an image dep to the archive output group (`""`). An explicit group is
/// rejected rather than honoured: every other group on a `docker_build` target
/// (`digest`) is a text file, and handing the archive reader a text file where
/// it expects a layout fails deep inside the parse.
pub(crate) fn pin_archive_group(
    image_ref: &mut TargetAddr,
    spec_value: &str,
) -> anyhow::Result<()> {
    match image_ref.output.as_deref() {
        None => {
            image_ref.output = Some(String::new());
            Ok(())
        }
        Some("") => Ok(()),
        Some(group) => anyhow::bail!(
            "`image` {spec_value:?} selects output group {group:?}; this driver consumes the \
             image archive, which is the default group. Drop the `|{group}` selector."
        ),
    }
}
/// Pick the image manifest for `platform` out of a layout.
///
/// Goes through [`archive::Layout::manifests`] rather than the top-level index,
/// because a buildx multi-platform image nests one index inside another and
/// records the platform on the *inner* entry.
///
/// A single-image layout has nothing to choose and is returned as-is — asking a
/// one-platform archive for `linux/amd64` and failing because its index carries
/// no platform annotation would be pedantry, not safety.
pub(crate) fn select_platform(
    layout: &archive::Layout,
    platform: &str,
) -> anyhow::Result<(oci_client::manifest::OciImageManifest, String)> {
    let manifests = layout.manifests()?;
    anyhow::ensure!(
        !manifests.is_empty(),
        "the image layout holds no manifests; there is nothing to load"
    );
    if manifests.len() == 1 {
        let (m, _, digest) = manifests.into_iter().next().expect("checked non-empty");
        return Ok((m, digest));
    }

    let (os, arch) = split_platform(platform)?;
    let mut available = Vec::new();
    for (manifest, p, digest) in manifests {
        let Some(p) = p else { continue };
        available.push(format!("{}/{}", p.os, p.architecture));
        if p.os.to_string() == os && p.architecture.to_string() == arch {
            return Ok((manifest, digest));
        }
    }
    anyhow::bail!(
        "the image has no {platform} instance (it has: {}). A daemon tag holds one image, so \
         `platform` has to name one the archive actually contains.",
        available.join(", ")
    )
}

/// Whether an attribute value names a target rather than a path.
///
/// The same two prefixes every other target-valued attribute here accepts
/// (`context`, `bases`, `image`): `//pkg:name` absolute, `:name` in this
/// package. No path worth writing starts with either.
pub(crate) fn is_addr(value: &str) -> bool {
    value.starts_with("//") || value.starts_with(':')
}

/// Join a package-relative path onto a (possibly empty) package prefix, yielding
/// a workspace-relative path.
pub(crate) fn ws_path(pkg: &str, rel: &str) -> String {
    if pkg.is_empty() {
        rel.to_string()
    } else {
        format!("{pkg}/{rel}")
    }
}

/// The platform a pull or load resolves to when the BUILD file does not name
/// one: Linux on the host's own architecture.
///
/// The OS is pinned to `linux` rather than taken from the host because container
/// images are Linux images: on macOS, asking a manifest list for a `darwin`
/// instance finds nothing, while the daemon's own default would quietly be
/// `linux` from inside its VM. Same target, two different answers, one an error.
pub(crate) fn default_platform() -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("linux/{arch}")
}

/// Split `os/arch` into its parts.
/// Normalize a platform string to the spelling BuildKit and containerd use.
///
/// The same platform has several accepted spellings — `linux/x86_64` and
/// `linux/amd64` are the same machine, `linux/arm64/v8` and `linux/arm64` the
/// same CPU — and every one of them was previously carried verbatim into
/// `--platform`, into the cache key, and (once `context_by_platform` stages by
/// platform) into a directory name. Two spellings therefore meant two cache
/// entries for one image, and a staged directory a Dockerfile expanding
/// `TARGETPLATFORM` could never find.
///
/// This is a deliberate subset of containerd's rules — the aliases people
/// actually type, plus the redundant default variant — not a reimplementation
/// of its full table. An unknown os/arch passes through lowercased rather than
/// being rejected: BuildKit accepts platforms heph has never heard of, and
/// guessing wrong here would be worse than carrying the user's own word.
pub(crate) fn normalize_platform(platform: &str) -> anyhow::Result<String> {
    let lower = platform.to_ascii_lowercase();
    let (os, arch) = split_platform(&lower)?;
    let variant = lower.splitn(3, '/').nth(2).unwrap_or("");

    let arch = match arch {
        "x86_64" | "x86-64" => "amd64",
        "aarch64" => "arm64",
        "i386" | "i686" | "x86" => "386",
        other => other,
    };
    // containerd treats v8 as arm64's default and drops it; keeping it would
    // make `linux/arm64` and `linux/arm64/v8` two keys for one CPU.
    let variant = if arch == "arm64" && variant == "v8" {
        ""
    } else {
        variant
    };

    Ok(if variant.is_empty() {
        format!("{os}/{arch}")
    } else {
        format!("{os}/{arch}/{variant}")
    })
}

pub(crate) fn split_platform(platform: &str) -> anyhow::Result<(&str, &str)> {
    // A platform may carry a variant (`linux/arm/v7`); only os and arch select
    // an instance out of a manifest list.
    let mut parts = platform.splitn(3, '/');
    match (parts.next(), parts.next()) {
        (Some(os), Some(arch)) if !os.is_empty() && !arch.is_empty() => Ok((os, arch)),
        _ => anyhow::bail!("`platform` must look like `os/arch`, got {platform:?}"),
    }
}
/// All paths a Dep input materialized into the sandbox, read from its `.list`
/// file (one absolute path per line — see `driver_managed.rs::list_path_for`).
fn dep_files(req: &ManagedRunRequest<'_, '_>, origin_id: &str) -> anyhow::Result<Vec<PathBuf>> {
    let Some(m) = req.inputs.iter().find(|m| {
        m.input.origin_id == origin_id
            && matches!(
                m.input.artifact.r#type,
                hplugin::driver::inputartifact::Type::Dep
            )
    }) else {
        return Ok(Vec::new());
    };
    let list_path = m.require_list_path()?;
    let content = std::fs::read_to_string(list_path)
        .with_context(|| format!("read dep list {list_path:?}"))?;
    Ok(content
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect())
}
/// Absolute path of the OCI layout a dep materialized — a layout *directory*,
/// or the single archive file that is the other shape this plugin writes.
///
/// A dep's list names *files*, never the directories holding them, so the root
/// cannot be found by looking for a directory entry. It is found by its marker
/// instead: an OCI layout is exactly a tree with an `oci-layout` file at its
/// root, which is also what buildx's `oci-layout://` wants pointed at.
///
/// `attr` names the BUILD-file attribute in the error, since the same shape is
/// consumed by `base`, `image` and `images`.
pub(crate) fn layout_path(
    req: &ManagedRunRequest<'_, '_>,
    origin: &str,
    attr: &str,
) -> anyhow::Result<PathBuf> {
    let paths = dep_files(req, origin)?;
    anyhow::ensure!(!paths.is_empty(), "{attr} produced no files in the sandbox");
    if let Some(dir) = paths
        .iter()
        .find(|p| p.file_name().is_some_and(|n| n == "oci-layout"))
        .and_then(|p| p.parent())
    {
        return Ok(dir.to_path_buf());
    }
    if let [only] = paths.as_slice() {
        return Ok(only.clone());
    }
    anyhow::bail!(
        "{attr} is neither an OCI layout directory nor a single archive: no `oci-layout` file \
         among {} staged path(s), the first being {:?}. Produce it with \
         `oci_pull(layout = True)`, `oci_image(...)` or `docker_build(...)`.",
        paths.len(),
        paths.first()
    )
}

/// Absolute path to the single file a Dep input materialized into the sandbox.
/// Reads the input's `.list` file (one absolute path per line — see
/// `driver_managed.rs::list_path_for`). Errors unless exactly one file was
/// produced, so a caller expecting one archive fails loudly on a mis-declared
/// dep.
pub(crate) fn dep_single_file(
    req: &ManagedRunRequest<'_, '_>,
    origin_id: &str,
) -> anyhow::Result<PathBuf> {
    let m = req
        .inputs
        .iter()
        .find(|m| {
            m.input.origin_id == origin_id
                && matches!(
                    m.input.artifact.r#type,
                    hplugin::driver::inputartifact::Type::Dep
                )
        })
        .with_context(|| format!("no dep input {origin_id:?} in sandbox"))?;
    let list_path = m.require_list_path()?;
    // The `.list` file holds one absolute materialized path per line.
    let content = std::fs::read_to_string(list_path)
        .with_context(|| format!("read dep list {list_path:?}"))?;
    let mut paths = content.lines().filter(|l| !l.is_empty());
    let first = paths
        .next()
        .with_context(|| format!("dep {origin_id:?} produced no files"))?;
    anyhow::ensure!(
        paths.next().is_none(),
        "dep {origin_id:?} produced more than one file; expected exactly one archive"
    );
    Ok(PathBuf::from(first))
}

fn basename(path: &str) -> anyhow::Result<&std::ffi::OsStr> {
    Path::new(path)
        .file_name()
        .with_context(|| format!("path {path:?} has no file name"))
}
/// Fake-binary scaffolding shared by every driver's `run()` tests.
///
/// The drivers' whole job is to assemble a command and interpret its result, so
/// the interesting behaviour only shows up once something is actually executed.
/// These helpers stand a shell script in for `docker`, record what it
/// was called with, and let a test dictate how it behaves.
#[cfg(test)]
pub(crate) mod testfake {
    use hdriver_support::driver_managed::{ManagedRunInput, ManagedRunRequest};
    use hplugin::driver::targetdef::TargetDef;
    use hplugin::driver::{RunInput, RunRequest, inputartifact, outputartifact};
    use std::path::PathBuf;

    /// A sandbox laid out the way the managed-driver bridge lays one out.
    pub(crate) struct Sandbox {
        pub dir: tempfile::TempDir,
        pub ws: PathBuf,
        pub pkg: PathBuf,
        /// Where fake binaries append one line per invocation.
        pub log: PathBuf,
    }

    impl Sandbox {
        pub(crate) fn new(package: &str) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let ws = dir.path().join("ws");
            let pkg = if package.is_empty() {
                ws.clone()
            } else {
                ws.join(package)
            };
            std::fs::create_dir_all(&pkg).expect("mkdir pkg");
            let log = dir.path().join("calls.log");
            Sandbox { dir, ws, pkg, log }
        }

        /// Install an executable fake at `name`. `body` runs after the call has
        /// been recorded; `$@` is the argv and `$LOG` the record file.
        ///
        /// The log path is derived from `$0` rather than an environment
        /// variable, because the driver clears the child's environment — which
        /// is the point, and which a fake depending on an inherited var would
        /// quietly defeat.
        pub(crate) fn fake(&self, name: &str, body: &str) -> String {
            let path = self.dir.path().join(name);
            let script = format!(
                "#!/bin/sh\nLOG=\"$(dirname \"$0\")/calls.log\"\nprintf '%s' \"{name}\" >> \
                 \"$LOG\"\nfor a in \"$@\"; do printf ' %s' \"$a\" >> \"$LOG\"; done\nprintf \
                 '\\n' >> \"$LOG\"\n{body}\n"
            );
            // Not `fs::write` + `set_permissions`: tests run in parallel, and a
            // sibling test's fork between our create and our exec inherits a
            // writable fd to this file, so the exec fails with `ETXTBSY`.
            // `write_executable` drains those descriptors before returning.
            hcore::fsutil::write_executable(&path, script.as_bytes()).expect("write fake");
            path.to_string_lossy().into_owned()
        }

        /// One entry per invocation, in call order.
        pub(crate) fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    /// Build a `ManagedRunRequest` over `sandbox` for `def`, with one Dep input
    /// per (origin_id, materialized paths) pair.
    /// What the seeded probe dep reports. Tests asserting on `--platform` match
    /// this; a test that needs another platform passes its own probe dep.
    pub(crate) const PROBED_PLATFORM: &str = "linux/arm64";

    pub(crate) fn run_request<'a>(
        request_id: &'a String,
        hashin: &'a str,
        def: &'a TargetDef,
        sandbox: &Sandbox,
        deps: &[(&str, Vec<PathBuf>)],
    ) -> ManagedRunRequest<'a, 'static> {
        let list_dir = sandbox.dir.path().join("lists");
        std::fs::create_dir_all(&list_dir).expect("mkdir lists");

        // Every `docker_build` without explicit `platforms` depends on the probe
        // target, so a runnable request needs its output. Seeded here rather
        // than in each test: it is a precondition of running at all, not
        // something an individual test is making a statement about. A test that
        // wants a specific platform passes its own.
        let mut deps: Vec<(&str, Vec<PathBuf>)> = deps.to_vec();
        if !deps
            .iter()
            .any(|(id, _)| *id == super::docker_build::PLATFORM_ORIGIN)
        {
            let probe = sandbox.dir.path().join("probed-platform.txt");
            std::fs::write(&probe, PROBED_PLATFORM).expect("write probe output");
            deps.push((super::docker_build::PLATFORM_ORIGIN, vec![probe]));
        }

        let mut inputs = Vec::new();
        for (origin_id, paths) in &deps {
            let list_path = list_dir.join(format!("input_{origin_id}.list"));
            let body: String = paths
                .iter()
                .map(|p| format!("{}\n", p.to_string_lossy()))
                .collect();
            std::fs::write(&list_path, body).expect("write list");
            inputs.push(ManagedRunInput {
                input: RunInput {
                    artifact: inputartifact::InputArtifact {
                        r#type: inputartifact::Type::Dep,
                        origin_id: (*origin_id).to_string(),
                        content: std::sync::Arc::new(outputartifact::OutputArtifact {
                            group: String::new(),
                            name: String::new(),
                            r#type: outputartifact::Type::Output,
                            content: outputartifact::Content::Raw(outputartifact::ContentRaw {
                                data: vec![],
                                path: String::new(),
                                x: false,
                            }),
                            hashout: "test".to_string(),
                        }),
                    },
                    origin_id: (*origin_id).to_string(),
                    source_addr: hmodel::htaddr::parse_addr("//test:dep").expect("addr"),
                    filters: vec![],
                    annotations: std::collections::BTreeMap::new(),
                },
                list_path: Some(list_path),
                unpack_root: sandbox.ws.clone(),
            });
        }

        ManagedRunRequest {
            request: RunRequest {
                request_id,
                target: def,
                tree_root_path: sandbox.ws.clone(),
                inputs: vec![],
                hashin,
                stdin: None,
                stdout: None,
                stderr: None,
                sandbox_dir: sandbox.dir.path().to_path_buf(),
            },
            sandbox_dir: sandbox.dir.path().to_path_buf(),
            sandbox_ws_dir: sandbox.ws.clone(),
            sandbox_pkg_dir: sandbox.pkg.clone(),
            inputs,
            runner: std::sync::Arc::new(hexec_runner::LocalSession::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_load_ref_prefers_the_last_loaded_line() {
        assert_eq!(
            parse_docker_load_ref("Loaded image: alpine:latest\n").expect("ref"),
            "alpine:latest"
        );
        assert_eq!(
            parse_docker_load_ref("Loaded image ID: sha256:abc123\n").expect("id"),
            "sha256:abc123"
        );
        assert_eq!(
            parse_docker_load_ref("Loaded image: a:1\nLoaded image: b:2\n").expect("last"),
            "b:2"
        );
        assert!(parse_docker_load_ref("nothing here").is_err());
    }
}
