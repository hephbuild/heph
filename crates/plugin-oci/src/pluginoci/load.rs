//! The `oci_load` driver: loads an image archive (produced by an `oci_image`
//! target) into the local docker daemon.
//!
//! An *action*, not an artifact: it mutates the host daemon's image store and is
//! therefore **not cached** — it runs every time. The tool follows the archive
//! format: a `docker` archive is loaded with `docker load -i` (tags come from
//! the archive); an `oci` archive is loaded with `skopeo copy oci-archive:<tar>
//! docker-daemon:<tag>`, which needs an explicit `tag` to name the image in the
//! daemon.
//!
//! A daemon tag holds one image, so loading a **multi-platform** archive means
//! choosing an instance: `platform` (default Linux on the host's architecture)
//! is pinned onto the skopeo copy rather than left to skopeo's host-derived
//! default, which on macOS is a `darwin` no Linux manifest list matches.

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

pub const DRIVER_NAME: &str = "oci_load";

const IMAGE_ORIGIN: &str = "image";

/// Config for an `oci_load` target.
#[derive(Spec)]
struct OciLoadSpec {
    /// Target address of the image to load — an `oci_image` target. Only its
    /// archive output (group `""`) is consumed.
    #[spec(required)]
    image: String,
    /// Local tag to give the loaded image, e.g. `app:dev`. Required when loading
    /// with `skopeo` (it must name the daemon image) — i.e. for the `oci` format
    /// or an explicit `tool = "skopeo"`. Optional for `docker load` (tags come
    /// from the archive).
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    tag: Option<String>,
    /// Source archive format: `oci` (default) or `docker`. Must match how the
    /// `oci_image` target was built.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    format: Option<String>,
    /// Tool to load with: `skopeo` (default for an `oci` archive) or `docker`
    /// (default for a `docker` archive — `docker load`, no skopeo needed).
    /// `docker` cannot load an `oci` archive.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    tool: Option<String>,
    /// Which instance to load out of a **multi-platform** archive, as `os/arch`
    /// (e.g. `linux/amd64`). Defaults to Linux on the host's architecture.
    ///
    /// A daemon holds one image per tag, so a multi-arch archive built by
    /// `oci_image(platforms = [...])` must be narrowed to one instance on the way
    /// in. Left to skopeo's own default the choice would follow the host's
    /// GOOS/GOARCH — `darwin` on macOS, which no Linux manifest list matches, so
    /// the load would fail outright there.
    ///
    /// `tool = "docker"` rejects it: `docker load` has no instance selection.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    platform: Option<String>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct OciLoadDef {
    format: ImageFormat,
    tag: Option<String>,
    tool: Tool,
    /// The `os/arch` instance taken out of the archive. `None` for `docker
    /// load`, which has no instance selection.
    platform: Option<String>,
}

/// v2: the skopeo path pins the loaded instance instead of following the host's
/// GOOS/GOARCH.
const OCI_LOAD_FORMAT_VERSION: u32 = 2;

impl Hash for OciLoadDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        OCI_LOAD_FORMAT_VERSION.hash(state);
        self.format.transport().hash(state);
        self.tag.hash(state);
        self.tool.label().hash(state);
        self.platform.hash(state);
    }
}

/// Assemble the load argv. Pure so it can be unit-tested without a daemon.
/// `argv[0]` is the binary (docker or skopeo, per `tool`).
fn load_argv(
    docker_bin: &str,
    skopeo_bin: &str,
    def: &OciLoadDef,
    tar: &std::path::Path,
) -> anyhow::Result<Vec<String>> {
    match def.tool {
        Tool::Docker => Ok(vec![
            docker_bin.to_string(),
            "load".to_string(),
            "-i".to_string(),
            tar.to_string_lossy().into_owned(),
        ]),
        Tool::Skopeo => {
            let tag = def.tag.as_deref().context(
                "oci_load with skopeo requires `tag` (skopeo must name the daemon image)",
            )?;
            let mut argv = vec![
                skopeo_bin.to_string(),
                "copy".to_string(),
                "--insecure-policy".to_string(),
            ];
            if let Some(platform) = &def.platform {
                argv.extend(super::platform_override_args(platform)?);
            }
            argv.push(format!(
                "{}:{}",
                def.format.transport(),
                tar.to_string_lossy()
            ));
            argv.push(format!("docker-daemon:{tag}"));
            Ok(argv)
        }
    }
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
        OciLoadSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let addr = &req.target_spec.addr;
        let spec =
            OciLoadSpec::from(req.target_spec.config.clone()).context("parse oci_load config")?;
        let format = ImageFormat::parse(spec.format.as_deref().unwrap_or("oci"))?;
        let tool = Tool::parse_opt(spec.tool.as_deref(), format)?;
        ensure_tool_supports_format(tool, format)?;

        // Fail closed at parse time: a skopeo load with no tag can never run.
        if tool == Tool::Skopeo && spec.tag.is_none() {
            anyhow::bail!("oci_load with skopeo requires `tag`");
        }

        let platform = match tool {
            Tool::Docker => {
                if let Some(p) = &spec.platform {
                    anyhow::bail!(
                        "`platform` ({p:?}) is not supported with tool = \"docker\": `docker load` \
                         loads what the archive holds and cannot select an instance. Use \
                         tool = \"skopeo\"."
                    );
                }
                None
            }
            // Always concrete, never the host's GOOS/GOARCH: a multi-arch
            // archive has no `darwin` instance to match on macOS.
            Tool::Skopeo => {
                let p = spec.platform.unwrap_or_else(super::default_platform);
                super::split_platform(&p)?;
                Some(p)
            }
        };

        let mut image_ref = TargetAddr::parse(&spec.image, &addr.package)
            .with_context(|| format!("parse image ref {:?}", spec.image))?;
        super::pin_archive_group(&mut image_ref, &spec.image)?;

        let def = OciLoadDef {
            format,
            tag: spec.tag,
            tool,
            platform,
        };
        let hash = {
            let mut h =
                DebugHasher::new(Xxh3Default::new(), || format!("oci_load_{}", addr.format()));
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
        let def = req.request.target.def_de::<OciLoadDef>().clone();
        let tar = dep_single_file(&req, IMAGE_ORIGIN)?;
        let cwd = req.sandbox_ws_dir.clone();
        let argv = load_argv(&self.tools.docker, &self.tools.skopeo, &def, &tar)?;
        let mut io = ToolIo::from_request(&mut req.request);
        let stdout = run_tool(argv, &cwd, "oci_load", &mut io, ctoken)
            .await
            .context("load image into docker daemon")?;

        // `docker load` takes tags from the archive and has no `--tag`, so an
        // explicit `tag` has to be applied afterwards. Doing nothing with it
        // would leave the user with a dangling `<none>:<none>` image and no way
        // to run what they just asked to load.
        match (def.tool, def.tag.as_deref()) {
            (Tool::Docker, Some(tag)) => {
                let loaded = parse_docker_load_ref(&stdout)?;
                run_tool(
                    vec![
                        self.tools.docker.clone(),
                        "tag".to_string(),
                        loaded,
                        tag.to_string(),
                    ],
                    &cwd,
                    "docker tag (oci_load)",
                    &mut io,
                    ctoken,
                )
                .await?;
            }
            // skopeo named the daemon image via `docker-daemon:<tag>` already,
            // and `docker load` with no `tag` keeps whatever the archive named.
            (Tool::Skopeo, _) | (Tool::Docker, None) => {}
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
    fn load_argv_docker_uses_docker_load() {
        let def = OciLoadDef {
            format: ImageFormat::Docker,
            tag: None,
            tool: Tool::Docker,
            platform: None,
        };
        let argv = load_argv("docker", "skopeo", &def, std::path::Path::new("/t/i.tar")).unwrap();
        assert_eq!(argv, ["docker", "load", "-i", "/t/i.tar"]);
    }

    #[test]
    fn load_argv_oci_uses_skopeo_to_daemon() {
        let def = OciLoadDef {
            format: ImageFormat::Oci,
            tag: Some("app:dev".to_string()),
            tool: Tool::Skopeo,
            platform: Some("linux/amd64".to_string()),
        };
        let argv = load_argv("docker", "skopeo", &def, std::path::Path::new("/t/i.tar")).unwrap();
        let joined = argv.join(" ");
        assert!(
            joined.starts_with("skopeo copy --insecure-policy"),
            "{joined}"
        );
        assert!(joined.contains("oci-archive:/t/i.tar"), "{joined}");
        assert!(joined.contains("docker-daemon:app:dev"), "{joined}");
    }

    /// Explicit skopeo on a docker-format archive loads via docker-archive
    /// transport — still no docker CLI.
    #[test]
    fn load_argv_skopeo_docker_format_uses_docker_archive() {
        let def = OciLoadDef {
            format: ImageFormat::Docker,
            tag: Some("app:dev".to_string()),
            tool: Tool::Skopeo,
            platform: Some("linux/amd64".to_string()),
        };
        let argv = load_argv("docker", "skopeo", &def, std::path::Path::new("/t/i.tar")).unwrap();
        let joined = argv.join(" ");
        assert!(joined.contains("docker-archive:/t/i.tar"), "{joined}");
        assert!(joined.contains("docker-daemon:app:dev"), "{joined}");
    }

    /// A daemon tag holds one image, so the instance taken out of a multi-arch
    /// archive is pinned rather than matched against the host — which on macOS is
    /// a `darwin` no Linux manifest list contains.
    #[test]
    fn load_argv_pins_the_instance_out_of_a_multi_arch_archive() {
        let def = OciLoadDef {
            format: ImageFormat::Oci,
            tag: Some("app:dev".to_string()),
            tool: Tool::Skopeo,
            platform: Some("linux/arm64".to_string()),
        };
        let joined = load_argv("docker", "skopeo", &def, std::path::Path::new("/t/i.tar"))
            .expect("argv")
            .join(" ");
        assert!(joined.contains("--override-os linux"), "{joined}");
        assert!(joined.contains("--override-arch arm64"), "{joined}");
    }

    #[test]
    fn load_argv_skopeo_without_tag_errors() {
        let def = OciLoadDef {
            format: ImageFormat::Oci,
            tag: None,
            tool: Tool::Skopeo,
            platform: Some("linux/amd64".to_string()),
        };
        let err = load_argv("docker", "skopeo", &def, std::path::Path::new("/t/i.tar"))
            .expect_err("skopeo without tag must fail");
        assert!(format!("{err:#}").contains("tag"), "got: {err:#}");
    }

    /// docker cannot load an oci archive — rejected at parse.
    #[tokio::test]
    async fn parse_docker_tool_with_oci_format_fails() {
        let err = Driver::new()
            .parse(
                parse_req(
                    "//app:load",
                    cfg(&[
                        ("image", Value::String(":img".to_string())),
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

    #[tokio::test]
    async fn parse_oci_without_tag_fails() {
        let err = Driver::new()
            .parse(
                parse_req(
                    "//app:load",
                    cfg(&[("image", Value::String(":img".to_string()))]),
                ),
                &StdCancellationToken::new(),
            )
            .await
            .err()
            .expect("oci load without tag must fail parse");
        assert!(format!("{err:#}").contains("tag"), "got: {err:#}");
    }

    /// An implicit load records a concrete instance, so a multi-arch archive
    /// resolves the same way on every host — and at all on macOS.
    #[tokio::test]
    async fn parse_defaults_platform_to_linux_host_arch() {
        let resp = parse(
            "//app:load",
            cfg(&[
                ("image", Value::String(":img".to_string())),
                ("tag", Value::String("app:dev".to_string())),
            ]),
        )
        .await;
        let platform = resp
            .target_def
            .def::<OciLoadDef>()
            .platform
            .clone()
            .expect("skopeo load pins a platform");
        assert!(platform.starts_with("linux/"), "got: {platform}");
    }

    /// Loading the amd64 instance and loading the arm64 one are different
    /// actions; they must not collapse to one def.
    #[tokio::test]
    async fn parse_hash_differs_per_platform() {
        let base = |p: &str| {
            cfg(&[
                ("image", Value::String(":img".to_string())),
                ("tag", Value::String("app:dev".to_string())),
                ("platform", Value::String(p.to_string())),
            ])
        };
        let a = parse("//app:load", base("linux/amd64")).await;
        let b = parse("//app:load", base("linux/arm64")).await;
        assert_ne!(a.target_def.hash, b.target_def.hash);
    }

    /// `docker load` has no instance selection, so honouring `platform` there is
    /// impossible — say so instead of loading whatever the archive holds.
    #[tokio::test]
    async fn parse_platform_with_docker_tool_fails() {
        let err = Driver::new()
            .parse(
                parse_req(
                    "//app:load",
                    cfg(&[
                        ("image", Value::String(":img".to_string())),
                        ("format", Value::String("docker".to_string())),
                        ("platform", Value::String("linux/amd64".to_string())),
                    ]),
                ),
                &StdCancellationToken::new(),
            )
            .await
            .err()
            .expect("platform + docker must fail");
        assert!(format!("{err:#}").contains("platform"), "got: {err:#}");
    }

    #[tokio::test]
    async fn parse_docker_format_without_tag_ok_and_not_cached() {
        let resp = Driver::new()
            .parse(
                parse_req(
                    "//app:load",
                    cfg(&[
                        ("image", Value::String(":img".to_string())),
                        ("format", Value::String("docker".to_string())),
                    ]),
                ),
                &StdCancellationToken::new(),
            )
            .await
            .expect("parse");
        assert_eq!(resp.target_def.inputs.len(), 1);
        assert_eq!(resp.target_def.inputs[0].r#ref.output.as_deref(), Some(""));
        assert!(resp.target_def.outputs.is_empty());
        assert!(!resp.target_def.cache.enabled);
    }

    /// `docker load` has no `--tag` and takes tags from the archive, so an
    /// explicit `tag` has to be applied afterwards. Accepting it and dropping it
    /// would leave a dangling `<none>:<none>` image the user cannot run.
    #[tokio::test]
    async fn run_docker_applies_an_explicit_tag() {
        let sbx = super::super::testfake::Sandbox::new("app");
        let tar = sbx.pkg.join("img.tar");
        std::fs::write(&tar, "tar").expect("tar");

        let resp = parse(
            "//app:load",
            cfg(&[
                ("image", Value::String(":img".to_string())),
                ("format", Value::String("docker".to_string())),
                ("tag", Value::String("app:dev".to_string())),
            ]),
        )
        .await;

        let docker = sbx.fake(
            "docker",
            "case \"$1\" in load) echo 'Loaded image ID: sha256:abc';; esac\nexit 0",
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

        let calls = sbx.calls();
        assert!(
            calls.iter().any(|c| c.contains("tag sha256:abc app:dev")),
            "the explicit tag must be applied: {calls:?}"
        );
    }

    /// Handing skopeo the `digest` group would give it a text file where it
    /// expects an archive.
    #[tokio::test]
    async fn parse_rejects_an_explicit_output_group() {
        let err = Driver::new()
            .parse(
                parse_req(
                    "//app:load",
                    cfg(&[
                        ("image", Value::String(":img|digest".to_string())),
                        ("tag", Value::String("t".to_string())),
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
