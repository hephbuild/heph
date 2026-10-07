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
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, Deferred,
    ParseRequest, ParseResponse, TargetAddr,
};
use hplugin::htspec::Spec;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

use super::auth::RegistryCredentials;
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
    ///
    /// Deferrable: `${read://pkg:name}` is the contents of that target's single
    /// output. `${image_hashout}` is the hashout of `image`'s archive, so
    /// `registry.io/me/app:${image_hashout}` gives every distinct image its own
    /// tag. That one costs the cache key nothing: `image` is already a hashed
    /// input, so the value cannot change without the key changing too.
    #[spec(required, rename = "ref")]
    dest: Deferred<String>,

    /// Push to an insecure (HTTP / self-signed) registry: plain HTTP, and
    /// certificate validation off.
    insecure: bool,
    /// Credentials to push with, as `credential` target addresses presented
    /// with `heph.auth.docker([...])`. When set, the registry is authenticated
    /// with these only — never with the host's own docker login — and a
    /// registry none of them covers is an error. When empty, the host's docker
    /// config is used, as the docker CLI would.
    ///
    /// Not an input: nothing about a credential reaches the cache key.
    credentials: Vec<String>,
    /// Push with the host's own docker login (`~/.docker/config.json`, its
    /// `credsStore`/`credHelpers`, podman's `auth.json`). Off by default: a
    /// target that sets neither this nor `credentials` pushes anonymously.
    /// Not combinable with `credentials`.
    ambient_credentials: bool,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct OciPushDef {
    /// Unresolved: `Deferred` hashes and serializes as the text the BUILD file
    /// wrote, so the def hash of a literal `ref` is what it was as a `String`.
    dest: Deferred<String>,
    insecure: bool,
    /// Who the push runs as, not what it pushes: deliberately not hashed.
    ambient_credentials: bool,
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

/// The variable `ref` may name besides a deferred reference: the hashout of the
/// image being pushed.
const IMAGE_HASHOUT: &str = "image_hashout";

/// Refuse, at parse, any `${…}` in `ref` that nothing would substitute.
///
/// A typo such as `${hashout}` would otherwise reach the registry as part of the
/// tag, or fail at run with a reference-parse error naming neither the field nor
/// the variables it accepts.
fn check_ref_template(raw: &str) -> anyhow::Result<()> {
    for piece in hcore::template::parse(raw).context("parse `ref`")? {
        let hcore::template::Piece::Ref(r) = piece else {
            continue;
        };
        let known = match r.kind {
            None => r.arg == IMAGE_HASHOUT,
            // Resolved by the host before the driver runs.
            kind => hcore::template::claims(kind, r.arg, hcore::template::DEFERRED_KINDS),
        };
        anyhow::ensure!(
            known,
            "`ref` holds `{}`, which is not something oci_push substitutes: use \
             `${{{IMAGE_HASHOUT}}}` for the image's hashout, or `${{read://pkg:name}}` for a \
             target's output",
            r.raw
        );
    }
    Ok(())
}

/// The hashout of the image archive input — the value `${image_hashout}` takes.
fn image_hashout(req: &ManagedRunRequest<'_, '_>) -> anyhow::Result<String> {
    req.inputs
        .iter()
        .find(|m| m.input.origin_id == IMAGE_ORIGIN)
        .with_context(|| format!("no dep input {IMAGE_ORIGIN:?} in sandbox"))?
        .input
        .artifact
        .content
        .hashout()
        .context("read the image's hashout")
}

/// `ref` with `${image_hashout}` replaced by `hashout`.
///
/// Runs over the value the host already resolved, so a `${read://…}` producer's
/// output is scanned too. Harmless: an image reference cannot contain `$`, so
/// such a value is broken either way, and this refuses it by name.
fn render_ref(resolved: &str, hashout: &str) -> anyhow::Result<String> {
    hcore::template::render(resolved, |r| {
        anyhow::ensure!(
            r.kind.is_none() && r.arg == IMAGE_HASHOUT,
            "`ref` resolved to {resolved:?}, and `{}` in it is not something oci_push \
             substitutes",
            r.raw
        );
        anyhow::ensure!(
            !hashout.is_empty(),
            "the image carries no hashout to substitute for `${{{IMAGE_HASHOUT}}}`"
        );
        Ok(hashout.to_string())
    })
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
        check_ref_template(spec.dest.raw())?;

        // Consume only the image archive (group ""), never the digest group.
        let mut image_ref = TargetAddr::parse(&spec.image, &addr.package)
            .with_context(|| format!("parse image ref {:?}", spec.image))?;
        super::pin_archive_group(&mut image_ref, &spec.image)?;
        super::auth::check_exclusive(&spec.credentials, spec.ambient_credentials)?;
        let credential_inputs =
            hdriver_support::credential::inputs(&spec.credentials, &addr.package)?;

        let def = OciPushDef {
            dest: spec.dest,
            insecure: spec.insecure,
            ambient_credentials: spec.ambient_credentials,
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
                inputs: std::iter::once(Input {
                    r#ref: image_ref,
                    mode: InputMode::Standard,
                    origin_id: IMAGE_ORIGIN.to_string(),
                    annotations: BTreeMap::new(),
                    hashed: true,
                    runtime: true,
                })
                .chain(credential_inputs)
                .collect(),
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
        let def = req.request.target.def_de::<OciPushDef>().clone();
        let path = dep_single_file(&req, IMAGE_ORIGIN)?;
        let dest = req.request.resolve(&def.dest)?;
        let dest = render_ref(dest, &image_hashout(&req)?)?;
        let layout =
            Layout::read(&path).with_context(|| format!("read the image to push from {path:?}"))?;

        let creds = RegistryCredentials::for_run(
            req.request.target,
            def.ambient_credentials,
            &req.request.credentials,
            &req.sandbox_dir,
            ctoken,
        )?;
        registry::push_layout(&layout, &dest, def.insecure, &creds)
            .await
            .with_context(|| format!("push {dest} ({})", creds.failure_hint()))?;

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

    /// A credential is an edge the cache key cannot see: annotated, neither
    /// hashed nor materialized, and the def hash does not move.
    #[tokio::test]
    async fn credentials_are_unhashed_edges() {
        let base = [
            ("image", Value::String(":img".to_string())),
            ("ref", Value::String("reg.io/app:1".to_string())),
        ];
        let without = parse("//app:push", cfg(&base)).await;
        let mut with_cfg = cfg(&base);
        with_cfg.insert(
            "credentials".to_string(),
            Value::List(vec![Value::String("//auth:reg".to_string())]),
        );
        let with = parse("//app:push", with_cfg).await;

        let cred = with
            .target_def
            .inputs
            .iter()
            .find(|i| hdriver_support::credential::is_credential(&i.annotations))
            .expect("a credential edge");
        assert_eq!(cred.r#ref.r#ref.format(), "//auth:reg");
        assert!(!cred.hashed && !cred.runtime);
        assert_eq!(with.target_def.hash, without.target_def.hash);
    }

    /// Opting in to the host's login is who pushes, not what: the key holds
    /// still. Combined with `credentials` it is refused at parse.
    #[tokio::test]
    async fn ambient_credentials_is_unhashed_and_exclusive() {
        let base = [
            ("image", Value::String(":img".to_string())),
            ("ref", Value::String("reg.io/app:1".to_string())),
        ];
        let without = parse("//app:push", cfg(&base)).await;
        let mut ambient = cfg(&base);
        ambient.insert("ambient_credentials".to_string(), Value::Bool(true));
        let with = parse("//app:push", ambient.clone()).await;
        assert_eq!(with.target_def.hash, without.target_def.hash);

        ambient.insert(
            "credentials".to_string(),
            Value::List(vec![Value::String("//auth:reg".to_string())]),
        );
        let err = Driver::new()
            .parse(
                parse_req("//app:push", ambient),
                &StdCancellationToken::new(),
            )
            .await
            .err()
            .expect("both must be refused");
        assert!(format!("{err:#}").contains("exclusive"), "{err:#}");
    }

    /// `ref` is a `Deferred`, which is what makes the host accept a
    /// `${read://…}` in it rather than refusing it for the whole driver.
    #[test]
    fn the_driver_accepts_deferred_values() {
        assert!(Driver::new().schema().accepts_deferred);
    }

    /// A literal `ref` hashes as the `String` it replaced, so retyping the field
    /// moves no def hash.
    #[tokio::test]
    async fn a_literal_ref_hashes_as_the_string_it_was() {
        let def = |dest: Deferred<String>| OciPushDef {
            dest,
            insecure: false,
            ambient_credentials: false,
        };
        let hash = |d: &OciPushDef| {
            let mut h = Xxh3Default::new();
            d.hash(&mut h);
            h.finish()
        };
        let mut h = Xxh3Default::new();
        OCI_PUSH_FORMAT_VERSION.hash(&mut h);
        "reg.io/app:1".to_string().hash(&mut h);
        false.hash(&mut h);
        assert_eq!(hash(&def(Deferred::new("reg.io/app:1"))), h.finish());
    }

    #[test]
    fn ref_template_accepts_what_is_substituted() {
        for ok in [
            "reg.io/app:1",
            "reg.io/app:${image_hashout}",
            "${read://infra:registry}/app:${image_hashout}",
            "${read://infra:ref}",
        ] {
            check_ref_template(ok).unwrap_or_else(|e| panic!("{ok:?}: {e:#}"));
        }
    }

    /// Each of these would otherwise reach the registry as text, or fail at run
    /// with an error that names neither the field nor what it accepts.
    #[test]
    fn ref_template_refuses_what_nothing_substitutes() {
        for bad in [
            "reg.io/app:${hashout}",
            "reg.io/app:${image_hashout2}",
            "reg.io/app:${IMAGE_HASHOUT}",
            "reg.io/app:${image_hashout }",
            "reg.io/app:${env:TAG}",
            // Not an absolute address, so the host leaves it alone.
            "reg.io/app:${read:tag}",
            "reg.io/app:${}",
        ] {
            let err = check_ref_template(bad).expect_err(bad);
            assert!(format!("{err:#}").contains("image_hashout"), "{bad:?}: {err:#}");
        }
    }

    #[test]
    fn render_ref_substitutes_every_image_hashout() {
        assert_eq!(
            render_ref("reg.io/app-${image_hashout}:${image_hashout}", "abc").expect("render"),
            "reg.io/app-abc:abc"
        );
        assert_eq!(
            render_ref("reg.io/app:1", "").expect("no variable, no hashout needed"),
            "reg.io/app:1"
        );
    }

    /// An empty hashout would turn `app:${image_hashout}` into `app:`, which
    /// parses as `app:latest` — a push to the wrong tag rather than an error.
    #[test]
    fn render_ref_refuses_an_empty_hashout() {
        let err = render_ref("reg.io/app:${image_hashout}", "").expect_err("empty");
        assert!(format!("{err:#}").contains("no hashout"), "{err:#}");
    }

    /// A `${read://…}` producer's output is scanned too; a variable in it that
    /// is not ours is refused rather than pushed as text.
    #[test]
    fn render_ref_refuses_an_unknown_variable_in_a_resolved_value() {
        let err = render_ref("reg.io/app:${tag}", "abc").expect_err("unknown");
        assert!(format!("{err:#}").contains("${tag}"), "{err:#}");
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
