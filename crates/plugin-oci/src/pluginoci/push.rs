//! The `oci_push` driver: pushes an image archive (produced by an `oci_image`
//! target) to a registry.
//!
//! An *action*, not an artifact: it has an external side effect (the upload) and
//! is therefore **not cached** — it runs every time it is requested.
//!
//! Two tools (see [`Tool`]): `skopeo copy <transport>:<tar> docker://<ref>` is
//! daemonless, reads both OCI and docker archives, and skips blobs the registry
//! already has; the `docker` CLI path (`docker load` + `tag` + `push`) needs the
//! daemon and only handles docker-format archives, but keeps skopeo optional.

use anyhow::Context as _;
use async_trait::async_trait;
use hcore::debug_hash::DebugHasher;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hplugin::driver::targetdef::{CacheConfig, Input, InputMode, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, TargetAddr,
};
use hplugin::htspec::Spec;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

use super::{
    ImageFormat, Tool, ToolIo, dep_single_file, ensure_tool_supports_format, parse_docker_load_ref,
    run_tool,
};

pub const DRIVER_NAME: &str = "oci_push";

/// The `origin_id` of the single image-archive dep input.
const IMAGE_ORIGIN: &str = "image";

/// Config for an `oci_push` target.
#[derive(Spec)]
struct OciPushSpec {
    /// Target address of the image to push — an `oci_image` target. Only its
    /// archive output (group `""`) is consumed.
    #[spec(required)]
    image: String,
    /// Destination registry reference, e.g. `registry.io/me/app:1.2`.
    #[spec(required, rename = "ref")]
    dest: String,
    /// Source archive format: `oci` (default) or `docker`. Must match how the
    /// `oci_image` target was built.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    format: Option<String>,
    /// Push to an insecure (HTTP / self-signed) registry — passes
    /// `--dest-tls-verify=false` to skopeo. Ignored by the `docker` tool, which
    /// takes insecure registries from the daemon config.
    insecure: bool,
    /// Tool to push with: `skopeo` (default for an `oci` archive) or `docker`
    /// (default for a `docker` archive — `docker load` + `tag` + `push`, no
    /// skopeo needed). `docker` cannot push an `oci` archive.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    tool: Option<String>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct OciPushDef {
    dest: String,
    format: ImageFormat,
    insecure: bool,
    tool: Tool,
}

const OCI_PUSH_FORMAT_VERSION: u32 = 1;

impl Hash for OciPushDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        OCI_PUSH_FORMAT_VERSION.hash(state);
        self.dest.hash(state);
        self.format.transport().hash(state);
        self.insecure.hash(state);
        self.tool.label().hash(state);
    }
}

/// Push a docker-format archive with the docker CLI: load it into the daemon,
/// tag the loaded image as `dest`, push, then drop the tag again.
///
/// The load is an unavoidable side effect of the docker path (the CLI can only
/// push from the daemon's store), but the *tag* is not: leaving it behind would
/// silently do `oci_load`'s job on an `oci_push` target, so it is removed once
/// the push has succeeded.
async fn docker_push(
    docker_bin: &str,
    tar: &std::path::Path,
    dest: &str,
    cwd: &std::path::Path,
    io: &mut ToolIo<'_>,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<()> {
    let stdout = run_tool(
        vec![
            docker_bin.to_string(),
            "load".to_string(),
            "-i".to_string(),
            tar.to_string_lossy().into_owned(),
        ],
        cwd,
        "docker load (oci_push)",
        io,
        ctoken,
    )
    .await?;
    let loaded = parse_docker_load_ref(&stdout)?;
    run_tool(
        vec![
            docker_bin.to_string(),
            "tag".to_string(),
            loaded,
            dest.to_string(),
        ],
        cwd,
        "docker tag (oci_push)",
        io,
        ctoken,
    )
    .await?;
    let pushed = run_tool(
        vec![docker_bin.to_string(), "push".to_string(), dest.to_string()],
        cwd,
        "docker push (oci_push)",
        io,
        ctoken,
    )
    .await;
    // Remove the tag whether or not the push succeeded, so a failed push does
    // not leave the daemon holding an image the user never asked to load.
    let untag = run_tool(
        vec![
            docker_bin.to_string(),
            "rmi".to_string(),
            "--no-prune".to_string(),
            dest.to_string(),
        ],
        cwd,
        "docker rmi (oci_push)",
        io,
        ctoken,
    )
    .await;
    pushed?;
    if let Err(e) = untag {
        tracing::warn!(
            dest,
            error = %e,
            "oci_push: pushed, but could not drop the temporary local tag"
        );
    }
    Ok(())
}

/// Assemble the `skopeo copy` argv. Pure so it can be unit-tested without
/// skopeo. `argv[0]` is the skopeo binary.
fn push_argv(
    skopeo_bin: &str,
    format: ImageFormat,
    tar: &std::path::Path,
    dest: &str,
    insecure: bool,
) -> Vec<String> {
    let mut argv = vec![
        skopeo_bin.to_string(),
        "copy".to_string(),
        // Avoid needing a host /etc/containers/policy.json.
        "--insecure-policy".to_string(),
        // Copy every instance in the archive, not just the one matching the
        // host. skopeo's default (`system`) would silently push a
        // single-architecture image from a multi-platform archive on Linux, and
        // fail outright on macOS, where no instance matches `darwin`.
        "--multi-arch".to_string(),
        "all".to_string(),
    ];
    if insecure {
        argv.push("--dest-tls-verify=false".to_string());
    }
    argv.push(format!("{}:{}", format.transport(), tar.to_string_lossy()));
    argv.push(format!("docker://{dest}"));
    argv
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
        OciPushSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let addr = &req.target_spec.addr;
        let spec =
            OciPushSpec::from(req.target_spec.config.clone()).context("parse oci_push config")?;
        let format = ImageFormat::parse(spec.format.as_deref().unwrap_or("oci"))?;
        let tool = Tool::parse_opt(spec.tool.as_deref(), format)?;
        ensure_tool_supports_format(tool, format)?;

        // `insecure` is a skopeo flag. The docker CLI takes insecure registries
        // from the daemon config instead, so accepting the attribute here and
        // dropping it would leave the user believing TLS verification is off
        // when it is not.
        if spec.insecure && tool == Tool::Docker {
            anyhow::bail!(
                "`insecure = True` is not supported with tool = \"docker\": the daemon decides \
                 which registries are insecure. Add the registry to the daemon's \
                 `insecure-registries`, or use tool = \"skopeo\"."
            );
        }

        // Consume only the image archive (group ""), never the digest group.
        let mut image_ref = TargetAddr::parse(&spec.image, &addr.package)
            .with_context(|| format!("parse image ref {:?}", spec.image))?;
        super::pin_archive_group(&mut image_ref, &spec.image)?;

        let def = OciPushDef {
            dest: spec.dest,
            format,
            insecure: spec.insecure,
            tool,
        };
        let hash = {
            let mut h =
                DebugHasher::new(Xxh3Default::new(), || format!("oci_push_{}", addr.format()));
            def.hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs: vec![Input {
                    r#ref: image_ref,
                    mode: InputMode::Standard,
                    origin_id: IMAGE_ORIGIN.to_string(),
                    annotations: BTreeMap::new(),
                    hashed: true,
                    runtime: true,
                }],
                outputs: vec![],
                support_files: vec![],
                // An action with an external side effect: never cached, always runs.
                cache: CacheConfig::off(),
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
        let def = req.request.target.def_de::<OciPushDef>().clone();
        let tar = dep_single_file(&req, IMAGE_ORIGIN)?;
        let cwd = req.sandbox_ws_dir.clone();
        let addr = req.request.target.addr.format();
        let mut io = ToolIo::from_request(&mut req.request);
        match def.tool {
            Tool::Skopeo => {
                let argv = push_argv(
                    &self.tools.skopeo,
                    def.format,
                    &tar,
                    &def.dest,
                    def.insecure,
                );
                run_tool(argv, &cwd, "skopeo copy (oci_push)", &mut io, ctoken).await?;
            }
            Tool::Docker => {
                docker_push(&self.tools.docker, &tar, &def.dest, &cwd, &mut io, ctoken).await?;
            }
        }
        // A push is the one place the build graph meets the outside world, so
        // say what left the machine: without this line neither a human nor an
        // agent can learn what `heph run //app:push` actually shipped.
        tracing::info!(addr, r#ref = def.dest, "oci_push: pushed");
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
    fn push_argv_oci_uses_oci_archive_transport() {
        let argv = push_argv(
            "skopeo",
            ImageFormat::Oci,
            std::path::Path::new("/sbx/app/image.tar"),
            "reg.io/me/app:1.2",
            false,
        );
        assert_eq!(argv[0..3], ["skopeo", "copy", "--insecure-policy"]);
        let joined = argv.join(" ");
        assert!(
            joined.contains("oci-archive:/sbx/app/image.tar"),
            "{joined}"
        );
        assert!(joined.contains("docker://reg.io/me/app:1.2"), "{joined}");
        assert!(!joined.contains("--dest-tls-verify"), "{joined}");
    }

    #[test]
    fn push_argv_docker_format_and_insecure() {
        let argv = push_argv(
            "skopeo",
            ImageFormat::Docker,
            std::path::Path::new("/t/image.tar"),
            "localhost:5000/app",
            true,
        );
        let joined = argv.join(" ");
        assert!(joined.contains("docker-archive:/t/image.tar"), "{joined}");
        assert!(joined.contains("--dest-tls-verify=false"), "{joined}");
    }

    #[tokio::test]
    async fn parse_declares_tar_group_input_and_no_outputs() {
        let resp = parse(
            "//app:push",
            cfg(&[
                ("image", Value::String(":img".to_string())),
                ("ref", Value::String("reg.io/app:1".to_string())),
            ]),
        )
        .await;

        assert_eq!(resp.target_def.inputs.len(), 1);
        // Pinned to the archive group "", not all groups.
        assert_eq!(resp.target_def.inputs[0].r#ref.output.as_deref(), Some(""));
        assert_eq!(resp.target_def.inputs[0].r#ref.r#ref.format(), "//app:img");
        assert!(resp.target_def.outputs.is_empty());
        // An action: never cached.
        assert!(!resp.target_def.cache.enabled);
        assert!(!resp.target_def.cache.remote_enabled);
    }

    #[tokio::test]
    async fn parse_requires_image_and_ref() {
        let err = Driver::new()
            .parse(
                parse_req(
                    "//app:push",
                    cfg(&[("ref", Value::String("x".to_string()))]),
                ),
                &StdCancellationToken::new(),
            )
            .await
            .err()
            .expect("missing image must fail");
        assert!(format!("{err:#}").contains("image"), "got: {err:#}");
    }

    #[tokio::test]
    async fn parse_rejects_unknown_format() {
        let err = Driver::new()
            .parse(
                parse_req(
                    "//app:push",
                    cfg(&[
                        ("image", Value::String(":img".to_string())),
                        ("ref", Value::String("r".to_string())),
                        ("format", Value::String("zip".to_string())),
                    ]),
                ),
                &StdCancellationToken::new(),
            )
            .await
            .err()
            .expect("bad format must fail");
        assert!(format!("{err:#}").contains("oci"), "got: {err:#}");
    }

    #[test]
    fn parse_docker_load_ref_prefers_last_loaded_line() {
        assert_eq!(
            parse_docker_load_ref("Loaded image: alpine:latest\n").unwrap(),
            "alpine:latest"
        );
        assert_eq!(
            parse_docker_load_ref("Loaded image ID: sha256:abc123\n").unwrap(),
            "sha256:abc123"
        );
        // Last wins when several are loaded.
        assert_eq!(
            parse_docker_load_ref("Loaded image: a:1\nLoaded image: b:2\n").unwrap(),
            "b:2"
        );
        assert!(parse_docker_load_ref("nothing here").is_err());
    }

    /// A docker-format archive defaults to the `docker` tool — no skopeo needed.
    #[tokio::test]
    async fn parse_docker_format_defaults_to_docker_tool() {
        let resp = parse(
            "//app:push",
            cfg(&[
                ("image", Value::String(":img".to_string())),
                ("ref", Value::String("reg.io/app:1".to_string())),
                ("format", Value::String("docker".to_string())),
            ]),
        )
        .await;
        assert_eq!(resp.target_def.def::<OciPushDef>().tool, Tool::Docker);
    }

    /// An oci-format archive defaults to skopeo.
    #[tokio::test]
    async fn parse_oci_format_defaults_to_skopeo_tool() {
        let resp = parse(
            "//app:push",
            cfg(&[
                ("image", Value::String(":img".to_string())),
                ("ref", Value::String("reg.io/app:1".to_string())),
            ]),
        )
        .await;
        assert_eq!(resp.target_def.def::<OciPushDef>().tool, Tool::Skopeo);
    }

    /// docker cannot push an oci archive — rejected at parse.
    #[tokio::test]
    async fn parse_docker_tool_with_oci_format_fails() {
        let err = Driver::new()
            .parse(
                parse_req(
                    "//app:push",
                    cfg(&[
                        ("image", Value::String(":img".to_string())),
                        ("ref", Value::String("r".to_string())),
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

    /// The docker path is load → tag → push → rmi, in that order. The final
    /// `rmi` is what keeps `oci_push` from also doing `oci_load`'s job: without
    /// it the daemon keeps an image the user never asked to load.
    #[tokio::test]
    async fn run_docker_push_loads_tags_pushes_then_drops_the_tag() {
        let sbx = super::super::testfake::Sandbox::new("app");
        let tar = sbx.pkg.join("img.tar");
        std::fs::write(&tar, "tar").expect("tar");

        let resp = parse(
            "//app:push",
            cfg(&[
                ("image", Value::String(":img".to_string())),
                ("ref", Value::String("reg.io/app:1".to_string())),
                ("format", Value::String("docker".to_string())),
            ]),
        )
        .await;

        let docker = sbx.fake(
            "docker",
            "case \"$1\" in load) echo 'Loaded image: sha256:abc';; esac\nexit 0",
        );
        let rid = "req".to_string();
        let req = super::super::testfake::run_request(
            &rid,
            "hashin",
            &resp.target_def,
            &sbx,
            &[(IMAGE_ORIGIN, vec![tar])],
        );
        Driver::with_tools(super::super::Tools {
            docker,
            skopeo: "skopeo".to_string(),
        })
        .run(req, &StdCancellationToken::new())
        .await
        .expect("run");

        let verbs: Vec<String> = sbx
            .calls()
            .iter()
            .filter_map(|c| c.split_whitespace().nth(1).map(str::to_string))
            .collect();
        assert_eq!(
            verbs,
            ["load", "tag", "push", "rmi"],
            "calls: {:?}",
            sbx.calls()
        );
        let tag = sbx
            .calls()
            .into_iter()
            .find(|c| c.contains(" tag "))
            .expect("tag call");
        assert!(tag.contains("sha256:abc reg.io/app:1"), "{tag}");
    }

    /// skopeo copies every instance in the archive. The default (`system`) would
    /// push one architecture out of a multi-platform archive on Linux and fail
    /// outright on macOS, where nothing matches `darwin`.
    #[tokio::test]
    async fn run_skopeo_push_copies_all_architectures() {
        let sbx = super::super::testfake::Sandbox::new("app");
        let tar = sbx.pkg.join("img.tar");
        std::fs::write(&tar, "tar").expect("tar");

        let resp = parse(
            "//app:push",
            cfg(&[
                ("image", Value::String(":img".to_string())),
                ("ref", Value::String("reg.io/app:1".to_string())),
            ]),
        )
        .await;

        let skopeo = sbx.fake("skopeo", "exit 0");
        let rid = "req".to_string();
        let req = super::super::testfake::run_request(
            &rid,
            "hashin",
            &resp.target_def,
            &sbx,
            &[(IMAGE_ORIGIN, vec![tar])],
        );
        Driver::with_tools(super::super::Tools {
            docker: "docker".to_string(),
            skopeo,
        })
        .run(req, &StdCancellationToken::new())
        .await
        .expect("run");

        let call = sbx.calls().into_iter().next().expect("a skopeo call");
        assert!(call.contains("--multi-arch all"), "{call}");
        assert!(call.contains("docker://reg.io/app:1"), "{call}");
    }

    /// `insecure` is a skopeo flag; silently dropping it for the docker tool
    /// would leave the user believing TLS verification is off when it is not.
    #[tokio::test]
    async fn parse_insecure_with_docker_tool_fails() {
        let err = Driver::new()
            .parse(
                parse_req(
                    "//app:push",
                    cfg(&[
                        ("image", Value::String(":img".to_string())),
                        ("ref", Value::String("r".to_string())),
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

    /// Handing skopeo the `digest` group would give it a text file where it
    /// expects an archive, failing deep inside its layout parser.
    #[tokio::test]
    async fn parse_rejects_an_explicit_output_group() {
        let err = Driver::new()
            .parse(
                parse_req(
                    "//app:push",
                    cfg(&[
                        ("image", Value::String(":img|digest".to_string())),
                        ("ref", Value::String("r".to_string())),
                    ]),
                ),
                &StdCancellationToken::new(),
            )
            .await
            .err()
            .expect("an explicit group must fail");
        assert!(format!("{err:#}").contains("digest"), "got: {err:#}");
    }
}
