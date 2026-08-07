//! The `oci_index` driver: groups per-platform images into one multi-platform
//! image.
//!
//! # What it is for
//!
//! `docker_build(platforms = [...])` is **one** buildx invocation. One
//! Dockerfile, one context, one set of build args, one `--target` stage, for
//! every platform at once. `context_by_platform` lets the *deps* differ, and
//! `TARGETPLATFORM` lets the Dockerfile branch — but the build is still one
//! recipe, and when the platforms need genuinely different ones (a different
//! base, a different package manager, a stage that only exists on one arch)
//! there is nowhere to put the difference.
//!
//! So build them separately and group them here:
//!
//! ```python
//! docker_build(name = "amd64", dockerfile = ":Dockerfile.amd64",
//!              platforms = ["linux/amd64"], context = [...])
//! docker_build(name = "arm64", dockerfile = ":Dockerfile.arm64",
//!              platforms = ["linux/arm64"], context = [...])
//!
//! oci_index(name = "img", images = [":amd64", ":arm64"])
//! ```
//!
//! `//pkg:img` is then one image everywhere downstream: `oci_push` sends the
//! whole manifest list under one tag, `oci_load` picks the instance for the
//! host, and `docker_build`'s `bases` resolves the right platform out of it.
//! The split is a build-time detail that does not leak into what the repo
//! refers to.
//!
//! # Why a separate rule
//!
//! It could have been an attribute on `docker_build` — and then a
//! single-Dockerfile multi-platform build and an N-Dockerfile one would be the
//! same rule in two modes, with half the attributes inert in each. Grouping is
//! also not a *build*: nothing is compiled, nothing is executed, no daemon is
//! touched. This rule reads the layouts its deps produced, writes one index
//! over them, and copies blobs by reference.
//!
//! It takes anything that produces a layout, not just `docker_build` —
//! `oci_image`, `oci_pull`, or another `oci_index`. Restricting it would be
//! arbitrary, and mixing is useful: a hand-assembled `oci_image` for one
//! architecture beside a Dockerfile build for another is exactly the case that
//! has nowhere else to go.
//!
//! # What it does not do
//!
//! Nothing is rebuilt or re-hashed. The per-platform images keep their own
//! digests, so pushing the index pushes blobs the registry may already have,
//! and each input target caches on its own inputs — changing the arm64
//! Dockerfile does not rebuild amd64.

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
use oci_client::manifest::{ImageIndexEntry, OciImageIndex, OciImageManifest, Platform};
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

use super::archive::{self, Blobs, Layout};
use super::{ImageFormat, layout_path, ws_path};

pub const DRIVER_NAME: &str = "oci_index";

fn image_origin(i: usize) -> String {
    format!("images|{i}")
}

/// Groups per-platform images into one multi-platform image. Nothing is built:
/// use it when the platforms need different `docker_build` recipes.
#[derive(Spec)]
struct OciIndexSpec {
    /// The images to group, one per platform — `docker_build`, `oci_image`,
    /// `oci_pull` or another `oci_index` target.
    ///
    /// Each contributes every instance it holds, so grouping two single-platform
    /// builds is the common case but a multi-platform input is merged rather
    /// than rejected. Two inputs claiming the same platform is an error: one
    /// would silently shadow the other, and which one won would depend on the
    /// order they happen to be written in.
    #[spec(required)]
    images: Vec<String>,
    /// Archive format: `oci` (default). `docker` is rejected — a docker-format
    /// archive holds a single image, which is the one thing this rule does not
    /// produce.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    format: Option<String>,
    /// Write an OCI **layout directory** instead of a single archive file. This
    /// is the shape `docker_build`'s `bases` and buildx's `oci-layout://` build
    /// context consume.
    layout: bool,
    /// Output filename (or directory name, with `layout = True`), relative to
    /// the target's package. Must be a bare name. Default `<target name>.tar`,
    /// or `<target name>.oci` for a layout.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    out: Option<String>,
    /// Caching for the grouped image. Defaults to on for both tiers.
    cache: TargetSpecCache,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct OciIndexDef {
    /// Workspace-relative output path.
    out: String,
    /// Workspace-relative digest output path.
    digest_out: String,
    layout: bool,
    format: ImageFormat,
    /// Normalized `images` addresses, **in order**.
    ///
    /// The addresses, because `hashin` cannot carry them: it folds a *sorted,
    /// unlabeled multiset* of dep hashouts (`engine/meta.rs`) with no origin,
    /// no address and no output-group selector. Order matters here too — the
    /// manifest list's entry order is part of its bytes, and therefore of the
    /// image's digest.
    images: Vec<String>,
}

/// Bump to invalidate cached indexes when the emitted bytes change shape.
const OCI_INDEX_FORMAT_VERSION: u32 = 1;

impl Hash for OciIndexDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        OCI_INDEX_FORMAT_VERSION.hash(state);
        self.out.hash(state);
        self.digest_out.hash(state);
        self.layout.hash(state);
        self.format.output_type().hash(state);
        self.images.hash(state);
    }
}

/// The platform an image manifest is for.
///
/// The index entry's annotation is preferred when it has one, but it often does
/// not: a single-platform `--output type=oci` build writes an index whose entry
/// carries no platform at all. The image **config** always does — `architecture`
/// and `os` are required fields — so that is the fallback, and it is the
/// authoritative answer either way since it is what a runtime reads.
fn platform_of(
    layout: &Layout,
    manifest: &OciImageManifest,
    declared: Option<&Platform>,
) -> anyhow::Result<Platform> {
    if let Some(p) = declared {
        return Ok(p.clone());
    }
    let raw = layout
        .blob_bytes(&manifest.config.digest)
        .with_context(|| format!("read the image config {}", manifest.config.digest))?;
    let config: serde_json::Value =
        serde_json::from_slice(&raw).context("parse the image config")?;
    let os = config
        .get("os")
        .and_then(serde_json::Value::as_str)
        .context("the image config has no `os`, so there is no platform to file it under")?;
    let architecture = config
        .get("architecture")
        .and_then(serde_json::Value::as_str)
        .context("the image config has no `architecture`")?;
    Ok(Platform {
        os: os.into(),
        architecture: architecture.into(),
        // Carried through when the config states one: `linux/arm/v7` and
        // `linux/arm/v6` are different machines and must stay different entries.
        variant: config
            .get("variant")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        os_version: config
            .get("os.version")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        os_features: None,
        features: None,
    })
}

/// `os/arch[/variant]`, for error messages and duplicate detection.
fn platform_label(p: &Platform) -> String {
    match &p.variant {
        Some(v) if !v.is_empty() => format!("{}/{}/{}", p.os, p.architecture, v),
        _ => format!("{}/{}", p.os, p.architecture),
    }
}

/// Stateless: an index is assembled from declared inputs and nothing else.
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
            name: DRIVER_NAME.to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        OciIndexSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let addr = &req.target_spec.addr;
        let pkg = addr.package.to_owned();
        let spec = OciIndexSpec::from(&req.target_spec.config).context("parse oci_index config")?;

        anyhow::ensure!(
            !spec.images.is_empty(),
            "`images` is empty; an oci_index groups per-platform images and needs at least one"
        );
        let format = spec
            .format
            .as_deref()
            .map_or(Ok(ImageFormat::Oci), ImageFormat::parse)?;
        anyhow::ensure!(
            !matches!(format, ImageFormat::Docker),
            "`format = \"docker\"` holds a single image, which is the one thing an oci_index does \
             not produce. Use the default `format = \"oci\"`, and `oci_load` to put one instance \
             of it in a daemon."
        );

        let mut inputs = Vec::new();
        let mut images = Vec::new();
        for (i, image) in spec.images.iter().enumerate() {
            let mut r#ref = TargetAddr::parse(image, &pkg)
                .with_context(|| format!("parse `images` entry {image:?}"))?;
            // Pin to the archive group. Unpinned, the dep also stages the
            // `digest` group's text file, and the layout root can no longer be
            // found by its marker among two unrelated files.
            super::pin_archive_group(&mut r#ref, image)?;
            images.push(r#ref.to_string());
            inputs.push(Input {
                r#ref,
                mode: InputMode::Standard,
                origin_id: image_origin(i),
                // Out of the workspace root: these are whole images, and
                // unpacking them over the tree would put one target's layers in
                // another's build context.
                annotations: BTreeMap::from([(
                    "unpack_root".to_string(),
                    format!("oci_index_{i}"),
                )]),
                hashed: true,
                runtime: true,
            });
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

        let def = OciIndexDef {
            out: out.clone(),
            digest_out: digest_out.clone(),
            layout: spec.layout,
            format,
            images,
        };
        let hash = {
            let mut h = DebugHasher::new(Xxh3Default::new(), || {
                format!("oci_index_{}", addr.format())
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
                support_files: vec![],
                cache: spec.cache.into(),
                pty: false,
                hash,
                transparent: false,
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
        let def = req.request.target.def_de::<OciIndexDef>().clone();
        let out_path = req.sandbox_pkg_dir.join(super::basename(&def.out)?);

        let mut blobs: Blobs = Blobs::new();
        let mut entries: Vec<ImageIndexEntry> = Vec::new();
        // Which target contributed each platform, so a collision names both.
        let mut seen: BTreeMap<String, String> = BTreeMap::new();

        for (i, addr) in def.images.iter().enumerate() {
            let origin = image_origin(i);
            let path = layout_path(&req, &origin, "`images`")?;
            let layout = Layout::read(&path)
                .with_context(|| format!("read the image {addr} at {path:?}"))?;
            let manifests = layout
                .manifests()
                .with_context(|| format!("read the manifests of {addr}"))?;
            anyhow::ensure!(
                !manifests.is_empty(),
                "`images` entry {addr} holds no manifests; it is not an image"
            );

            for (manifest, declared, digest) in manifests {
                let platform = platform_of(&layout, &manifest, declared.as_ref())
                    .with_context(|| format!("determine the platform of {addr}"))?;
                let label = platform_label(&platform);
                if let Some(other) = seen.insert(label.clone(), addr.clone()) {
                    // Silently keeping one would make which image ships depend
                    // on the order `images` happens to be written in.
                    anyhow::bail!(
                        "{other} and {addr} both provide {label}. An index holds one image per \
                         platform, so one would shadow the other. Drop one, or narrow its \
                         `platforms`."
                    );
                }

                // Carried by reference: with the blobs located rather than
                // loaded, grouping N images copies no layer bytes into memory.
                // Deduped by digest, so a base layer shared between platforms is
                // stored once.
                let raw = layout.blob(&digest)?.clone();
                let size = raw.len()? as i64;
                blobs.insert(digest.clone(), raw);
                blobs.insert(
                    manifest.config.digest.clone(),
                    layout.blob(&manifest.config.digest)?.clone(),
                );
                for layer in &manifest.layers {
                    blobs
                        .entry(layer.digest.clone())
                        .or_insert(layout.blob(&layer.digest)?.clone());
                }

                entries.push(ImageIndexEntry {
                    media_type: manifest
                        .media_type
                        .clone()
                        .unwrap_or_else(|| oci_client::manifest::OCI_IMAGE_MEDIA_TYPE.to_string()),
                    artifact_type: None,
                    digest,
                    size,
                    platform: Some(platform),
                    annotations: None,
                });
            }
        }

        let index = OciImageIndex {
            schema_version: 2,
            media_type: Some(oci_client::manifest::OCI_IMAGE_INDEX_MEDIA_TYPE.to_string()),
            artifact_type: None,
            manifests: entries.clone(),
            annotations: None,
        };

        if def.layout {
            archive::write_layout_dir_blobs(&out_path, &index, &blobs)
        } else {
            archive::write_layout_tar_blobs(&out_path, &index, &blobs)
        }
        .with_context(|| format!("write the grouped image to {out_path:?}"))?;

        // The list's digest, which is what a registry files the tag under — even
        // when only one image was grouped, since the shape is still a list.
        let raw = serde_json::to_vec(&index).context("encode the manifest list")?;
        let digest = archive::sha256_digest(&raw);
        std::fs::write(
            req.sandbox_pkg_dir.join(super::basename(&def.digest_out)?),
            &digest,
        )
        .context("write the digest output")?;

        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use hcore::htvalue::Value;
    use hmodel::htaddr::parse_addr;
    use hplugin::provider::TargetSpec;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    fn images(list: &[&str]) -> Value {
        Value::List(
            list.iter()
                .map(|s| Value::String((*s).to_string()))
                .collect(),
        )
    }

    async fn parse_at(addr: &str, config: HashMap<String, Value>) -> anyhow::Result<ParseResponse> {
        Driver::new()
            .parse(
                ParseRequest {
                    request_id: "test".to_string(),
                    target_spec: Arc::new(TargetSpec {
                        addr: parse_addr(addr).expect("addr"),
                        driver: DRIVER_NAME.to_string(),
                        config,
                        ..Default::default()
                    }),
                },
                &StdCancellationToken::new(),
            )
            .await
    }

    fn def_hash(d: &OciIndexDef) -> u64 {
        let mut h = Xxh3Default::new();
        d.hash(&mut h);
        h.finish()
    }

    fn bare() -> OciIndexDef {
        OciIndexDef {
            out: "app/img.tar".to_string(),
            digest_out: "app/img.digest".to_string(),
            layout: false,
            format: ImageFormat::Oci,
            images: vec![],
        }
    }

    /// Each grouped image is a dep, and both output groups are declared — the
    /// digest without unpacking the archive, exactly as `docker_build` and
    /// `oci_image` offer.
    #[tokio::test]
    async fn parse_declares_a_dep_per_image_and_two_outputs() {
        let resp = parse_at(
            "//app:img",
            cfg(&[("images", images(&[":amd64", ":arm64"]))]),
        )
        .await
        .expect("parse");
        let def = resp.target_def.def::<OciIndexDef>();
        // Pinned to the archive group (`|`), never the sibling `digest` one.
        assert_eq!(def.images, vec!["//app:amd64|", "//app:arm64|"]);
        assert_eq!(resp.target_def.inputs.len(), 2);
        let groups: Vec<&str> = resp
            .target_def
            .outputs
            .iter()
            .map(|o| o.group.as_str())
            .collect();
        assert_eq!(groups, vec!["", "digest"]);
    }

    /// The manifest list's entry order is part of its bytes, so it is part of
    /// the digest — and `hashin` cannot carry it: it folds a sorted, unlabeled
    /// multiset of dep hashouts with no address and no output-group selector.
    #[test]
    fn the_order_and_identity_of_the_images_reach_the_key() {
        let mut ab = bare();
        ab.images = vec!["//app:a".to_string(), "//app:b".to_string()];
        let mut ba = bare();
        ba.images = vec!["//app:b".to_string(), "//app:a".to_string()];
        assert_ne!(def_hash(&ab), def_hash(&ba), "entry order is the digest");

        let mut rel = bare();
        rel.images = vec!["//app:a|release".to_string()];
        let mut dbg = bare();
        dbg.images = vec!["//app:a|debug".to_string()];
        assert_ne!(
            def_hash(&rel),
            def_hash(&dbg),
            "the output-group selector reaches no other part of the key"
        );

        let mut as_layout = bare();
        as_layout.layout = true;
        assert_ne!(def_hash(&bare()), def_hash(&as_layout));
    }

    /// A docker-format archive holds one image, which is the one thing this
    /// rule does not produce — so the mismatch is refused where it is written,
    /// not discovered by whatever tries to read the result.
    #[tokio::test]
    async fn docker_format_is_refused() {
        let err = parse_at(
            "//app:img",
            cfg(&[
                ("images", images(&[":amd64"])),
                ("format", Value::String("docker".to_string())),
            ]),
        )
        .await
        .err()
        .expect("docker format must fail");
        assert!(format!("{err:#}").contains("single image"), "got: {err:#}");
    }

    #[tokio::test]
    async fn an_empty_index_is_refused() {
        let err = parse_at("//app:img", cfg(&[("images", images(&[]))]))
            .await
            .err()
            .expect("empty must fail");
        assert!(format!("{err:#}").contains("at least one"), "got: {err:#}");
    }

    /// A platform is read off the image config when the index entry carries no
    /// annotation — which is the normal case for a single-platform build, so
    /// this is the path that runs most of the time, not the fallback.
    #[test]
    fn the_platform_comes_from_the_config_when_the_entry_omits_it() {
        let config = serde_json::json!({
            "architecture": "arm",
            "os": "linux",
            "variant": "v7",
            "rootfs": {"type": "layers", "diff_ids": []},
        });
        let config_bytes = serde_json::to_vec(&config).expect("config");
        let config_digest = archive::sha256_digest(&config_bytes);
        let manifest: OciImageManifest = serde_json::from_value(serde_json::json!({
            "schemaVersion": 2,
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": config_digest,
                "size": config_bytes.len(),
            },
            "layers": [],
        }))
        .expect("manifest");

        let layout = Layout {
            index: OciImageIndex {
                schema_version: 2,
                media_type: None,
                artifact_type: None,
                manifests: vec![],
                annotations: None,
            },
            blobs: Blobs::from([(config_digest, archive::Blob::Bytes(config_bytes))]),
        };

        let got = platform_of(&layout, &manifest, None).expect("platform");
        assert_eq!(platform_label(&got), "linux/arm/v7");

        // A declared platform wins without the config being read at all.
        let declared = Platform {
            os: "linux".into(),
            architecture: "amd64".into(),
            variant: None,
            os_version: None,
            os_features: None,
            features: None,
        };
        let got = platform_of(&layout, &manifest, Some(&declared)).expect("platform");
        assert_eq!(platform_label(&got), "linux/amd64");
    }

    /// `linux/arm/v7` and `linux/arm/v6` are different machines: the variant has
    /// to stay in the label, or they would be reported as a duplicate and one of
    /// them dropped.
    #[test]
    fn variants_are_distinct_platforms() {
        let p = |variant: Option<&str>| Platform {
            os: "linux".into(),
            architecture: "arm".into(),
            variant: variant.map(str::to_string),
            os_version: None,
            os_features: None,
            features: None,
        };
        assert_ne!(
            platform_label(&p(Some("v7"))),
            platform_label(&p(Some("v6")))
        );
        assert_eq!(platform_label(&p(None)), "linux/arm");
        assert_eq!(platform_label(&p(Some(""))), "linux/arm");
    }
}
