//! The `oci_push` driver: pushes an image archive (produced by an `oci_image`
//! target) to a registry.
//!
//! An *action*, not an artifact: it has an external side effect (the upload) and
//! is therefore **not cached** — it runs every time it is requested. The upload
//! is done with `skopeo copy`, which reads both OCI and docker archives
//! (`<transport>:<tar>` → `docker://<ref>`) daemonlessly and skips blobs the
//! registry already has, so a re-push of an unchanged image is cheap.

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
    /// `--dest-tls-verify=false` to skopeo.
    insecure: bool,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct OciPushDef {
    dest: String,
    format: ImageFormat,
    insecure: bool,
}

const OCI_PUSH_FORMAT_VERSION: u32 = 1;

impl Hash for OciPushDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        OCI_PUSH_FORMAT_VERSION.hash(state);
        self.dest.hash(state);
        self.format.transport().hash(state);
        self.insecure.hash(state);
    }
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
    ];
    if insecure {
        argv.push("--dest-tls-verify=false".to_string());
    }
    argv.push(format!("{}:{}", format.transport(), tar.to_string_lossy()));
    argv.push(format!("docker://{dest}"));
    argv
}

pub struct Driver {
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
            skopeo_bin: "skopeo".to_string(),
        }
    }

    #[cfg(test)]
    fn with_binary(bin: impl Into<String>) -> Self {
        Driver {
            skopeo_bin: bin.into(),
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

        // Consume only the image archive (group ""), never the digest group.
        let mut image_ref = TargetAddr::parse(&spec.image, &addr.package)
            .with_context(|| format!("parse image ref {:?}", spec.image))?;
        if image_ref.output.is_none() {
            image_ref.output = Some(String::new());
        }

        let def = OciPushDef {
            dest: spec.dest,
            format,
            insecure: spec.insecure,
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
        req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let def = req.request.target.def_de::<OciPushDef>();
        let tar = dep_single_file(&req, IMAGE_ORIGIN)?;
        let argv = push_argv(&self.skopeo_bin, def.format, &tar, &def.dest, def.insecure);
        run_cmd_cancellable(argv, ctoken, "skopeo copy (oci_push)")
            .await
            .with_context(|| format!("push image to {}", def.dest))?;
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
    fn with_binary_overrides_skopeo_bin() {
        assert_eq!(
            Driver::with_binary("/fake/skopeo").skopeo_bin,
            "/fake/skopeo"
        );
    }
}
