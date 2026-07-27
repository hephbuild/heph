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

use super::{ImageFormat, Tool, ToolIo, ensure_tool_supports_format, run_tool, ws_path};

pub const DRIVER_NAME: &str = "oci_pull";

/// Config for an `oci_pull` target.
#[derive(Spec)]
struct OciPullSpec {
    /// Source image reference, e.g. `docker.io/library/alpine:3.20` or, pinned,
    /// `alpine@sha256:...`. Pin by digest for a reproducible pull.
    #[spec(required, rename = "ref")]
    src: String,
    /// Output archive format: `oci` (default) or `docker`. Ignored when
    /// `layout = True`, which always writes an OCI layout.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    format: Option<String>,
    /// Write an OCI **layout directory** instead of a single archive file. This
    /// is the form `oci_image`'s `bases` consumes — buildx's `oci-layout://`
    /// build context reads a layout tree, not a tar.
    ///
    /// Requires `tool = "skopeo"` (the docker CLI cannot write a layout).
    layout: bool,
    /// Image platform to pull out of a multi-platform manifest list, as
    /// `os/arch` (e.g. `linux/arm64`). Defaults to Linux on the host's
    /// architecture, and is always part of the cache key — otherwise an arm64
    /// and an amd64 machine would share one entry for two different images.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    platform: Option<String>,
    /// Output filename (or directory name, with `layout = True`), relative to
    /// the target's package. Must be a bare name. Default `<target name>.tar`,
    /// or `<target name>.oci` for a layout.
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
    /// Workspace-relative output archive (or layout directory) path.
    out: String,
    layout: bool,
    /// The `os/arch` instance selected from the manifest list. Always concrete,
    /// never "whatever the host is" — that is what makes the key honest.
    platform: String,
    insecure: bool,
    tool: Tool,
}

/// v2: the selected platform is part of the key, and `layout` changes the output
/// shape.
const OCI_PULL_FORMAT_VERSION: u32 = 2;

impl Hash for OciPullDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        OCI_PULL_FORMAT_VERSION.hash(state);
        self.src.hash(state);
        self.format.transport().hash(state);
        self.out.hash(state);
        self.layout.hash(state);
        self.platform.hash(state);
        self.insecure.hash(state);
        self.tool.label().hash(state);
    }
}

/// The platform a bare `oci_pull` resolves to: Linux on the host's own
/// architecture.
///
/// The OS is pinned to `linux` rather than taken from the host because container
/// images are Linux images — on macOS, `skopeo` would otherwise ask a manifest
/// list for a `darwin` instance that does not exist and fail, while `docker`
/// would quietly get `linux` from the daemon's VM. Same target, two different
/// answers, one of them an error.
fn default_platform() -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("linux/{arch}")
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

/// Split `os/arch` into its parts.
fn split_platform(platform: &str) -> anyhow::Result<(&str, &str)> {
    // A platform may carry a variant (`linux/arm/v7`); only os and arch are
    // addressable by skopeo's override flags.
    let mut parts = platform.splitn(3, '/');
    match (parts.next(), parts.next()) {
        (Some(os), Some(arch)) if !os.is_empty() && !arch.is_empty() => Ok((os, arch)),
        _ => anyhow::bail!("`platform` must look like `os/arch`, got {platform:?}"),
    }
}

/// Pull an image into a docker-format archive with the docker CLI: pull it into
/// the daemon, then `docker save` it to `out_tar`. No skopeo.
async fn docker_pull(
    docker_bin: &str,
    src: &str,
    platform: &str,
    out_tar: &std::path::Path,
    cwd: &std::path::Path,
    io: &mut ToolIo<'_>,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<()> {
    run_tool(
        vec![
            docker_bin.to_string(),
            "pull".to_string(),
            "--platform".to_string(),
            platform.to_string(),
            src.to_string(),
        ],
        cwd,
        "docker pull (oci_pull)",
        io,
        ctoken,
    )
    .await?;
    run_tool(
        vec![
            docker_bin.to_string(),
            "save".to_string(),
            src.to_string(),
            "-o".to_string(),
            out_tar.to_string_lossy().into_owned(),
        ],
        cwd,
        "docker save (oci_pull)",
        io,
        ctoken,
    )
    .await?;
    Ok(())
}

/// Assemble the `skopeo copy` argv for a pull. Pure so it can be unit-tested
/// without skopeo. `argv[0]` is the skopeo binary.
///
/// `dest` is the already-formatted destination (`oci-archive:/path`,
/// `docker-archive:/path`, or `oci:/dir:latest` for a layout directory).
fn pull_argv(
    skopeo_bin: &str,
    src: &str,
    platform: &str,
    dest: &str,
    insecure: bool,
) -> anyhow::Result<Vec<String>> {
    let (os, arch) = split_platform(platform)?;
    let mut argv = vec![
        skopeo_bin.to_string(),
        "copy".to_string(),
        "--insecure-policy".to_string(),
        // Pin the instance chosen out of a manifest list. Without these skopeo
        // takes the host's GOOS/GOARCH — which is `darwin` on macOS, where no
        // Linux image has a matching instance.
        "--override-os".to_string(),
        os.to_string(),
        "--override-arch".to_string(),
        arch.to_string(),
    ];
    if insecure {
        argv.push("--src-tls-verify=false".to_string());
    }
    argv.push(format!("docker://{src}"));
    argv.push(dest.to_string());
    Ok(argv)
}

#[derive(Default)]
pub struct Driver {
    tools: super::Tools,
}

impl Driver {
    pub fn new() -> Self {
        Driver::default()
    }

    /// Point the driver at specific binaries. Public so tests — including
    /// out-of-crate e2e — can substitute fakes.
    pub fn with_tools(tools: super::Tools) -> Self {
        Driver { tools }
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

        if spec.layout && tool == Tool::Docker {
            anyhow::bail!(
                "`layout = True` requires tool = \"skopeo\": the docker CLI can only write a \
                 docker-format archive, not an OCI layout directory"
            );
        }
        if spec.insecure && tool == Tool::Docker {
            anyhow::bail!(
                "`insecure = True` is not supported with tool = \"docker\": the daemon decides \
                 which registries are insecure. Add the registry to the daemon's \
                 `insecure-registries`, or use tool = \"skopeo\"."
            );
        }

        let platform = spec.platform.unwrap_or_else(default_platform);
        // Validate now rather than at run time: a malformed platform is a typo
        // in the BUILD file, and parse is where the user finds out.
        split_platform(&platform)?;

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
            format,
            out: out.clone(),
            layout: spec.layout,
            platform,
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
        mut req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let def = req.request.target.def_de::<OciPullDef>().clone();
        let out_name = std::path::Path::new(&def.out)
            .file_name()
            .with_context(|| format!("out {:?} has no file name", def.out))?;
        let out_path = req.sandbox_pkg_dir.join(out_name);
        let cwd = req.sandbox_ws_dir.clone();
        let addr = req.request.target.addr.format();
        let mut io = ToolIo::from_request(&mut req.request);

        match def.tool {
            Tool::Skopeo => {
                let dest = if def.layout {
                    // `oci:<dir>:<tag>` writes an OCI layout tree. The tag is
                    // internal to the layout; `latest` keeps `FROM <name>`
                    // resolving without the user naming it.
                    format!("oci:{}:latest", out_path.to_string_lossy())
                } else {
                    format!("{}:{}", def.format.transport(), out_path.to_string_lossy())
                };
                let argv = pull_argv(
                    &self.tools.skopeo,
                    &def.src,
                    &def.platform,
                    &dest,
                    def.insecure,
                )?;
                run_tool(argv, &cwd, "skopeo copy (oci_pull)", &mut io, ctoken)
                    .await
                    .with_context(|| format!("pull image {}", def.src))?;
            }
            Tool::Docker => {
                docker_pull(
                    &self.tools.docker,
                    &def.src,
                    &def.platform,
                    &out_path,
                    &cwd,
                    &mut io,
                    ctoken,
                )
                .await
                .with_context(|| format!("pull image {}", def.src))?;
            }
        }
        tracing::info!(
            addr,
            image = def.src,
            platform = def.platform,
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

    #[test]
    fn pull_argv_oci_from_registry() {
        let argv = pull_argv(
            "skopeo",
            "docker.io/library/alpine:3.20",
            "linux/arm64",
            "oci-archive:/sbx/base/image.tar",
            false,
        )
        .expect("argv");
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

    /// The instance taken out of a manifest list is pinned explicitly. Without
    /// these, skopeo asks for the host's GOOS — `darwin` on macOS, which no
    /// Linux image publishes — so the same target fails there and silently picks
    /// the host arch on Linux.
    #[test]
    fn pull_argv_overrides_os_and_arch_from_platform() {
        let argv = pull_argv(
            "skopeo",
            "alpine:3.20",
            "linux/amd64",
            "oci-archive:/t/i.tar",
            false,
        )
        .expect("argv");
        let joined = argv.join(" ");
        assert!(joined.contains("--override-os linux"), "{joined}");
        assert!(joined.contains("--override-arch amd64"), "{joined}");
    }

    #[test]
    fn pull_argv_rejects_malformed_platform() {
        let err = pull_argv("skopeo", "alpine", "linux", "oci-archive:/t.tar", false)
            .expect_err("a platform without an arch must fail");
        assert!(format!("{err:#}").contains("os/arch"), "got: {err:#}");
    }

    #[test]
    fn pull_argv_docker_format_and_insecure() {
        let argv = pull_argv(
            "skopeo",
            "localhost:5000/app:dev",
            "linux/amd64",
            "docker-archive:/t/image.tar",
            true,
        )
        .expect("argv");
        let joined = argv.join(" ");
        assert!(joined.contains("docker-archive:/t/image.tar"), "{joined}");
        assert!(joined.contains("--src-tls-verify=false"), "{joined}");
    }

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
        assert_eq!(def.platform, default_platform());
        assert!(
            def.platform.starts_with("linux/"),
            "container images are linux images even on a mac host: {}",
            def.platform
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

    #[tokio::test]
    async fn parse_layout_with_docker_tool_fails() {
        let err = Driver::new()
            .parse(
                parse_req(
                    "//base:x",
                    cfg(&[
                        ("ref", Value::String(PINNED.to_string())),
                        ("layout", Value::Bool(true)),
                        ("format", Value::String("docker".to_string())),
                    ]),
                ),
                &StdCancellationToken::new(),
            )
            .await
            .err()
            .expect("docker cannot write an OCI layout");
        assert!(format!("{err:#}").contains("layout"), "got: {err:#}");
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

    /// `insecure` is a skopeo flag; accepting it and dropping it would leave the
    /// user believing TLS verification is off when it is not.
    #[tokio::test]
    async fn parse_insecure_with_docker_tool_fails() {
        let err = Driver::new()
            .parse(
                parse_req(
                    "//base:x",
                    cfg(&[
                        ("ref", Value::String(PINNED.to_string())),
                        ("format", Value::String("docker".to_string())),
                        ("insecure", Value::Bool(true)),
                    ]),
                ),
                &StdCancellationToken::new(),
            )
            .await
            .err()
            .expect("insecure + docker must fail");
        assert!(format!("{err:#}").contains("insecure"), "got: {err:#}");
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

    /// `layout = True` writes an OCI layout *tree* via skopeo's `oci:` transport
    /// — the form buildx's `oci-layout://` build context reads. An `oci-archive:`
    /// tar cannot be used there, so this is what makes `bases` work at all.
    #[tokio::test]
    async fn run_layout_uses_the_oci_directory_transport() {
        let sbx = super::super::testfake::Sandbox::new("base");
        let resp = parse(
            "//base:alpine",
            cfg(&[
                ("ref", Value::String(PINNED.to_string())),
                ("layout", Value::Bool(true)),
            ]),
        )
        .await;

        let skopeo = sbx.fake("skopeo", "exit 0");
        let rid = "req".to_string();
        let req = super::super::testfake::run_request(&rid, "hashin", &resp.target_def, &sbx, &[]);
        Driver::with_tools(super::super::Tools {
            docker: "docker".to_string(),
            skopeo,
        })
        .run(req, &StdCancellationToken::new())
        .await
        .expect("run");

        let call = sbx.calls().into_iter().next().expect("a skopeo call");
        assert!(
            call.contains(" oci:"),
            "expected the oci: transport: {call}"
        );
        assert!(call.contains("alpine.oci:latest"), "{call}");
        assert!(!call.contains("oci-archive:"), "{call}");
    }

    /// A failing pull surfaces the tool's own message.
    #[tokio::test]
    async fn run_surfaces_stderr_on_failure() {
        let sbx = super::super::testfake::Sandbox::new("base");
        let resp = parse(
            "//base:alpine",
            cfg(&[("ref", Value::String(PINNED.to_string()))]),
        )
        .await;

        let skopeo = sbx.fake("skopeo", "echo 'FATA[0000] unauthorized' >&2\nexit 1");
        let rid = "req".to_string();
        let req = super::super::testfake::run_request(&rid, "hashin", &resp.target_def, &sbx, &[]);
        let err = Driver::with_tools(super::super::Tools {
            docker: "docker".to_string(),
            skopeo,
        })
        .run(req, &StdCancellationToken::new())
        .await
        .err()
        .expect("a failing pull must error");
        assert!(format!("{err:#}").contains("unauthorized"), "got: {err:#}");
    }
}
