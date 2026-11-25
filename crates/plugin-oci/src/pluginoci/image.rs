//! The `oci_image` driver: assembles an image from target outputs. No
//! Dockerfile, no BuildKit, no docker daemon, no execution of any kind.
//!
//! Use [`super::docker_build`] when the image needs to *run* commands — package
//! installs, compilers, a multi-stage build. This rule is for the other case,
//! which is most of what a build system ships: a static binary and a few files
//! on top of a base.
//!
//! # Why this is the default
//!
//! Not because it avoids a daemon, though it does. Because it is the only image
//! rule whose cache key can cover what the build reads.
//!
//! `docker_build` says so itself: the `docker`/`buildx` version is not hashed,
//! `FROM` is resolved by BuildKit from the network, `RUN` fetches whatever it
//! fetches, and secret *values* cannot be hashed. Here there is nothing to
//! exempt — no host binary, no subprocess, no env var, no run-time network. The
//! inputs are exactly the declared deps and the attributes below.
//!
//! That claim stops at this rule's own boundary, and the docs should not
//! overreach: a `base` is only as hermetic as the `oci_pull` that produced it,
//! and `oci_pull` keys on the ref *string*. Pin bases by `@sha256:` for the full
//! property.
//!
//! The second consequence is cross-architecture builds. A layer is a file tree,
//! so nothing has to *execute* for the target architecture — an arm64 mac emits
//! `linux/amd64` and `linux/arm64` images from one run, with no QEMU, and the
//! same digests CI produces.
//!
//! # Shape
//!
//! Layers come from [`super::layer`] targets, in order, on top of an optional
//! `base`. Output is the same OCI layout `docker_build` emits — a single
//! archive, or with `layout = True` the directory shape `bases` and buildx's
//! `oci-layout://` consume — plus a `digest` group holding the image digest as
//! text.
//!
//! Blobs shared between platforms are stored once: two architectures sharing a
//! static-asset layer reference one blob, which buildx cannot do at all.

use anyhow::Context as _;
use async_trait::async_trait;
use hcore::debug_hash::DebugHasher;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path as OutPath};
use hplugin::driver::targetdef::{Input, InputMode, Output, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, TargetAddr,
};
use hplugin::htspec::{Spec, TargetSpecCache};
use oci_client::manifest::ImageIndexEntry;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

use super::archive::{self, Blob, Blobs};
use super::layer::{DESCRIPTOR_SUFFIX, GZIP_MAGIC, LayerDescriptor, LayerEncoding};
use super::{
    ImageFormat, dep_files, is_os_arch, layout_path, normalize_platform, split_platform, ws_path,
};

const CONFIG_MEDIA_TYPE: &str = "application/vnd.oci.image.config.v1+json";

const BASE_ORIGIN: &str = "base";

fn layer_origin(platform: Option<&str>, i: usize) -> String {
    match platform {
        None => format!("layers|{i}"),
        Some(p) => format!("layers_by_platform|{}|{i}", p.replace('/', "_")),
    }
}

/// Assembles an image from target outputs — no Dockerfile, no docker, no
/// execution. Use `docker_build` when the image needs to run commands (`RUN`,
/// package installs, multi-stage).
#[derive(Spec)]
struct OciImageSpec {
    /// Base image: an `oci_pull(layout = True)` (or another `oci_image`) target.
    /// Omitted, the image starts from scratch.
    ///
    /// Its layers go under this target's, and its config is inherited — `Env`,
    /// `Entrypoint`, `Cmd`, `User`, `WorkingDir`, `Labels`, `ExposedPorts` —
    /// with anything set here winning. Dropping a base's `PATH` silently is the
    /// single easiest way to ship an image that starts and then cannot find its
    /// own entrypoint, so inheritance is on and explicit.
    ///
    /// For a multi-platform build the base needs an instance per platform:
    /// `oci_pull(layout = True, all_platforms = True)`.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    base: Option<String>,
    /// `oci.layer` targets, bottom to top: one address or a list of them.
    /// **Order matters**: a later layer's file shadows the same path in an
    /// earlier one.
    ///
    /// Only `oci.layer` targets, or a `group` re-exporting exactly one: any
    /// other target (a binary, a tar written by a script) fails the image
    /// build, naming it. Its output would otherwise be filed as a layer blob as
    /// it is, and a runtime rejects that only at deploy.
    ///
    /// The check runs when the image target *runs*, not at query or
    /// `heph validate`: it reads what the dep produced. The fix is an
    /// `oci.layer` over the dep: wrap its files (`srcs = [...]`,
    /// `prefix = ...`), or, when it produces a prebuilt tar, adopt it as it is
    /// (`tar = ...`).
    layers: Vec<String>,
    /// Layers that differ per platform, appended after `layers`, keyed by the
    /// platform they belong to. Each value is one `oci.layer` address or a
    /// list of them.
    ///
    /// ```python
    /// platforms = ["linux/amd64", "linux/arm64"],
    /// layers_by_platform = {
    ///     "linux/amd64": [":app_amd64"],
    ///     "linux/arm64": [":app_arm64"],
    /// },
    /// ```
    ///
    /// Every platform in `platforms` must have an entry, and vice versa — a
    /// missing one would silently ship an image without its binary.
    layers_by_platform: BTreeMap<String, Vec<String>>,
    /// Target platforms, e.g. `["linux/amd64", "linux/arm64"]`. **Required.**
    ///
    /// There is deliberately no default. `platforms` is a label written into the
    /// image config, and nothing relates it to the layers: defaulting to the
    /// host's architecture would make an arm64 laptop and an amd64 CI runner
    /// produce different images from one BUILD file, and would let an
    /// `amd64` binary ship inside a config claiming `arm64` — an `exec format
    /// error` at run time, correctly cached, with no build-time error.
    platforms: Vec<String>,
    /// The image's entrypoint (`ENTRYPOINT`). Setting it clears any `cmd`
    /// inherited from the base, as a Dockerfile's `ENTRYPOINT` does.
    entrypoint: Vec<String>,
    /// The image's default arguments (`CMD`).
    cmd: Vec<String>,
    /// Environment variables, merged over the base's by name.
    env: HashMap<String, String>,
    /// The user to run as (`USER`), e.g. `"65532:65532"`.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    user: Option<String>,
    /// The working directory (`WORKDIR`).
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    workdir: Option<String>,
    /// Image labels, merged over the base's.
    labels: HashMap<String, String>,
    /// Ports the image declares (`EXPOSE`), e.g. `["8080/tcp"]`.
    exposed_ports: Vec<String>,
    /// Archive format: `oci` (default) or `docker`.
    format: ImageFormat,
    /// Write an OCI **layout directory** instead of a single archive file. This
    /// is the shape another image's `base`, and buildx's `oci-layout://` build
    /// context, consume.
    layout: bool,
    /// Output filename (or directory name, with `layout = True`), relative to
    /// the target's package. Must be a bare name. Default `<target name>.tar`,
    /// or `<target name>.oci` for a layout.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    out: Option<String>,
    /// Caching for the built image. Defaults to on for both tiers.
    cache: TargetSpecCache,
    /// What the target reserves from the resource pools while it runs, e.g.
    /// `{"cpu": 2}`. IO-bound: an undeclared `cpu` costs 0.1. Never hashed.
    resources: hplugin::driver::Resources,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct OciImageDef {
    /// Workspace-relative output path.
    out: String,
    /// Workspace-relative digest output path.
    digest_out: String,
    layout: bool,
    format: ImageFormat,
    /// The base's normalized address, or `None`.
    ///
    /// The address, not just the dep's hash: see [`layers`](Self::layers).
    base: Option<String>,
    /// Normalized `layers` addresses, **in order**.
    ///
    /// `hashin` cannot carry this. It folds a *sorted, unlabeled multiset* of
    /// dep hashouts — no addr, no origin, and no output-group selector, since
    /// `inputs_result_meta` folds every group a dep has. So `[":a", ":b"]` and
    /// `[":b", ":a"]` reach it identically, as do `base = ":a", layers = [":b"]`
    /// and the swap, and `[":bin|release"]` and `[":bin|debug"]` — four ways to
    /// serve one cache entry for two different images.
    ///
    /// (`docker_build`'s single `dockerfile = ":target"` role can safely omit
    /// its address: one occupant means any change to *which* target fills it
    /// changes that dep's hashout. An ordered list has no such property.)
    layers: Vec<String>,
    /// Normalized per-platform layer addresses, in platform order then list
    /// order. Same reason.
    layers_by_platform: Vec<(String, Vec<String>)>,
    /// Platforms, in order: the manifest list's entry order is its bytes.
    platforms: Vec<String>,
    entrypoint: Vec<String>,
    cmd: Vec<String>,
    /// Sorted for a stable hash and a canonical config.
    env: BTreeMap<String, String>,
    user: Option<String>,
    workdir: Option<String>,
    labels: BTreeMap<String, String>,
    exposed_ports: Vec<String>,
}

/// Bump to invalidate cached images when the emitted bytes change shape.
///
/// Covers the transforms that are not themselves hashed fields: the config JSON
/// canonicalization, the base-config merge rule, the layer media type, and the
/// absence of `created`. Changing one of those without a bump is the
/// same-key-different-artifact shape.
///
/// 2: layers are read through their descriptor, and a dep without one is
/// refused. The bump also evicts images cached from a non-layer dep, which a
/// cache hit would otherwise keep serving.
const OCI_IMAGE_FORMAT_VERSION: u32 = 2;

impl Hash for OciImageDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        OCI_IMAGE_FORMAT_VERSION.hash(state);
        self.out.hash(state);
        self.digest_out.hash(state);
        self.layout.hash(state);
        self.format.output_type().hash(state);
        self.base.hash(state);
        self.layers.hash(state);
        self.layers_by_platform.hash(state);
        self.platforms.hash(state);
        self.entrypoint.hash(state);
        self.cmd.hash(state);
        self.env.hash(state);
        self.user.hash(state);
        self.workdir.hash(state);
        self.labels.hash(state);
        self.exposed_ports.hash(state);
    }
}

/// A base image's contribution to one platform: its layer descriptors, its
/// `diff_ids`, and its config to inherit from.
#[derive(Default)]
struct BaseFor {
    layers: Vec<Value>,
    diff_ids: Vec<Value>,
    config: Map<String, Value>,
    /// Blobs to carry into the output layout, by reference — a base's layers
    /// are never read here.
    blobs: Vec<(String, Blob)>,
}

/// Merge the base's `config` object with what the BUILD file set.
///
/// Dockerfile semantics, because that is what a user migrating from
/// `docker_build` expects: `Env` and `Labels` merge by key with ours winning,
/// scalars are replaced only when set here, and setting `Entrypoint` clears an
/// inherited `Cmd` — otherwise the base's arguments would be silently passed to
/// a completely different program.
fn merge_config(base: &Map<String, Value>, def: &OciImageDef) -> Map<String, Value> {
    let mut cfg = base.clone();

    // Sorted by name, not kept in the base's order: `Env` is a map spelled as a
    // list, the order carries no meaning, and a stable one is what makes two
    // encodings of the same image the same bytes.
    //
    // An entry with no `=` is not dropped — a malformed base is the base's
    // problem, and silently losing one of its variables here would be ours.
    let mut env: BTreeMap<String, Option<String>> = BTreeMap::new();
    if let Some(Value::Array(existing)) = base.get("Env") {
        for item in existing {
            if let Some(s) = item.as_str() {
                match s.split_once('=') {
                    Some((k, v)) => env.insert(k.to_string(), Some(v.to_string())),
                    None => env.insert(s.to_string(), None),
                };
            }
        }
    }
    for (k, v) in &def.env {
        env.insert(k.clone(), Some(v.clone()));
    }
    if env.is_empty() {
        cfg.remove("Env");
    } else {
        cfg.insert(
            "Env".to_string(),
            Value::Array(
                env.iter()
                    .map(|(k, v)| match v {
                        Some(v) => Value::String(format!("{k}={v}")),
                        None => Value::String(k.clone()),
                    })
                    .collect(),
            ),
        );
    }

    if !def.entrypoint.is_empty() {
        cfg.insert("Entrypoint".to_string(), json!(def.entrypoint));
        // An inherited `Cmd` is arguments for the base's entrypoint. Keeping it
        // would hand them to whatever this image runs instead.
        cfg.remove("Cmd");
    }
    if !def.cmd.is_empty() {
        cfg.insert("Cmd".to_string(), json!(def.cmd));
    }
    if let Some(user) = &def.user {
        cfg.insert("User".to_string(), json!(user));
    }
    if let Some(workdir) = &def.workdir {
        cfg.insert("WorkingDir".to_string(), json!(workdir));
    }
    if !def.labels.is_empty() {
        let mut labels = match base.get("Labels") {
            Some(Value::Object(m)) => m.clone(),
            _ => Map::new(),
        };
        for (k, v) in &def.labels {
            labels.insert(k.clone(), json!(v));
        }
        cfg.insert("Labels".to_string(), Value::Object(labels));
    }
    if !def.exposed_ports.is_empty() {
        let mut ports = match base.get("ExposedPorts") {
            Some(Value::Object(m)) => m.clone(),
            _ => Map::new(),
        };
        for p in &def.exposed_ports {
            ports.insert(p.clone(), json!({}));
        }
        cfg.insert("ExposedPorts".to_string(), Value::Object(ports));
    }
    cfg
}

/// The image config for one platform.
///
/// `created` is deliberately absent, as are per-layer timestamps: a wall clock
/// in here would give the same inputs a different image digest on every build
/// and defeat cross-machine cache sharing entirely.
///
/// Built through `serde_json`, whose object type is a `BTreeMap` in this tree —
/// so the emitted bytes are canonical without a second pass. (`oci-spec`'s own
/// config types use `HashMap` for `Labels`, and Rust's `RandomState` is seeded
/// *per process*: two heph runs on one machine would emit two byte orders, two
/// config digests, and two manifest digests under one cache key.)
fn build_config(
    platform: &str,
    base: &BaseFor,
    def: &OciImageDef,
    diff_ids: &[Value],
) -> anyhow::Result<Vec<u8>> {
    let (os, arch) = split_platform(platform)?;
    let mut config = Map::new();
    config.insert("architecture".to_string(), json!(arch));
    config.insert("os".to_string(), json!(os));
    config.insert(
        "config".to_string(),
        Value::Object(merge_config(&base.config, def)),
    );
    config.insert(
        "rootfs".to_string(),
        json!({"type": "layers", "diff_ids": diff_ids}),
    );
    serde_json::to_vec(&Value::Object(config)).context("encode the image config")
}

/// The `base` image, its manifests listed once for every platform built on it.
struct BaseImage {
    layout: archive::Layout,
    manifests: Vec<(
        oci_client::manifest::OciImageManifest,
        Option<oci_client::manifest::Platform>,
        String,
    )>,
}

/// The base's per-platform pieces, or an empty contribution when there is none.
fn base_for(base: Option<&BaseImage>, platform: &str) -> anyhow::Result<BaseFor> {
    let Some(BaseImage { layout, manifests }) = base else {
        return Ok(BaseFor::default());
    };
    anyhow::ensure!(
        !manifests.is_empty(),
        "`base` holds no manifests; it is not an image"
    );
    let (os, arch) = split_platform(platform)?;

    let mut available = Vec::new();
    let mut chosen = None;
    for (manifest, p, _) in manifests {
        match p {
            Some(p) => {
                available.push(format!("{}/{}", p.os, p.architecture));
                // The last match wins (`select_platform` keeps the first);
                // kept as it was when the matcher was shared.
                if is_os_arch(p, os, arch) {
                    chosen = Some(manifest);
                }
            }
            // A single-image layout carries no platform annotation; there is
            // nothing to choose and refusing it would be pedantry.
            None if manifests.len() == 1 => chosen = Some(manifest),
            None => {}
        }
    }
    let manifest = chosen.with_context(|| {
        format!(
            "`base` has no {platform} instance (it has: {}). Pull it with \
             an `oci.pull` target with `layout = True, all_platforms = True` so every platform in `platforms` \
             has a base to sit on.",
            available.join(", ")
        )
    })?;

    let config_bytes = layout.blob_bytes(&manifest.config.digest)?;
    let config: Value =
        serde_json::from_slice(&config_bytes).context("parse the base image config")?;
    let diff_ids = config
        .get("rootfs")
        .and_then(|r| r.get("diff_ids"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let inner = match config.get("config") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };

    let mut blobs = Vec::new();
    let mut layers = Vec::new();
    for layer in &manifest.layers {
        // The base's layers are carried by *reference* — they stay in the base's
        // own layout on disk and are streamed straight into this image's.
        blobs.push((layer.digest.clone(), layout.blob(&layer.digest)?.clone()));
        layers.push(json!({
            "mediaType": layer.media_type,
            "digest": layer.digest,
            "size": layer.size,
        }));
    }
    Ok(BaseFor {
        layers,
        diff_ids,
        config: inner,
        blobs,
    })
}

/// Stateless: an image is assembled from declared inputs and attributes.
#[derive(Default)]
pub struct Driver;

impl Driver {
    pub fn new() -> Self {
        Driver
    }
}

#[async_trait]
impl ManagedDriver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "image".to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        OciImageSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let addr = &req.target_spec.addr;
        let pkg = addr.package.to_owned();
        let spec = OciImageSpec::from(&req.target_spec.config).context("parse oci_image config")?;

        anyhow::ensure!(
            !spec.platforms.is_empty(),
            "`platforms` is required, e.g. `platforms = [\"linux/amd64\"]`. There is no default \
             on purpose: the platform is written into the image config and nothing checks it \
             against the layers, so a host-derived default would put an amd64 binary in an arm64 \
             image on one machine and not the other, cache it, and say nothing."
        );
        let platforms = spec
            .platforms
            .iter()
            .map(|p| normalize_platform(p))
            .collect::<anyhow::Result<Vec<_>>>()
            .context("`platforms`")?;

        // Keyed by the normalized platform, each with the key as written: two
        // spellings of one platform ("linux/amd64", "linux/x86_64") would
        // otherwise collapse into whichever sorts last, and the other's layers
        // vanish from the image.
        let mut pbp: BTreeMap<String, (&str, &Vec<String>)> = BTreeMap::new();
        for (key, list) in &spec.layers_by_platform {
            let platform = normalize_platform(key)
                .with_context(|| format!("`layers_by_platform` key {key:?}"))?;
            if let Some((other, _)) = pbp.insert(platform.clone(), (key.as_str(), list)) {
                anyhow::bail!(
                    "`layers_by_platform` has two entries for {platform:?}: {other:?} and \
                     {key:?} name the same platform. Merge them into one entry."
                );
            }
        }
        if !pbp.is_empty() {
            for platform in &platforms {
                anyhow::ensure!(
                    pbp.contains_key(platform),
                    "`layers_by_platform` has no entry for {platform:?}, which `platforms` \
                     lists. Every platform needs its own layers, or the image ships without \
                     them on that architecture."
                );
            }
            for platform in pbp.keys() {
                anyhow::ensure!(
                    platforms.contains(platform),
                    "`layers_by_platform` has an entry for {platform:?}, which `platforms` does \
                     not list. Add it there, or drop the entry."
                );
            }
        }
        anyhow::ensure!(
            !spec.layers.is_empty() || !pbp.is_empty() || spec.base.is_some(),
            "this image has no `base` and no `layers`, so it would have no filesystem at all. \
             Add at least one `oci.layer` target."
        );

        let format = spec.format;
        anyhow::ensure!(
            !(matches!(format, ImageFormat::Docker) && platforms.len() > 1),
            "`format = \"docker\"` holds a single image, but `platforms` lists {}. A daemon tag \
             is one image: build one platform, or use the default `format = \"oci\"`.",
            platforms.len()
        );

        let mut inputs = Vec::new();
        let mut push_dep = |addr_str: &str, origin: String| -> anyhow::Result<String> {
            let r#ref = TargetAddr::parse(addr_str, &pkg)
                .with_context(|| format!("parse target address {addr_str:?}"))?;
            let normalized = r#ref.to_string();
            inputs.push(Input {
                r#ref,
                mode: InputMode::Standard,
                origin_id: origin.clone(),
                // Out of the workspace root: a base layout is hundreds of
                // megabytes and has no business being unpacked over the tree.
                annotations: BTreeMap::from([(
                    "unpack_root".to_string(),
                    format!("oci_{}", origin.replace(['|', '/'], "_")),
                )]),
                hashed: true,
                runtime: true,
            });
            Ok(normalized)
        };

        let base = match &spec.base {
            Some(b) => Some(push_dep(b, BASE_ORIGIN.to_string())?),
            None => None,
        };
        let mut layers = Vec::new();
        for (i, l) in spec.layers.iter().enumerate() {
            layers.push(push_dep(l, layer_origin(None, i))?);
        }
        let mut layers_by_platform = Vec::new();
        for (platform, (_, list)) in &pbp {
            let mut normalized = Vec::new();
            for (i, l) in list.iter().enumerate() {
                normalized.push(push_dep(l, layer_origin(Some(platform), i))?);
            }
            layers_by_platform.push((platform.clone(), normalized));
        }

        let default_out = if spec.layout {
            format!("{}.oci", addr.name)
        } else {
            format!("{}.tar", addr.name)
        };
        let out_rel = spec.out.unwrap_or(default_out);
        anyhow::ensure!(
            std::path::Path::new(&out_rel)
                .file_name()
                .map(std::ffi::OsStr::new)
                == Some(out_rel.as_ref()),
            "`out` {out_rel:?} must be a bare name (no directory component): the driver writes \
             it into the target's package directory"
        );
        let out = ws_path(pkg.as_str(), &out_rel);
        let digest_out = ws_path(pkg.as_str(), &format!("{}.digest", addr.name));

        let def = OciImageDef {
            out: out.clone(),
            digest_out: digest_out.clone(),
            layout: spec.layout,
            format,
            base,
            layers,
            layers_by_platform,
            platforms,
            entrypoint: spec.entrypoint,
            cmd: spec.cmd,
            env: spec.env.into_iter().collect(),
            user: spec.user,
            workdir: spec.workdir,
            labels: spec.labels.into_iter().collect(),
            exposed_ports: spec.exposed_ports,
        };
        let hash = {
            let mut h = DebugHasher::new(Xxh3Default::new(), || {
                format!("oci_image_{}", addr.format())
            });
            def.hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs,
                outputs: vec![
                    Output {
                        group: String::new(),
                        paths: vec![OutPath {
                            content: if spec.layout {
                                Content::DirPath(out)
                            } else {
                                Content::FilePath(out)
                            },
                            codegen_tree: CodegenMode::None,
                            collect: true,
                        }],
                    },
                    Output {
                        group: "digest".to_string(),
                        paths: vec![OutPath {
                            content: Content::FilePath(digest_out),
                            codegen_tree: CodegenMode::None,
                            collect: true,
                        }],
                    },
                ],
                cache: spec.cache.into(),
                hash,
                resources: spec
                    .resources
                    .advertise(
                        hplugin::provider::PoolName::cpu(),
                        hplugin::driver::Resources::IO_CPU,
                    )
                    .context("oci.image resources")?,
                ..Default::default()
            },
        })
    }

    async fn apply_transitive(
        &self,
        req: ApplyTransitiveRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ApplyTransitiveResponse> {
        Ok(ApplyTransitiveResponse {
            target_def: req.target_def,
        })
    }

    async fn run<'a, 'io>(
        &self,
        req: ManagedRunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let def = req.request.target.def_de::<OciImageDef>().clone();
        let out_path = req.sandbox_pkg_dir.join(super::basename(&def.out)?);

        let base_image = match &def.base {
            Some(_) => {
                let path = layout_path(&req, BASE_ORIGIN, "`base`")?;
                let layout = archive::Layout::read(&path)
                    .with_context(|| format!("read the base image at {path:?}"))?;
                // Once, not once per platform: listing reads every manifest.
                let manifests = layout.manifests()?;
                Some(BaseImage { layout, manifests })
            }
            None => None,
        };

        let mut blobs: Blobs = Blobs::new();
        let mut entries: Vec<ImageIndexEntry> = Vec::new();

        for platform in &def.platforms {
            let base = base_for(base_image.as_ref(), platform)?;
            for (digest, blob) in &base.blobs {
                blobs.insert(digest.clone(), blob.clone());
            }

            let mut layer_descs = base.layers.clone();
            let mut diff_ids = base.diff_ids.clone();
            // (origin, attribute, the entry as written): errors name the entry,
            // not the input behind it — through a `group`, that is the group's
            // first dep, not what the BUILD file lists.
            let mut own = Vec::new();
            for (i, dep) in def.layers.iter().enumerate() {
                own.push((layer_origin(None, i), "`layers`".to_string(), dep));
            }
            if let Some((_, list)) = def.layers_by_platform.iter().find(|(p, _)| p == platform) {
                for (i, dep) in list.iter().enumerate() {
                    own.push((
                        layer_origin(Some(platform), i),
                        format!("`layers_by_platform[{platform:?}]`"),
                        dep,
                    ));
                }
            }
            for (origin, attr, dep) in &own {
                let (path, descriptor) = select_layer(&dep_files(&req, origin)?, dep, attr)?;
                // Streamed, not read: a layer can be most of an image, and this
                // runs once per platform per image target. sha256 over every
                // byte is CPU work, off the async worker.
                let job_path = path.clone();
                let (digest, size) =
                    hcore::blocking::run(move || archive::sha256_file(&job_path)).await?;
                // Uncompressed, the blob is the tar: the two digests are one
                // value, and both are in hand. A mismatch is a corrupted or
                // hand-edited layer output, which a runtime would reject on
                // unpack.
                anyhow::ensure!(
                    descriptor.media_type != LayerEncoding::Tar || descriptor.diff_id == digest,
                    "{attr} entry {dep}: its uncompressed layer blob has digest {digest}, but its \
                     descriptor records diff_id {}; the layer's output is inconsistent, rebuild it",
                    descriptor.diff_id,
                );
                layer_descs.push(json!({
                    "mediaType": descriptor.media_type.media_type(),
                    "digest": digest,
                    "size": size,
                }));
                // The uncompressed tar's digest, which only the layer could
                // compute without inflating the blob again.
                diff_ids.push(json!(descriptor.diff_id));
                // Deduped by digest: two platforms sharing a static layer
                // reference one blob, which is a thing buildx cannot do.
                blobs.entry(digest).or_insert_with(|| Blob::File(path));
            }

            let config_bytes = build_config(platform, &base, &def, &diff_ids)?;
            let config_digest = archive::sha256_digest(&config_bytes);
            let config_size = config_bytes.len();
            blobs.insert(config_digest.clone(), Blob::Bytes(config_bytes));

            let manifest = json!({
                "schemaVersion": 2,
                "mediaType": oci_client::manifest::OCI_IMAGE_MEDIA_TYPE,
                "config": {
                    "mediaType": CONFIG_MEDIA_TYPE,
                    "digest": config_digest,
                    "size": config_size,
                },
                "layers": layer_descs,
            });
            let manifest_bytes = serde_json::to_vec(&manifest).context("encode the manifest")?;
            let manifest_digest = archive::sha256_digest(&manifest_bytes);
            let (os, arch) = split_platform(platform)?;
            entries.push(archive::image_manifest_entry(
                manifest_digest.clone(),
                manifest_bytes.len() as i64,
                Some(oci_client::manifest::Platform {
                    os: os.into(),
                    architecture: arch.into(),
                    os_version: None,
                    os_features: None,
                    variant: None,
                    features: None,
                }),
            ));
            blobs.insert(manifest_digest, Blob::Bytes(manifest_bytes));
        }

        let layout = archive::Layout {
            index: archive::oci_index(entries),
            blobs,
        };
        let index = &layout.index;

        match (def.layout, def.format) {
            (true, _) => archive::write_layout_dir_blobs(&out_path, index, &layout.blobs),
            (false, ImageFormat::Oci) => {
                archive::write_layout_tar_blobs(&out_path, index, &layout.blobs)
            }
            (false, ImageFormat::Docker) => write_docker(&out_path, &layout),
        }
        .with_context(|| format!("write the image to {out_path:?}"))?;

        // A single image reports its own manifest digest; several report the
        // list's, which is what a registry files the tag under.
        let digest = match index.manifests.as_slice() {
            [only] => only.digest.clone(),
            _ => {
                let raw = serde_json::to_vec(index).context("encode the manifest list")?;
                archive::sha256_digest(&raw)
            }
        };
        std::fs::write(
            req.sandbox_pkg_dir.join(super::basename(&def.digest_out)?),
            &digest,
        )
        .context("write the digest output")?;

        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

/// How many file names an error lists before it says "and N more": a `group`
/// or a `glob()` dep can produce thousands.
const NAMES_SHOWN: usize = 10;

/// The file names of `paths`, at most [`NAMES_SHOWN`] of them, for an error.
fn name_list(paths: &[&Path]) -> String {
    if paths.is_empty() {
        return "nothing".to_string();
    }
    let shown = paths
        .iter()
        .take(NAMES_SHOWN)
        .map(|p| p.file_name().unwrap_or(p.as_os_str()).to_string_lossy())
        .collect::<Vec<_>>()
        .join(", ");
    match paths.len().checked_sub(NAMES_SHOWN).filter(|n| *n > 0) {
        Some(more) => format!("{shown}, and {more} more"),
        None => shown,
    }
}

/// The blob an `oci.layer` dep produced, and its descriptor, out of the files
/// the dep produced. `dep` and `attr` name the entry in errors.
///
/// The descriptor is what marks the dep as a layer. Anything else — a binary,
/// a config file, a tar some other target wrote — is refused here: filed as a
/// layer blob, it would build and push clean and fail only when a runtime
/// tries to unpack it. So is a layer with anything beside it (a `group` of a
/// layer and a binary): the image would ship without the binary, and nothing
/// would say so.
fn select_layer(
    paths: &[PathBuf],
    dep: &str,
    attr: &str,
) -> anyhow::Result<(PathBuf, LayerDescriptor)> {
    let descriptors: Vec<&Path> = paths
        .iter()
        .map(PathBuf::as_path)
        .filter(|p| {
            p.file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|n| n.ends_with(DESCRIPTOR_SUFFIX))
        })
        .collect();
    let descriptor_path = match descriptors.as_slice() {
        [only] => *only,
        [] => return Err(not_a_layer(paths, dep, attr)),
        many => anyhow::bail!(
            "{attr} entry {dep} produced {} layers ({}); list each `oci.layer` target on its \
             own, in the order they stack",
            many.len(),
            name_list(many)
        ),
    };
    let descriptor: LayerDescriptor = serde_json::from_slice(
        &std::fs::read(descriptor_path)
            .with_context(|| format!("read the layer descriptor {descriptor_path:?}"))?,
    )
    .with_context(|| {
        format!("{attr} entry {dep}: parse its layer descriptor {descriptor_path:?}")
    })?;
    anyhow::ensure!(
        Path::new(&descriptor.blob).file_name() == Some(OsStr::new(&descriptor.blob)),
        "{attr} entry {dep}: its layer descriptor names the blob {:?}, which is not a plain \
         file name",
        descriptor.blob
    );
    let blob_path = descriptor_path.with_file_name(&descriptor.blob);
    // Else the descriptor's own JSON would be filed as the layer blob.
    anyhow::ensure!(
        blob_path != descriptor_path,
        "{attr} entry {dep}: its layer descriptor names itself, {:?}, as the blob; the layer's \
         output is inconsistent, rebuild it",
        descriptor.blob
    );
    anyhow::ensure!(
        paths.contains(&blob_path),
        "{attr} entry {dep}: its layer descriptor names the blob {:?}, which it did not \
         produce (it produced: {})",
        descriptor.blob,
        name_list(&paths.iter().map(PathBuf::as_path).collect::<Vec<_>>())
    );
    let extra: Vec<&Path> = paths
        .iter()
        .map(PathBuf::as_path)
        .filter(|p| *p != descriptor_path && *p != blob_path)
        .collect();
    anyhow::ensure!(
        extra.is_empty(),
        "{attr} entry {dep} produced {} file(s) besides its layer: {}. An image takes a layer as \
         one `oci.layer` target writes it, and those files would be left out of the image \
         without a word. Put them in an `oci.layer` of their own and list that, or into this \
         layer's `srcs`.",
        extra.len(),
        name_list(&extra)
    );
    Ok((blob_path, descriptor))
}

/// The error for a `layers` entry that is not an `oci.layer`: what it
/// produced, and the target that would make it one: a tar is adopted with
/// `tar =`, a gzipped file decompressed first, anything else wrapped with
/// `srcs`.
fn not_a_layer(paths: &[PathBuf], dep: &str, attr: &str) -> anyhow::Error {
    let produced = name_list(&paths.iter().map(PathBuf::as_path).collect::<Vec<_>>());
    let head = match paths {
        [only] => file_head(only),
        _ => Vec::new(),
    };
    let fix = if head.starts_with(&GZIP_MAGIC) {
        format!(
            "It is gzip-compressed; `oci.layer` adopts an uncompressed tar. Decompress it in a \
             target of its own, then adopt that:\n  target(name = \"<name>_tar\", driver = \
             \"bash\", run = \"gunzip -c $SRC > $OUT\", deps = [\"{dep}\"], out = \
             \"<name>.tar\")\n  target(name = \"<name>\", driver = \"oci.layer\", tar = \
             \":<name>_tar\")"
        )
    } else if head.get(257..262) == Some(b"ustar".as_slice()) {
        format!(
            "It is a tar; adopt it as the layer, as it is:\n  target(name = \"<name>\", driver = \
             \"oci.layer\", tar = \"{dep}\")\n(the tar's entries are checked when that target \
             runs)"
        )
    } else {
        format!(
            "Wrap it, to put its files at a path inside the image:\n  target(name = \"<name>\", \
             driver = \"oci.layer\", srcs = [\"{dep}\"], prefix = \"<dir in image>\")  # e.g. \
             /usr/bin"
        )
    };
    anyhow::anyhow!(
        "{attr} entry {dep} is not an `oci.layer` target (it produced: {produced}). An image \
         layer must be built by `oci.layer`, which records the layer's digests. {fix}"
    )
}

/// The first bytes of `path`, enough to see a tar's `ustar` magic at offset
/// 257. Empty when it cannot be read: it only picks the suggestion.
fn file_head(path: &Path) -> Vec<u8> {
    use std::io::Read as _;
    let mut head = Vec::with_capacity(263);
    if let Ok(file) = std::fs::File::open(path)
        && file.take(263).read_to_end(&mut head).is_err()
    {
        head.clear();
    }
    head
}

/// Write the docker-format archive a daemon's `docker load` accepts.
///
/// The image is already an in-memory [`archive::Layout`] — the index plus the
/// blobs, layers still on disk — which is what the docker writer reads, so it
/// takes the layout as it is: no layout directory written and read back.
/// `parse` refuses `format = "docker"` with more than one platform, so the
/// first manifest is the image.
fn write_docker(out: &std::path::Path, layout: &archive::Layout) -> anyhow::Result<()> {
    let (manifest, _, digest) = layout
        .manifests()?
        .into_iter()
        .next()
        .context("the assembled image has no manifest")?;
    archive::write_docker_archive(out, layout, &manifest, &digest, "heph:latest")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def() -> OciImageDef {
        OciImageDef {
            out: "app/img.tar".to_string(),
            digest_out: "app/img.digest".to_string(),
            layout: false,
            format: ImageFormat::Oci,
            base: None,
            layers: vec![],
            layers_by_platform: vec![],
            platforms: vec!["linux/amd64".to_string()],
            entrypoint: vec![],
            cmd: vec![],
            env: BTreeMap::new(),
            user: None,
            workdir: None,
            labels: BTreeMap::new(),
            exposed_ports: vec![],
        }
    }

    fn hash_of(d: &OciImageDef) -> u64 {
        let mut h = Xxh3Default::new();
        d.hash(&mut h);
        h.finish()
    }

    /// `format` decodes straight to an [`ImageFormat`]: absent or `None` is
    /// `oci`, and an unknown spelling is refused naming the field and the
    /// choices.
    #[test]
    fn format_decodes_to_the_image_format() {
        use hcore::htvalue::Value;
        let parse = |v: Option<Value>| {
            let config: HashMap<String, Value> =
                v.map(|v| ("format".to_string(), v)).into_iter().collect();
            OciImageSpec::from(&config).map(|s| s.format)
        };
        assert_eq!(parse(None).expect("absent"), ImageFormat::Oci);
        assert_eq!(parse(Some(Value::Null())).expect("None"), ImageFormat::Oci);
        assert_eq!(
            parse(Some(Value::String("docker".to_string()))).expect("docker"),
            ImageFormat::Docker
        );
        let err = parse(Some(Value::String("tar".to_string()))).expect_err("tar");
        assert_eq!(
            format!("{err:#}"),
            r#"parse `format`: invalid: expected one of [oci, docker], got: "tar""#
        );
    }

    /// `hashin` folds a sorted, unlabeled multiset of dep hashouts: no address,
    /// no origin, and no output-group selector. Every one of these swaps leaves
    /// it identical, so the def hash is the only thing standing between them and
    /// one cache entry serving two different images.
    #[test]
    fn swapping_dep_roles_changes_the_key() {
        let base = def();

        let mut ab = base.clone();
        ab.layers = vec!["//app:a".to_string(), "//app:b".to_string()];
        let mut ba = base.clone();
        ba.layers = vec!["//app:b".to_string(), "//app:a".to_string()];
        assert_ne!(
            hash_of(&ab),
            hash_of(&ba),
            "layer order decides diff_ids order and which file shadows which"
        );

        let mut base_a = base.clone();
        base_a.base = Some("//app:a".to_string());
        base_a.layers = vec!["//app:b".to_string()];
        let mut base_b = base.clone();
        base_b.base = Some("//app:b".to_string());
        base_b.layers = vec!["//app:a".to_string()];
        assert_ne!(hash_of(&base_a), hash_of(&base_b), "base is not a layer");

        let mut release = base.clone();
        release.layers = vec!["//app:bin|release".to_string()];
        let mut debug = base.clone();
        debug.layers = vec!["//app:bin|debug".to_string()];
        assert_ne!(
            hash_of(&release),
            hash_of(&debug),
            "the output-group selector reaches no other part of the key"
        );

        let mut amd = base.clone();
        amd.platforms = vec!["linux/amd64".to_string(), "linux/arm64".to_string()];
        amd.layers_by_platform = vec![
            ("linux/amd64".to_string(), vec!["//app:x".to_string()]),
            ("linux/arm64".to_string(), vec!["//app:y".to_string()]),
        ];
        let mut swapped = amd.clone();
        swapped.layers_by_platform = vec![
            ("linux/amd64".to_string(), vec!["//app:y".to_string()]),
            ("linux/arm64".to_string(), vec!["//app:x".to_string()]),
        ];
        assert_ne!(
            hash_of(&amd),
            hash_of(&swapped),
            "which platform gets which layer is the whole point of the map"
        );
    }

    /// Attributes that change the image must change the key; a map's iteration
    /// order must not.
    #[test]
    fn config_attributes_reach_the_key_but_map_order_does_not() {
        let a = def();
        for mutate in [
            (|d: &mut OciImageDef| d.entrypoint = vec!["/bin/x".to_string()]) as fn(&mut _),
            |d| d.cmd = vec!["--flag".to_string()],
            |d| d.user = Some("65532".to_string()),
            |d| d.workdir = Some("/srv".to_string()),
            |d| d.exposed_ports = vec!["80/tcp".to_string()],
            |d| {
                d.env.insert("PORT".to_string(), "8080".to_string());
            },
            |d| {
                d.labels.insert(
                    "org.opencontainers.image.source".to_string(),
                    "x".to_string(),
                );
            },
            |d| d.platforms = vec!["linux/arm64".to_string()],
            |d| d.layout = true,
            |d| d.format = ImageFormat::Docker,
        ] {
            let mut b = a.clone();
            mutate(&mut b);
            assert_ne!(hash_of(&a), hash_of(&b), "a changed attribute must re-key");
        }

        // `env` and `labels` are BTreeMaps, so insertion order cannot leak.
        let mut one = def();
        one.env.insert("A".into(), "1".into());
        one.env.insert("B".into(), "2".into());
        let mut other = def();
        other.env.insert("B".into(), "2".into());
        other.env.insert("A".into(), "1".into());
        assert_eq!(hash_of(&one), hash_of(&other));
    }

    /// Dropping a base's `PATH` is the easiest way to ship an image that starts
    /// and then cannot find its own entrypoint, so inheritance is asserted per
    /// field rather than assumed.
    #[test]
    fn the_base_config_is_inherited_and_overridden_per_field() {
        let base: Map<String, Value> = serde_json::from_value(json!({
            "Env": ["PATH=/usr/bin", "LANG=C"],
            "Entrypoint": ["/bin/base"],
            "Cmd": ["--base-arg"],
            "User": "root",
            "WorkingDir": "/base",
            "Labels": {"vendor": "alpine"},
        }))
        .expect("base config");

        // Nothing set: everything is inherited. `Env` comes back sorted — it is
        // a map spelled as a list, and a stable order is what makes two
        // encodings of one image the same bytes.
        let inherited = merge_config(&base, &def());
        assert_eq!(
            inherited.get("Env").expect("Env"),
            &json!(["LANG=C", "PATH=/usr/bin"])
        );
        for key in ["Entrypoint", "Cmd", "User", "WorkingDir", "Labels"] {
            assert_eq!(
                inherited.get(key),
                base.get(key),
                "{key} must be inherited untouched"
            );
        }

        // A base entry with no `=` is malformed, but dropping it would lose a
        // variable the base meant to set.
        let odd: Map<String, Value> =
            serde_json::from_value(json!({"Env": ["BARE", "A=1"]})).expect("odd");
        assert_eq!(
            merge_config(&odd, &def()).get("Env").expect("Env"),
            &json!(["A=1", "BARE"])
        );

        // Env merges by key, ours winning; the base's other vars survive.
        let mut d = def();
        d.env.insert("PATH".into(), "/opt/bin".into());
        d.env.insert("PORT".into(), "8080".into());
        let merged = merge_config(&base, &d);
        assert_eq!(
            merged.get("Env").expect("Env"),
            &json!(["LANG=C", "PATH=/opt/bin", "PORT=8080"])
        );

        // Setting entrypoint clears the base's Cmd: those arguments were meant
        // for a different program.
        let mut d = def();
        d.entrypoint = vec!["/usr/bin/server".to_string()];
        let merged = merge_config(&base, &d);
        assert_eq!(
            merged.get("Entrypoint").expect("ep"),
            &json!(["/usr/bin/server"])
        );
        assert!(
            merged.get("Cmd").is_none(),
            "an inherited Cmd must not survive"
        );

        // …unless a cmd is given too.
        let mut d = def();
        d.entrypoint = vec!["/usr/bin/server".to_string()];
        d.cmd = vec!["--serve".to_string()];
        assert_eq!(
            merge_config(&base, &d).get("Cmd").expect("cmd"),
            &json!(["--serve"])
        );

        // Scalars replace, labels merge.
        let mut d = def();
        d.user = Some("65532:65532".to_string());
        d.workdir = Some("/srv".to_string());
        d.labels.insert("app".into(), "server".into());
        let merged = merge_config(&base, &d);
        assert_eq!(merged.get("User").expect("user"), &json!("65532:65532"));
        assert_eq!(merged.get("WorkingDir").expect("wd"), &json!("/srv"));
        assert_eq!(
            merged.get("Labels").expect("labels"),
            &json!({"app": "server", "vendor": "alpine"})
        );
    }

    /// The config's bytes are its digest, so two encodings of the same image
    /// must be identical — and must carry no timestamp.
    #[test]
    fn the_config_is_canonical_and_has_no_timestamp() {
        let mut d = def();
        for (k, v) in [("Z", "1"), ("A", "2"), ("M", "3")] {
            d.env.insert(k.to_string(), v.to_string());
        }
        d.labels.insert("b".into(), "2".into());
        d.labels.insert("a".into(), "1".into());
        let base = BaseFor::default();
        let diff_ids = vec![json!("sha256:aa")];

        let one = build_config("linux/amd64", &base, &d, &diff_ids).expect("one");
        let two = build_config("linux/amd64", &base, &d, &diff_ids).expect("two");
        assert_eq!(one, two, "the same image must encode to the same bytes");

        let text = String::from_utf8(one).expect("utf8");
        assert!(!text.contains("created"), "no wall clock: {text}");
        assert!(
            text.contains(r#""architecture":"amd64""#) && text.contains(r#""os":"linux""#),
            "got: {text}"
        );
        // serde_json's Map is a BTreeMap here, so keys come out sorted — the
        // property that keeps two runs of one process from disagreeing.
        assert!(
            text.find(r#""A=2""#) < text.find(r#""M=3""#)
                && text.find(r#""M=3""#) < text.find(r#""Z=1""#),
            "env must be sorted: {text}"
        );
    }

    /// Two instances filed under the same os/arch (`linux/arm`, variants v6
    /// and v7), told apart by their configs' diff_ids. Returns the layout and
    /// the two manifest digests, in index order.
    fn same_platform_twice() -> (archive::Layout, [String; 2]) {
        let mut blobs = Blobs::new();
        let mut entries = Vec::new();
        let mut digests = Vec::new();
        for (variant, diff_id) in [("v6", "sha256:first"), ("v7", "sha256:second")] {
            let config = serde_json::to_vec(&json!({
                "architecture": "arm",
                "os": "linux",
                "rootfs": {"type": "layers", "diff_ids": [diff_id]},
            }))
            .expect("encode config");
            let config_digest = archive::sha256_digest(&config);
            let manifest = serde_json::to_vec(&json!({
                "schemaVersion": 2,
                "mediaType": oci_client::manifest::OCI_IMAGE_MEDIA_TYPE,
                "config": {
                    "mediaType": CONFIG_MEDIA_TYPE,
                    "digest": config_digest,
                    "size": config.len(),
                },
                "layers": [],
            }))
            .expect("encode manifest");
            let digest = archive::sha256_digest(&manifest);
            entries.push(archive::image_manifest_entry(
                digest.clone(),
                manifest.len() as i64,
                Some(oci_client::manifest::Platform {
                    os: "linux".into(),
                    architecture: "arm".into(),
                    os_version: None,
                    os_features: None,
                    variant: Some(variant.to_string()),
                    features: None,
                }),
            ));
            blobs.insert(config_digest, Blob::Bytes(config));
            blobs.insert(digest.clone(), Blob::Bytes(manifest));
            digests.push(digest);
        }
        let layout = archive::Layout {
            index: archive::oci_index(entries),
            blobs,
        };
        let [first, second] = <[String; 2]>::try_from(digests).expect("two digests");
        (layout, [first, second])
    }

    /// Two instances under one os/arch: `base_for` takes the last, as it
    /// always has, and `select_platform` the first. Sharing the matcher kept
    /// each caller's policy; this pins both.
    #[test]
    fn same_platform_twice_keeps_each_pickers_policy() {
        let (layout, [first, _second]) = same_platform_twice();
        let manifests = layout.manifests().expect("manifests");
        let base = BaseImage { layout, manifests };

        let got = base_for(Some(&base), "linux/arm").expect("base_for");
        assert_eq!(
            got.diff_ids,
            vec![json!("sha256:second")],
            "base_for: last wins"
        );

        let (_, digest) = super::super::select_platform(&base.layout, "linux/arm").expect("select");
        assert_eq!(digest, first, "select_platform: first wins");
    }

    /// A one-platform image: a layer on disk, a layer in memory, a config and
    /// the manifest, filed under an index the way `run` files them.
    fn docker_fixture(dir: &std::path::Path) -> archive::Layout {
        let mut blobs = Blobs::new();
        let disk_layer = dir.join("layer-on-disk.tar");
        std::fs::write(&disk_layer, vec![7u8; 70_000]).expect("write layer");
        let (disk_digest, disk_size) = archive::sha256_file(&disk_layer).expect("hash layer");
        blobs.insert(disk_digest.clone(), Blob::File(disk_layer));
        let mem_layer = b"in-memory layer".to_vec();
        let mem_digest = archive::sha256_digest(&mem_layer);
        let mem_size = mem_layer.len();
        blobs.insert(mem_digest.clone(), Blob::Bytes(mem_layer));
        let config = br#"{"architecture":"amd64","os":"linux"}"#.to_vec();
        let config_digest = archive::sha256_digest(&config);
        let config_size = config.len();
        blobs.insert(config_digest.clone(), Blob::Bytes(config));
        let manifest = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": oci_client::manifest::OCI_IMAGE_MEDIA_TYPE,
            "config": {"mediaType": CONFIG_MEDIA_TYPE, "digest": config_digest, "size": config_size},
            "layers": [
                {"mediaType": LayerEncoding::Tar.media_type(), "digest": disk_digest, "size": disk_size},
                {"mediaType": LayerEncoding::Tar.media_type(), "digest": mem_digest, "size": mem_size},
            ],
        }))
        .expect("encode manifest");
        let manifest_digest = archive::sha256_digest(&manifest);
        let entry = archive::image_manifest_entry(
            manifest_digest.clone(),
            manifest.len() as i64,
            Some(oci_client::manifest::Platform {
                os: "linux".into(),
                architecture: "amd64".into(),
                os_version: None,
                os_features: None,
                variant: None,
                features: None,
            }),
        );
        blobs.insert(manifest_digest, Blob::Bytes(manifest));
        archive::Layout {
            index: archive::oci_index(vec![entry]),
            blobs,
        }
    }

    /// The docker archive written straight from the in-memory layout is the
    /// one the old route wrote — a layout directory on disk, read back — byte
    /// for byte, and two writes of one image agree.
    #[test]
    fn the_docker_archive_matches_the_round_trip_through_a_layout_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = docker_fixture(dir.path());

        let direct = dir.path().join("direct.tar");
        write_docker(&direct, &layout).expect("write direct");

        let tmp = dir.path().join("round-trip.oci-tmp");
        archive::write_layout_dir_blobs(&tmp, &layout.index, &layout.blobs).expect("write dir");
        let reread = archive::Layout::read(&tmp).expect("read dir");
        let (manifest, _, digest) = reread
            .manifests()
            .expect("manifests")
            .into_iter()
            .next()
            .expect("one manifest");
        let round_trip = dir.path().join("round-trip.tar");
        archive::write_docker_archive(&round_trip, &reread, &manifest, &digest, "heph:latest")
            .expect("write round trip");

        let direct_bytes = std::fs::read(&direct).expect("read direct");
        assert_eq!(
            direct_bytes,
            std::fs::read(&round_trip).expect("read round trip"),
            "the archive must not depend on the route"
        );

        let again = dir.path().join("again.tar");
        write_docker(&again, &layout).expect("write again");
        assert_eq!(
            direct_bytes,
            std::fs::read(&again).expect("read again"),
            "the same image must write the same archive"
        );
    }

    const DEP: &str = "//app:dep";
    const ATTR: &str = "`layers`";

    /// Write `files` (name, body) into `dir` and return their paths, as a
    /// dep's list names them.
    fn produced(dir: &Path, files: &[(&str, &str)]) -> Vec<PathBuf> {
        files
            .iter()
            .map(|(name, body)| {
                let p = dir.join(name);
                std::fs::write(&p, body).expect("write");
                p
            })
            .collect()
    }

    fn descriptor_json(blob: &str, media_type: &str) -> String {
        json!({"blob": blob, "mediaType": media_type, "diffId": "sha256:00"}).to_string()
    }

    fn refused(paths: &[PathBuf]) -> String {
        format!(
            "{:#}",
            select_layer(paths, DEP, ATTR).expect_err("must be refused")
        )
    }

    /// The one shape accepted: a descriptor and the blob it names, nothing
    /// else.
    #[test]
    fn a_layer_is_its_descriptor_and_its_blob() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = produced(
            dir.path(),
            &[
                ("l.tar.gz", "gz"),
                (
                    "l.oci-layer.json",
                    &descriptor_json("l.tar.gz", LayerEncoding::TarGzip.media_type()),
                ),
            ],
        );
        let (blob, descriptor) = select_layer(&paths, DEP, ATTR).expect("a layer");
        assert_eq!(blob, dir.path().join("l.tar.gz"));
        assert_eq!(descriptor.media_type, LayerEncoding::TarGzip);
    }

    /// Every way a dep's output can fail to be one layer, each refused with
    /// the entry named — never a layer blob filed by guesswork.
    #[test]
    fn everything_else_is_refused_naming_the_dep() {
        /// A case: its name, the files the dep produced, and the error text.
        type Case = (&'static str, Vec<(&'static str, String)>, &'static str);
        let gz = LayerEncoding::TarGzip.media_type();
        let cases: Vec<Case> = vec![
            (
                "no descriptor",
                vec![("server", "elf".into())],
                "is not an `oci.layer` target (it produced: server)",
            ),
            (
                "two descriptors",
                vec![
                    ("a.tar.gz", "a".into()),
                    ("a.oci-layer.json", descriptor_json("a.tar.gz", gz)),
                    ("b.tar.gz", "b".into()),
                    ("b.oci-layer.json", descriptor_json("b.tar.gz", gz)),
                ],
                "produced 2 layers (a.oci-layer.json, b.oci-layer.json)",
            ),
            (
                "unparseable descriptor",
                vec![("l.tar.gz", "gz".into()), ("l.oci-layer.json", "{".into())],
                "parse its layer descriptor",
            ),
            (
                "unknown media type",
                vec![
                    ("l.tar.gz", "gz".into()),
                    (
                        "l.oci-layer.json",
                        descriptor_json("l.tar.gz", "application/vnd.oci.image.layer.v1.tar+zstd"),
                    ),
                ],
                "unknown variant",
            ),
            (
                "blob not produced",
                vec![("l.oci-layer.json", descriptor_json("l.tar.gz", gz))],
                "names the blob \"l.tar.gz\", which it did not produce",
            ),
            (
                "blob with a separator",
                vec![
                    ("l.tar.gz", "gz".into()),
                    ("l.oci-layer.json", descriptor_json("sub/l.tar.gz", gz)),
                ],
                "which is not a plain file name",
            ),
            (
                "blob escaping the output",
                vec![
                    ("l.tar.gz", "gz".into()),
                    ("l.oci-layer.json", descriptor_json("..", gz)),
                ],
                "which is not a plain file name",
            ),
            (
                "descriptor naming itself",
                vec![("l.oci-layer.json", descriptor_json("l.oci-layer.json", gz))],
                "its layer descriptor names itself, \"l.oci-layer.json\", as the blob",
            ),
            (
                "a file beside the layer",
                vec![
                    ("l.tar.gz", "gz".into()),
                    ("l.oci-layer.json", descriptor_json("l.tar.gz", gz)),
                    ("server", "elf".into()),
                ],
                "produced 1 file(s) besides its layer: server",
            ),
        ];
        for (case, files, want) in cases {
            let dir = tempfile::tempdir().expect("tempdir");
            let files: Vec<(&str, &str)> = files.iter().map(|(n, b)| (*n, b.as_str())).collect();
            let err = refused(&produced(dir.path(), &files));
            assert!(err.contains(DEP), "{case}: names the dep: {err}");
            assert!(err.contains(want), "{case}: wanted {want:?} in: {err}");
        }
    }

    /// The fix offered fits what the dep produced: a tar is adopted with
    /// `tar =`, a gzip stream is decompressed first, anything else is wrapped
    /// with `srcs`.
    #[test]
    fn the_refusal_suggests_the_fix_for_what_was_produced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_size(2);
        h.set_cksum();
        b.append_data(&mut h, "etc/motd", &b"hi"[..])
            .expect("append");
        let tar_path = dir.path().join("rootfs.tar");
        std::fs::write(&tar_path, b.into_inner().expect("finish")).expect("write");
        let gz_path = dir.path().join("rootfs.tar.gz");
        std::fs::write(&gz_path, [0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0]).expect("write");
        let bin_path = dir.path().join("server");
        std::fs::write(&bin_path, "elf").expect("write");

        let cases = [
            (
                "a tar",
                tar_path,
                format!(
                    "It is a tar; adopt it as the layer, as it is:\n  target(name = \"<name>\", driver = \"oci.layer\", tar = \"{DEP}\")\n(the tar's entries are checked when that target runs)"
                ),
                "srcs =",
            ),
            (
                "a gzip stream",
                gz_path,
                format!(
                    "It is gzip-compressed; `oci.layer` adopts an uncompressed tar. Decompress it \
                     in a target of its own, then adopt that:\n  target(name = \"<name>_tar\", \
                     driver = \"bash\", run = \"gunzip -c $SRC > $OUT\", deps = [\"{DEP}\"], out \
                     = \"<name>.tar\")"
                ),
                "srcs =",
            ),
            (
                "anything else",
                bin_path,
                format!("srcs = [\"{DEP}\"], prefix = \"<dir in image>\""),
                "tar =",
            ),
        ];
        for (case, path, want, not) in cases {
            let err = refused(&[path]);
            assert!(
                err.contains("is not an `oci.layer` target"),
                "{case}: the stable phrase: {err}"
            );
            assert!(err.contains(&want), "{case}: wanted {want:?} in: {err}");
            assert!(!err.contains(not), "{case}: no {not:?} in: {err}");
        }
    }

    /// A big `group` or `glob()` dep would otherwise put thousands of names
    /// into one error line.
    #[test]
    fn the_produced_list_is_capped() {
        let paths: Vec<PathBuf> = (0..25)
            .map(|i| PathBuf::from(format!("/s/f{i:02}")))
            .collect();
        let err = refused(&paths);
        assert!(err.contains("f09, and 15 more"), "got: {err}");
        assert!(!err.contains("f10"), "got: {err}");
    }

    fn parse_image(pairs: &[(&str, hcore::htvalue::Value)]) -> anyhow::Result<ParseResponse> {
        let req = ParseRequest {
            request_id: "test".to_string(),
            target_spec: Arc::new(hplugin::provider::TargetSpec {
                addr: hmodel::htaddr::parse_addr("//app:img").expect("addr"),
                driver: "oci.image".to_string(),
                config: pairs
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), v.clone()))
                    .collect(),
                ..Default::default()
            }),
        };
        futures::executor::block_on(
            Driver::new().parse(req, &hcore::hasync::StdCancellationToken::new()),
        )
    }

    /// Two spellings of one platform must not collapse into whichever sorts
    /// last: the other entry's layers would silently leave the image.
    #[test]
    fn two_spellings_of_one_platform_are_refused() {
        use hcore::htvalue::Value;
        let s = |v: &str| Value::String(v.to_string());
        let err = format!(
            "{:#}",
            parse_image(&[
                ("platforms", Value::List(vec![s("linux/amd64")])),
                (
                    "layers_by_platform",
                    Value::Map(HashMap::from([
                        ("linux/amd64".to_string(), s(":a")),
                        ("linux/x86_64".to_string(), s(":b")),
                    ])),
                ),
            ])
            .err()
            .expect("two keys for one platform")
        );
        assert!(
            err.contains("\"linux/amd64\" and \"linux/x86_64\" name the same platform"),
            "names both spellings: {err}"
        );
    }
}
