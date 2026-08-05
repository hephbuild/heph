//! The `oci_push` driver: pushes an image archive (produced by a `docker_build`
//! target) to a registry.
//!
//! An *action*, not an artifact: it has an external side effect (the upload) and
//! is therefore **not cached** — it runs every time it is requested.
//!
//! Speaks the OCI distribution protocol in-process (see [`super::registry`]):
//! no daemon, no skopeo, and blobs the registry already has are skipped. A
//! multi-platform archive pushes every instance plus the manifest list that ties
//! them together.

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

use super::{archive::Layout, dep_single_file, registry};

pub const DRIVER_NAME: &str = "oci_push";

/// The `origin_id` of the single image-archive dep input.
const IMAGE_ORIGIN: &str = "image";

/// Config for an `oci_push` target.
#[derive(Spec)]
struct OciPushSpec {
    /// Target address of the image to push — a `docker_build` target. Only its
    /// archive output (group `""`) is consumed.
    #[spec(required)]
    image: String,
    /// Destination registry reference, e.g. `registry.io/me/app:1.2`.
    #[spec(required, rename = "ref")]
    dest: String,

    /// Push to an insecure (HTTP / self-signed) registry: plain HTTP, and
    /// certificate validation off.
    insecure: bool,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct OciPushDef {
    dest: String,
    insecure: bool,
}

/// v2: pushed in-process over the distribution protocol; `tool` and `format` are
/// gone, so neither is in the key any more.
const OCI_PUSH_FORMAT_VERSION: u32 = 2;

impl Hash for OciPushDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        OCI_PUSH_FORMAT_VERSION.hash(state);
        self.dest.hash(state);
        self.insecure.hash(state);
    }
}

/// Push a docker-format archive with the docker CLI: load it into the daemon,
/// tag the loaded image as `dest`, push, then drop the tag again.
///
/// The load is an unavoidable side effect of the docker path (the CLI can only
/// push from the daemon's store), but the *tag* is not: leaving it behind would
/// silently do `oci_load`'s job on an `oci_push` target, so it is removed once
/// the push has succeeded.
/// Stateless: the registry client is built per push from the target's own
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
        OciPushSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let addr = &req.target_spec.addr;
        let spec = OciPushSpec::from(&req.target_spec.config).context("parse oci_push config")?;

        // Consume only the image archive (group ""), never the digest group.
        let mut image_ref = TargetAddr::parse(&spec.image, &addr.package)
            .with_context(|| format!("parse image ref {:?}", spec.image))?;
        super::pin_archive_group(&mut image_ref, &spec.image)?;

        let def = OciPushDef {
            dest: spec.dest,
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
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let def = req.request.target.def_de::<OciPushDef>().clone();
        let path = dep_single_file(&req, IMAGE_ORIGIN)?;
        let layout =
            Layout::read(&path).with_context(|| format!("read the image to push from {path:?}"))?;

        let digest = registry::push_layout(&layout, &def.dest, def.insecure)
            .await
            .with_context(|| format!("push {}", def.dest))?;

        // A push is the one place the build graph meets the outside world, so
        // say what left the machine: without it neither a human nor an agent can
        // learn what `heph run //app:push` actually shipped.
        tracing::info!(r#ref = def.dest, digest, "oci_push: pushed");
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
