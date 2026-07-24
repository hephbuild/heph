//! The `oci_pull` driver: pulls an image from a registry into a cacheable
//! archive output.
//!
//! The image-world analogue of `http_fetch`: bytes come from the network (no
//! target inputs), and the pulled archive is a content-addressed, cacheable
//! target output — so a base image shared by many `oci_image` builds is pulled
//! once and served from the local or remote cache thereafter.
//!
//! Like `http_fetch` without a `sha256`, a pull of a **mutable tag** (no
//! `@sha256:` digest) is only as reproducible as the registry: heph keys the
//! cache on the ref string, so if the tag later moves the stale archive is still
//! served. The driver warns; pin the ref by digest for reproducibility.

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

use super::{ImageFormat, Tool, ensure_tool_supports_format, run_cmd_cancellable};

pub const DRIVER_NAME: &str = "oci_pull";

/// Config for an `oci_pull` target.
#[derive(Spec)]
struct OciPullSpec {
    /// Source image reference, e.g. `docker.io/library/alpine:3.20` or, pinned,
    /// `alpine@sha256:...`. Pin by digest for a reproducible pull.
    #[spec(required, rename = "ref")]
    src: String,
    /// Output archive format: `oci` (default) or `docker`.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    format: Option<String>,
    /// Output filename, relative to the target's package. Default `image.tar`.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    out: Option<String>,
    /// Pull from an insecure (HTTP / self-signed) registry — passes
    /// `--src-tls-verify=false` to skopeo. Ignored by the `docker` tool, which
    /// takes insecure registries from the daemon config.
    insecure: bool,
    /// Tool to pull with: `skopeo` (default for an `oci` archive) or `docker`
    /// (default for a `docker` archive — `docker pull` + `docker save`, no
    /// skopeo needed). `docker` can only produce a `docker` archive.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    tool: Option<String>,
    /// Caching for the pulled archive. Defaults to on for both tiers. A pull is
    /// content-addressed only when the ref is digest-pinned.
    cache: TargetSpecCache,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct OciPullDef {
    src: String,
    format: ImageFormat,
    /// Workspace-relative output archive path.
    out: String,
    insecure: bool,
    tool: Tool,
}

const OCI_PULL_FORMAT_VERSION: u32 = 1;

impl Hash for OciPullDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        OCI_PULL_FORMAT_VERSION.hash(state);
        self.src.hash(state);
        self.format.transport().hash(state);
        self.out.hash(state);
        self.insecure.hash(state);
        self.tool.label().hash(state);
    }
}

/// Pull an image into a docker-format archive with the docker CLI: pull it into
/// the daemon, then `docker save` it to `out_tar`. No skopeo.
async fn docker_pull(
    docker_bin: &str,
    src: &str,
    out_tar: &std::path::Path,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<()> {
    run_cmd_cancellable(
        vec![docker_bin.to_string(), "pull".to_string(), src.to_string()],
        ctoken,
        "docker pull (oci_pull)",
    )
    .await?;
    run_cmd_cancellable(
        vec![
            docker_bin.to_string(),
            "save".to_string(),
            src.to_string(),
            "-o".to_string(),
            out_tar.to_string_lossy().into_owned(),
        ],
        ctoken,
        "docker save (oci_pull)",
    )
    .await?;
    Ok(())
}

/// Assemble the `skopeo copy` argv for a pull. Pure so it can be unit-tested
/// without skopeo. `argv[0]` is the skopeo binary.
fn pull_argv(
    skopeo_bin: &str,
    src: &str,
    format: ImageFormat,
    out_tar: &std::path::Path,
    insecure: bool,
) -> Vec<String> {
    let mut argv = vec![
        skopeo_bin.to_string(),
        "copy".to_string(),
        "--insecure-policy".to_string(),
    ];
    if insecure {
        argv.push("--src-tls-verify=false".to_string());
    }
    argv.push(format!("docker://{src}"));
    argv.push(format!(
        "{}:{}",
        format.transport(),
        out_tar.to_string_lossy()
    ));
    argv
}

fn ws_path(pkg: &str, rel: &str) -> String {
    if pkg.is_empty() {
        rel.to_string()
    } else {
        format!("{pkg}/{rel}")
    }
}

pub struct Driver {
    skopeo_bin: String,
    docker_bin: String,
}

impl Default for Driver {
    fn default() -> Self {
        Driver::new()
    }
}

impl Driver {
    pub fn new() -> Self {
        Driver {
            skopeo_bin: "skopeo".to_string(),
            docker_bin: "docker".to_string(),
        }
    }

    #[cfg(test)]
    fn with_binaries(skopeo: impl Into<String>, docker: impl Into<String>) -> Self {
        Driver {
            skopeo_bin: skopeo.into(),
            docker_bin: docker.into(),
        }
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
        let spec =
            OciPullSpec::from(req.target_spec.config.clone()).context("parse oci_pull config")?;
        let format = ImageFormat::parse(spec.format.as_deref().unwrap_or("oci"))?;
        let tool = Tool::parse_opt(spec.tool.as_deref(), format)?;
        ensure_tool_supports_format(tool, format)?;
        let out_rel = spec.out.unwrap_or_else(|| "image.tar".to_string());
        let out = ws_path(addr.package.as_str(), &out_rel);

        // Fail open, but warn: an unpinned tag makes the cache key (the ref
        // string) lie if the tag later moves — same tradeoff as an http_fetch
        // without sha256.
        if !spec.src.contains('@') {
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
            format,
            out: out.clone(),
            insecure: spec.insecure,
            tool,
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
                        content: Content::FilePath(out),
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
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let def = req.request.target.def_de::<OciPullDef>();
        let out_name = std::path::Path::new(&def.out)
            .file_name()
            .with_context(|| format!("out {:?} has no file name", def.out))?;
        let out_tar = req.sandbox_pkg_dir.join(out_name);

        match def.tool {
            Tool::Skopeo => {
                let argv = pull_argv(
                    &self.skopeo_bin,
                    &def.src,
                    def.format,
                    &out_tar,
                    def.insecure,
                );
                run_cmd_cancellable(argv, ctoken, "skopeo copy (oci_pull)")
                    .await
                    .with_context(|| format!("pull image {}", def.src))?;
            }
            Tool::Docker => {
                docker_pull(&self.docker_bin, &def.src, &out_tar, ctoken)
                    .await
                    .with_context(|| format!("pull image {}", def.src))?;
            }
        }
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

    #[test]
    fn pull_argv_oci_from_registry() {
        let argv = pull_argv(
            "skopeo",
            "docker.io/library/alpine:3.20",
            ImageFormat::Oci,
            std::path::Path::new("/sbx/base/image.tar"),
            false,
        );
        assert_eq!(argv[0..3], ["skopeo", "copy", "--insecure-policy"]);
        let joined = argv.join(" ");
        assert!(
            joined.contains("docker://docker.io/library/alpine:3.20"),
            "{joined}"
        );
        assert!(
            joined.contains("oci-archive:/sbx/base/image.tar"),
            "{joined}"
        );
        assert!(!joined.contains("--src-tls-verify"), "{joined}");
    }

    #[test]
    fn pull_argv_docker_format_and_insecure() {
        let argv = pull_argv(
            "skopeo",
            "localhost:5000/app:dev",
            ImageFormat::Docker,
            std::path::Path::new("/t/image.tar"),
            true,
        );
        let joined = argv.join(" ");
        assert!(joined.contains("docker-archive:/t/image.tar"), "{joined}");
        assert!(joined.contains("--src-tls-verify=false"), "{joined}");
    }

    #[tokio::test]
    async fn parse_declares_no_inputs_one_output_and_cached() {
        let resp = parse(
            "//base:alpine",
            cfg(&[("ref", Value::String("alpine@sha256:abc".to_string()))]),
        )
        .await;
        assert!(resp.target_def.inputs.is_empty());
        assert_eq!(resp.target_def.outputs.len(), 1);
        assert!(matches!(
            &resp.target_def.outputs[0].paths[0].content,
            Content::FilePath(p) if p == "base/image.tar"
        ));
        assert!(resp.target_def.cache.enabled);
        assert!(resp.target_def.cache.remote_enabled);
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

    /// A docker-format pull defaults to the `docker` tool (docker pull + save) —
    /// no skopeo needed.
    #[tokio::test]
    async fn parse_docker_format_defaults_to_docker_tool() {
        let resp = parse(
            "//base:x",
            cfg(&[
                ("ref", Value::String("alpine@sha256:abc".to_string())),
                ("format", Value::String("docker".to_string())),
            ]),
        )
        .await;
        assert_eq!(resp.target_def.def::<OciPullDef>().tool, Tool::Docker);
    }

    /// docker save only makes a docker archive — docker+oci is rejected.
    #[tokio::test]
    async fn parse_docker_tool_with_oci_format_fails() {
        let err = Driver::new()
            .parse(
                parse_req(
                    "//base:x",
                    cfg(&[
                        ("ref", Value::String("alpine@sha256:abc".to_string())),
                        ("tool", Value::String("docker".to_string())),
                    ]),
                ),
                &StdCancellationToken::new(),
            )
            .await
            .err()
            .expect("docker+oci must fail");
        assert!(format!("{err:#}").contains("oci"), "got: {err:#}");
    }

    #[test]
    fn with_binaries_overrides() {
        let d = Driver::with_binaries("/fake/skopeo", "/fake/docker");
        assert_eq!(d.skopeo_bin, "/fake/skopeo");
        assert_eq!(d.docker_bin, "/fake/docker");
    }
}
