//! The `oci_load` driver: loads an image archive (produced by an `oci_image`
//! target) into the local docker daemon.
//!
//! An *action*, not an artifact: it mutates the host daemon's image store and is
//! therefore **not cached** — it runs every time. The tool follows the archive
//! format: a `docker` archive is loaded with `docker load -i` (tags come from
//! the archive); an `oci` archive is loaded with `skopeo copy oci-archive:<tar>
//! docker-daemon:<tag>`, which needs an explicit `tag` to name the image in the
//! daemon.

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

use super::{ImageFormat, dep_single_file, run_cmd_cancellable};

pub const DRIVER_NAME: &str = "oci_load";

const IMAGE_ORIGIN: &str = "image";

/// Config for an `oci_load` target.
#[derive(Spec)]
struct OciLoadSpec {
    /// Target address of the image to load — an `oci_image` target. Only its
    /// archive output (group `""`) is consumed.
    #[spec(required)]
    image: String,
    /// Local tag to give the loaded image, e.g. `app:dev`. Required for the
    /// `oci` format (skopeo must name the daemon image); optional for the
    /// `docker` format (tags come from the archive).
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    tag: Option<String>,
    /// Source archive format: `oci` (default) or `docker`. Must match how the
    /// `oci_image` target was built.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    format: Option<String>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct OciLoadDef {
    format: ImageFormat,
    tag: Option<String>,
}

const OCI_LOAD_FORMAT_VERSION: u32 = 1;

impl Hash for OciLoadDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        OCI_LOAD_FORMAT_VERSION.hash(state);
        self.format.transport().hash(state);
        self.tag.hash(state);
    }
}

/// Assemble the load argv. Pure so it can be unit-tested without a daemon.
/// `argv[0]` is the binary (docker or skopeo, per format).
fn load_argv(
    docker_bin: &str,
    skopeo_bin: &str,
    def: &OciLoadDef,
    tar: &std::path::Path,
) -> anyhow::Result<Vec<String>> {
    match def.format {
        ImageFormat::Docker => Ok(vec![
            docker_bin.to_string(),
            "load".to_string(),
            "-i".to_string(),
            tar.to_string_lossy().into_owned(),
        ]),
        ImageFormat::Oci => {
            let tag = def.tag.as_deref().context(
                "oci_load of an `oci` archive requires `tag` (skopeo must name the daemon image)",
            )?;
            Ok(vec![
                skopeo_bin.to_string(),
                "copy".to_string(),
                "--insecure-policy".to_string(),
                format!("oci-archive:{}", tar.to_string_lossy()),
                format!("docker-daemon:{tag}"),
            ])
        }
    }
}

pub struct Driver {
    docker_bin: String,
    skopeo_bin: String,
}

impl Default for Driver {
    fn default() -> Self {
        Driver::new()
    }
}

impl Driver {
    pub fn new() -> Self {
        Driver {
            docker_bin: "docker".to_string(),
            skopeo_bin: "skopeo".to_string(),
        }
    }

    #[cfg(test)]
    fn with_binaries(docker: impl Into<String>, skopeo: impl Into<String>) -> Self {
        Driver {
            docker_bin: docker.into(),
            skopeo_bin: skopeo.into(),
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

        // Fail closed at parse time: an `oci` load with no tag can never run.
        if format == ImageFormat::Oci && spec.tag.is_none() {
            anyhow::bail!("oci_load of an `oci` archive requires `tag`");
        }

        let mut image_ref = TargetAddr::parse(&spec.image, &addr.package)
            .with_context(|| format!("parse image ref {:?}", spec.image))?;
        if image_ref.output.is_none() {
            image_ref.output = Some(String::new());
        }

        let def = OciLoadDef {
            format,
            tag: spec.tag,
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
        req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let def = req.request.target.def_de::<OciLoadDef>();
        let tar = dep_single_file(&req, IMAGE_ORIGIN)?;
        let argv = load_argv(&self.docker_bin, &self.skopeo_bin, def, &tar)?;
        run_cmd_cancellable(argv, ctoken, "oci_load")
            .await
            .context("load image into docker daemon")?;
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

    #[test]
    fn load_argv_docker_uses_docker_load() {
        let def = OciLoadDef {
            format: ImageFormat::Docker,
            tag: None,
        };
        let argv = load_argv("docker", "skopeo", &def, std::path::Path::new("/t/i.tar")).unwrap();
        assert_eq!(argv, ["docker", "load", "-i", "/t/i.tar"]);
    }

    #[test]
    fn load_argv_oci_uses_skopeo_to_daemon() {
        let def = OciLoadDef {
            format: ImageFormat::Oci,
            tag: Some("app:dev".to_string()),
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

    #[test]
    fn load_argv_oci_without_tag_errors() {
        let def = OciLoadDef {
            format: ImageFormat::Oci,
            tag: None,
        };
        let err = load_argv("docker", "skopeo", &def, std::path::Path::new("/t/i.tar"))
            .expect_err("oci without tag must fail");
        assert!(format!("{err:#}").contains("tag"), "got: {err:#}");
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

    #[test]
    fn with_binaries_overrides() {
        let d = Driver::with_binaries("/d", "/s");
        assert_eq!(d.docker_bin, "/d");
        assert_eq!(d.skopeo_bin, "/s");
    }
}
