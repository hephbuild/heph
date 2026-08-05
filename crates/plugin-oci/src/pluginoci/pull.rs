//! The `oci_pull` driver: pulls an image from a registry into a cacheable
//! archive output.
//!
//! The image-world analogue of `http_fetch`: bytes come from the network (no
//! target inputs), and the pulled archive is a cacheable target output — so a
//! base image shared by many `oci_image` builds is pulled once and served from
//! the local or remote cache thereafter.
//!
//! To actually *be* that shared base, a pull needs `layout = True`: that writes
//! an OCI layout directory, which `oci_image`'s `bases` wires to a buildx
//! `--build-context <name>=oci-layout://…` so the Dockerfile can `FROM <name>`.
//! Without it the pulled archive can only be pushed or loaded — a plain
//! `FROM alpine:3.20` still goes to the network, unhashed, whatever this target
//! pulled.
//!
//! Two things decide what a pull returns, and both are in the cache key:
//!
//! - the **ref**. Like `http_fetch` without a `sha256`, a **mutable tag** is
//!   only as reproducible as the registry: heph keys on the ref string, so if
//!   the tag later moves the stale archive is still served. The driver warns;
//!   pin by `@sha256:` for reproducibility.
//! - the **platform**. A registry ref usually names a manifest *list*, and both
//!   tools pick one instance out of it. Left implicit that choice follows the
//!   host — which would make an arm64 and an amd64 machine share one cache entry
//!   for two different images, and would fail outright on macOS, where skopeo
//!   would ask for a `darwin` instance no Linux image publishes. So the platform
//!   is always resolved to a concrete `os/arch` and always hashed.
//!
//!   `all_platforms = True` is the other half of that: it keeps the whole index
//!   rather than selecting one instance, which is what a base image for a
//!   **multi-platform** `oci_image` has to be — a one-instance layout has no
//!   manifest for the architectures it was not pulled for.

use anyhow::Context as _;
use async_trait::async_trait;
use hcore::debug_hash::DebugHasher;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path as OutPath};
use hplugin::driver::targetdef::{Output, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse,
};
use hplugin::htspec::{Spec, TargetSpecCache};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

use super::ws_path;

pub const DRIVER_NAME: &str = "oci_pull";

/// Config for an `oci_pull` target.
#[derive(Spec)]
struct OciPullSpec {
    /// Source image reference, e.g. `docker.io/library/alpine:3.20` or, pinned,
    /// `alpine@sha256:...`. Pin by digest for a reproducible pull.
    #[spec(required, rename = "ref")]
    src: String,
    /// Write an OCI **layout directory** instead of a single archive file. This
    /// is the form `oci_image`'s `bases` consumes — buildx's `oci-layout://`
    /// build context reads a layout tree, not a tar.
    ///
    /// This is the shape `oci_image`'s `bases` consumes.
    layout: bool,
    /// Image platform to pull out of a multi-platform manifest list, as
    /// `os/arch` (e.g. `linux/arm64`). Defaults to Linux on the host's
    /// architecture, and is always part of the cache key — otherwise an arm64
    /// and an amd64 machine would share one entry for two different images.
    ///
    /// Mutually exclusive with `all_platforms`.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    platform: Option<String>,
    /// Pull **every** instance of the manifest list instead of selecting one,
    /// keeping the index intact.
    ///
    /// This is what a base image for a multi-platform `oci_image` needs: a
    /// single-instance layout has no manifest for the other platforms, so
    /// `platforms = ["linux/amd64", "linux/arm64"]` would fail on whichever one
    /// the base was not pulled for. Pair it with `layout = True`:
    ///
    /// ```python
    /// oci_pull(name = "alpine", ref = "alpine:3.20", layout = True, all_platforms = True)
    /// oci_image(name = "img", bases = {"base": ":alpine"},
    ///           platforms = ["linux/amd64", "linux/arm64"])
    /// ```
    ///
    all_platforms: bool,
    /// Output filename (or directory name, with `layout = True`), relative to
    /// the target's package. Must be a bare name. Default `<target name>.tar`,
    /// or `<target name>.oci` for a layout.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    out: Option<String>,
    /// Pull from an insecure (HTTP / self-signed) registry: plain HTTP, and
    /// certificate validation off.
    insecure: bool,
    /// Caching for the pulled archive. Defaults to on for both tiers. A pull is
    /// content-addressed only when the ref is digest-pinned.
    cache: TargetSpecCache,
}

/// Which instance(s) of a manifest list a pull takes.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum PlatformSelect {
    /// One concrete `os/arch`, pinned with skopeo's override flags.
    One(String),
    /// Every instance, index intact (`--multi-arch all`).
    All,
}

impl PlatformSelect {
    /// Stable label for hashing and for the `oci_pull: pulled` log line.
    fn label(&self) -> &str {
        match self {
            PlatformSelect::One(p) => p,
            PlatformSelect::All => "all",
        }
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct OciPullDef {
    src: String,
    /// Workspace-relative output archive (or layout directory) path.
    out: String,
    layout: bool,
    /// Which instance of the manifest list is pulled. Never "whatever the host
    /// is" — that is what makes the key honest.
    platform: PlatformSelect,
    insecure: bool,
}

/// v3: `all_platforms` pulls the whole index, so the platform selection is no
/// longer a single `os/arch` string.
/// v4: pulled in-process over the distribution protocol; `tool` and `format` are
/// gone, so neither is in the key any more.
const OCI_PULL_FORMAT_VERSION: u32 = 4;

impl Hash for OciPullDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        OCI_PULL_FORMAT_VERSION.hash(state);
        self.src.hash(state);
        self.out.hash(state);
        self.layout.hash(state);
        self.platform.label().hash(state);
        self.insecure.hash(state);
    }
}

/// True when `ref` is pinned to a content digest (`…@sha256:<64 hex>`), the only
/// form that makes the ref string a sound cache key.
///
/// A bare `@` is not enough: `alpine@latest` contains one and pins nothing.
fn is_digest_pinned(image_ref: &str) -> bool {
    let Some((_, digest)) = image_ref.rsplit_once('@') else {
        return false;
    };
    let Some((algo, hex)) = digest.split_once(':') else {
        return false;
    };
    !algo.is_empty() && hex.len() >= 32 && hex.chars().all(|c| c.is_ascii_hexdigit())
}

/// Stateless: the registry client is built per pull from the target's own
/// config, and there is no host binary left to point anywhere.
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
        OciPullSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let addr = &req.target_spec.addr;
        let spec = OciPullSpec::from(&req.target_spec.config).context("parse oci_pull config")?;
        let platform = match (spec.all_platforms, spec.platform) {
            (true, Some(p)) => anyhow::bail!(
                "`all_platforms = True` pulls every instance, so `platform` ({p:?}) has nothing to \
                 select; drop one of them"
            ),
            (true, None) => PlatformSelect::All,
            // Validate now rather than at run time: a malformed platform is a
            // typo in the BUILD file, and parse is where the user finds out.
            (false, Some(p)) => {
                let p = super::normalize_platform(&p).context("`platform`")?;
                PlatformSelect::One(p)
            }
            (false, None) => PlatformSelect::One(super::default_platform()),
        };

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
            "`out` {out_rel:?} must be a bare name (no directory component): the driver writes it \
             into the target's package directory"
        );
        let out = ws_path(addr.package.as_str(), &out_rel);

        // Fail open, but warn: an unpinned tag makes the cache key (the ref
        // string) lie if the tag later moves — same tradeoff as an http_fetch
        // without sha256. A `@` alone is not enough; require a real digest so
        // `alpine@latest` (which is not a pin) still warns.
        if !is_digest_pinned(&spec.src) {
            tracing::warn!(
                image = spec.src,
                "oci_pull: pulling a mutable tag {:?} — heph caches on the ref string, so a \
                 moved tag serves the stale archive; pin the ref by @sha256:digest to make the \
                 pull reproducible",
                spec.src
            );
        }

        let def = OciPullDef {
            src: spec.src,
            out: out.clone(),
            layout: spec.layout,
            platform,
            insecure: spec.insecure,
        };
        let hash = {
            let mut h =
                DebugHasher::new(Xxh3Default::new(), || format!("oci_pull_{}", addr.format()));
            def.hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                // No inputs: the bytes come from the registry, not other targets.
                inputs: vec![],
                outputs: vec![Output {
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
                }],
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
        let def = req.request.target.def_de::<OciPullDef>().clone();
        let out_name = std::path::Path::new(&def.out)
            .file_name()
            .with_context(|| format!("out {:?} has no file name", def.out))?;
        let out_path = req.sandbox_pkg_dir.join(out_name);

        let (index, blobs) = super::registry::pull_layout(&def.src, def.insecure)
            .await
            .with_context(|| format!("pull image {}", def.src))?;

        if def.layout {
            // A layout *directory* is the shape `oci_image`'s `bases` consumes:
            // buildx's `oci-layout://` reads a tree, not a tar.
            super::archive::write_layout_dir(&out_path, &index, &blobs)
        } else {
            super::archive::write_layout_tar(&out_path, &index, &blobs)
        }
        .with_context(|| format!("write the pulled image to {out_path:?}"))?;

        tracing::info!(
            image = def.src,
            platform = def.platform.label(),
            "oci_pull: pulled"
        );
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

    fn parse_req(addr: &str, config: HashMap<String, Value>) -> ParseRequest {
        ParseRequest {
            request_id: "test".to_string(),
            target_spec: Arc::new(TargetSpec {
                addr: parse_addr(addr).expect("addr"),
                driver: DRIVER_NAME.to_string(),
                config,
                ..Default::default()
            }),
        }
    }

    fn cfg(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    async fn parse(addr: &str, config: HashMap<String, Value>) -> ParseResponse {
        Driver::new()
            .parse(parse_req(addr, config), &StdCancellationToken::new())
            .await
            .expect("parse")
    }

    /// A digest-pinned ref, 64 hex chars — the form `is_digest_pinned` accepts.
    const PINNED: &str =
        "alpine@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// Only a real content digest counts as a pin. `@latest` contains an `@`
    /// and pins nothing, so it must still warn.
    #[test]
    fn digest_pin_detection_requires_a_real_digest() {
        assert!(is_digest_pinned(PINNED));
        assert!(!is_digest_pinned("alpine@latest"));
        assert!(!is_digest_pinned("alpine:3.20"));
        assert!(!is_digest_pinned("alpine@sha256:nothex"));
    }

    #[tokio::test]
    async fn parse_declares_no_inputs_one_output_and_cached() {
        let resp = parse(
            "//base:alpine",
            cfg(&[("ref", Value::String(PINNED.to_string()))]),
        )
        .await;
        assert!(resp.target_def.inputs.is_empty());
        assert_eq!(resp.target_def.outputs.len(), 1);
        // Named for the target, so two pulls in one package do not collide.
        assert!(matches!(
            &resp.target_def.outputs[0].paths[0].content,
            Content::FilePath(p) if p == "base/alpine.tar"
        ));
        assert!(resp.target_def.cache.enabled);
        assert!(resp.target_def.cache.remote_enabled);
    }

    /// The platform is part of the key: an arm64 host and an amd64 host must not
    /// share one cache entry for two different images.
    #[tokio::test]
    async fn parse_hash_differs_per_platform() {
        let a = parse(
            "//base:x",
            cfg(&[
                ("ref", Value::String(PINNED.to_string())),
                ("platform", Value::String("linux/amd64".to_string())),
            ]),
        )
        .await;
        let b = parse(
            "//base:x",
            cfg(&[
                ("ref", Value::String(PINNED.to_string())),
                ("platform", Value::String("linux/arm64".to_string())),
            ]),
        )
        .await;
        assert_ne!(
            a.target_def.hash, b.target_def.hash,
            "the pulled platform must be part of the cache key"
        );
    }

    /// An implicit pull records a concrete platform rather than leaving it to
    /// whatever the host happens to be at run time.
    #[tokio::test]
    async fn parse_defaults_platform_to_linux_host_arch() {
        let resp = parse(
            "//base:x",
            cfg(&[("ref", Value::String(PINNED.to_string()))]),
        )
        .await;
        let def = resp.target_def.def::<OciPullDef>();
        assert_eq!(
            def.platform,
            PlatformSelect::One(super::super::default_platform())
        );
        assert!(
            def.platform.label().starts_with("linux/"),
            "container images are linux images even on a mac host: {}",
            def.platform.label()
        );
    }

    /// `layout = True` produces the OCI layout *directory* that `oci_image`'s
    /// `bases` (buildx `oci-layout://`) can consume — a tar cannot be used there.
    #[tokio::test]
    async fn parse_layout_declares_a_directory_output() {
        let resp = parse(
            "//base:alpine",
            cfg(&[
                ("ref", Value::String(PINNED.to_string())),
                ("layout", Value::Bool(true)),
            ]),
        )
        .await;
        assert!(matches!(
            &resp.target_def.outputs[0].paths[0].content,
            Content::DirPath(p) if p == "base/alpine.oci"
        ));
    }

    /// The multi-arch base recipe: an all-platforms layout, which is what a
    /// `platforms = [...]` build needs from its `bases` entry.
    #[tokio::test]
    async fn parse_all_platforms_selects_the_whole_index() {
        let resp = parse(
            "//base:alpine",
            cfg(&[
                ("ref", Value::String(PINNED.to_string())),
                ("layout", Value::Bool(true)),
                ("all_platforms", Value::Bool(true)),
            ]),
        )
        .await;
        assert_eq!(
            resp.target_def.def::<OciPullDef>().platform,
            PlatformSelect::All
        );
    }

    /// A one-instance archive and a full index are different bytes under the same
    /// ref — they must not share a cache entry.
    #[tokio::test]
    async fn parse_hash_differs_between_all_platforms_and_one() {
        let one = parse(
            "//base:x",
            cfg(&[("ref", Value::String(PINNED.to_string()))]),
        )
        .await;
        let all = parse(
            "//base:x",
            cfg(&[
                ("ref", Value::String(PINNED.to_string())),
                ("all_platforms", Value::Bool(true)),
            ]),
        )
        .await;
        assert_ne!(one.target_def.hash, all.target_def.hash);
    }

    /// Asking for every instance *and* naming one is a contradiction; honouring
    /// either silently would hand back an archive the BUILD file did not ask for.
    #[tokio::test]
    async fn parse_all_platforms_and_platform_are_exclusive() {
        let err = Driver::new()
            .parse(
                parse_req(
                    "//base:x",
                    cfg(&[
                        ("ref", Value::String(PINNED.to_string())),
                        ("all_platforms", Value::Bool(true)),
                        ("platform", Value::String("linux/amd64".to_string())),
                    ]),
                ),
                &StdCancellationToken::new(),
            )
            .await
            .err()
            .expect("all_platforms + platform must fail");
        assert!(format!("{err:#}").contains("all_platforms"), "got: {err:#}");
    }

    /// `out` names a file in the package dir; a subdirectory would declare an
    /// output path that `run` never writes.
    #[tokio::test]
    async fn parse_rejects_out_with_a_directory_component() {
        let err = Driver::new()
            .parse(
                parse_req(
                    "//base:x",
                    cfg(&[
                        ("ref", Value::String(PINNED.to_string())),
                        ("out", Value::String("sub/image.tar".to_string())),
                    ]),
                ),
                &StdCancellationToken::new(),
            )
            .await
            .err()
            .expect("a nested `out` must fail");
        assert!(format!("{err:#}").contains("bare name"), "got: {err:#}");
    }

    #[tokio::test]
    async fn parse_requires_ref() {
        let err = Driver::new()
            .parse(
                parse_req("//base:x", cfg(&[])),
                &StdCancellationToken::new(),
            )
            .await
            .err()
            .expect("missing ref must fail");
        assert!(format!("{err:#}").contains("ref"), "got: {err:#}");
    }

    /// Different refs are distinct cache entries.
    #[tokio::test]
    async fn parse_hash_differs_per_ref() {
        let a = parse(
            "//base:x",
            cfg(&[("ref", Value::String("a@sha256:1".to_string()))]),
        )
        .await;
        let b = parse(
            "//base:x",
            cfg(&[("ref", Value::String("b@sha256:2".to_string()))]),
        )
        .await;
        assert_ne!(a.target_def.hash, b.target_def.hash);
    }
}
