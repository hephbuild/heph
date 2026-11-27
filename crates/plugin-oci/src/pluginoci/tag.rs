//! The `oci_tag` driver: points more tags at an image already in a registry.
//!
//! An *action* with an external side effect (moving tags), so it is **not
//! cached**: it runs every time it is requested.
//!
//! The image's manifest is fetched and pushed back, byte for byte, under each
//! new tag. Nothing is downloaded beyond the manifest and nothing is rebuilt,
//! and every tag resolves to the same digest as `ref` — including a
//! multi-platform list, which is retagged whole. A tag is added in the
//! repository `ref` names: the blobs are already there, and copying an image to
//! another repository is a push, not a tag.
//!
//! Its one output, `<name>.ref`, is the reference pinned to what the tags now
//! point at, `registry/repository@digest`.

use anyhow::Context as _;
use async_trait::async_trait;
use hcore::debug_hash::DebugHasher;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path as OutPath};
use hplugin::driver::targetdef::{Output, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, Deferred,
    ParseRequest, ParseResponse,
};
use hplugin::htspec::Spec;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

use super::auth::RegistryCredentials;
use super::{ref_out_name, registry};

/// Config for an `oci_tag` target.
#[derive(Spec)]
struct OciTagSpec {
    /// The image to tag, already in the registry, by tag or digest, e.g.
    /// `registry.io/me/app:sha-abc` or `registry.io/me/app@sha256:…`.
    ///
    /// Deferrable: `${read://pkg:name}` is the contents of that target's single
    /// output, so `ref = "${read://app:push}"` tags what an `oci.push` pushed.
    #[spec(required, rename = "ref")]
    reference: Deferred<String>,
    /// The tags to add, bare (`["1.2", "latest"]`), each in the repository
    /// `ref` names. A tag that already exists is moved to this image.
    #[spec(required)]
    tags: Vec<String>,
    /// Talk to an insecure (HTTP / self-signed) registry: plain HTTP, and
    /// certificate validation off.
    insecure: bool,
    /// Credentials to tag with, as `credential` target addresses presented
    /// with `heph.auth.docker([...])`. They need push access to the
    /// repository. When set, the registry is authenticated with these only —
    /// never with the host's own docker login — and a registry none of them
    /// covers is an error. When empty, the request is anonymous unless
    /// `ambient_credentials` is set.
    ///
    /// Not an input: nothing about a credential reaches the cache key.
    credentials: Vec<String>,
    /// Tag with the host's own docker login (`~/.docker/config.json`, its
    /// `credsStore`/`credHelpers`, podman's `auth.json`). Off by default: a
    /// target that sets neither this nor `credentials` is anonymous.
    /// Not combinable with `credentials`.
    ambient_credentials: bool,
    /// What the target reserves from the resource pools while it runs, e.g.
    /// `{"cpu": 2}`. IO-bound: an undeclared `cpu` costs 0.1. Never hashed.
    resources: hplugin::driver::Resources,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct OciTagDef {
    /// Unresolved: `Deferred` hashes and serializes as the text the BUILD file
    /// wrote.
    reference: Deferred<String>,
    /// Checked and deduplicated, in the order written.
    tags: Vec<String>,
    insecure: bool,
    /// Who tags, not what is tagged: deliberately not hashed.
    ambient_credentials: bool,
}

const OCI_TAG_FORMAT_VERSION: u32 = 1;

impl Hash for OciTagDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        OCI_TAG_FORMAT_VERSION.hash(state);
        self.reference.hash(state);
        self.tags.hash(state);
        self.insecure.hash(state);
    }
}

/// The longest tag the distribution spec allows.
const MAX_TAG_LEN: usize = 128;

/// Refuse anything the distribution spec does not accept as a tag,
/// `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`, at parse.
///
/// A whole reference is the likely mistake, and gets its own message: a tag
/// is added where the image already is, never in another repository.
fn check_tag(tag: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !tag.contains(['/', ':', '@']),
        "`tags` holds {tag:?}, which is a reference, not a tag: give the bare tag \
         (e.g. \"1.2\"). It is added in the repository `ref` names; copying an image to \
         another repository is an `oci.push`"
    );
    let valid = (1..=MAX_TAG_LEN).contains(&tag.len())
        && tag.bytes().enumerate().all(|(i, b)| {
            b.is_ascii_alphanumeric() || b == b'_' || (i > 0 && (b == b'.' || b == b'-'))
        });
    anyhow::ensure!(
        valid,
        "`tags` holds {tag:?}, which is not a valid tag: up to {MAX_TAG_LEN} of letters, \
         digits, `_`, `.` and `-`, not starting with `.` or `-`"
    );
    Ok(())
}

/// `tags` checked, with repeats dropped (first one wins).
fn checked_tags(tags: Vec<String>) -> anyhow::Result<Vec<String>> {
    anyhow::ensure!(
        !tags.is_empty(),
        "`tags` is empty: name at least one tag to add"
    );
    let mut seen = std::collections::HashSet::with_capacity(tags.len());
    let mut out = Vec::with_capacity(tags.len());
    for tag in tags {
        check_tag(&tag)?;
        if seen.insert(tag.clone()) {
            out.push(tag);
        }
    }
    Ok(out)
}

/// Stateless: the registry client is built per run from the target's config.
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
            name: "tag".to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        OciTagSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let addr = &req.target_spec.addr;
        let spec = OciTagSpec::from(&req.target_spec.config).context("parse oci_tag config")?;
        super::check_ref_reads("oci_tag", spec.reference.raw())?;
        let tags = checked_tags(spec.tags)?;
        super::auth::check_exclusive(&spec.credentials, spec.ambient_credentials)?;
        let inputs = hdriver_support::credential::inputs(&spec.credentials, &addr.package)?;

        let def = OciTagDef {
            reference: spec.reference,
            tags,
            insecure: spec.insecure,
            ambient_credentials: spec.ambient_credentials,
        };
        let hash = {
            let mut h =
                DebugHasher::new(Xxh3Default::new(), || format!("oci_tag_{}", addr.format()));
            def.hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs,
                outputs: vec![Output {
                    group: String::new(),
                    paths: vec![OutPath {
                        content: Content::FilePath(super::ws_path(
                            addr.package.as_str(),
                            &ref_out_name(&addr.name),
                        )),
                        codegen_tree: CodegenMode::None,
                        collect: true,
                    }],
                }],
                // An action with an external side effect: never cached (the
                // default `CacheConfig::off()`), always runs.
                hash,
                resources: spec
                    .resources
                    .advertise(
                        hplugin::provider::PoolName::cpu(),
                        hplugin::driver::Resources::IO_CPU,
                    )
                    .context("oci.tag resources")?,
                ..Default::default()
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
        let def = req.request.target.def_de::<OciTagDef>().clone();
        let reference = req
            .request
            .resolve(&def.reference)
            .context("resolve oci_tag `ref`")?;
        let creds = RegistryCredentials::for_run(
            req.request.target,
            def.ambient_credentials,
            &req.request.credentials,
            &req.sandbox_dir,
            ctoken,
        )?;
        let pinned = registry::tag(&reference, &def.tags, def.insecure, &creds)
            .await
            .with_context(|| {
                format!(
                    "tag {reference} as {} ({})",
                    def.tags.join(", "),
                    creds.failure_hint()
                )
            })?;
        tracing::debug!(target_addr = %req.request.target.addr.format(), %reference, tags = ?def.tags, %pinned, "tagged");

        let out = req
            .sandbox_pkg_dir
            .join(ref_out_name(&req.request.target.addr.name));
        tokio::fs::write(&out, &pinned)
            .await
            .with_context(|| format!("write the tagged reference to {out:?}"))?;
        Ok(ManagedRunResponse::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pluginoci::registry::testreg::{BOT_ANSWER, BOT_BASIC, MockRegistry, oci_manifest};
    use crate::pluginoci::testfake::{Sandbox, run_request};
    use hcore::hasync::StdCancellationToken;
    use hcore::htvalue::Value;
    use hmodel::htaddr::parse_addr;
    use hplugin::provider::TargetSpec;
    use std::collections::HashMap;

    fn s(v: &str) -> Value {
        Value::String(v.to_string())
    }

    fn list(v: &[&str]) -> Value {
        Value::List(v.iter().map(|x| s(x)).collect())
    }

    async fn parse(config: &[(&str, Value)]) -> anyhow::Result<ParseResponse> {
        Driver::new()
            .parse(
                ParseRequest {
                    request_id: "test".to_string(),
                    target_spec: Arc::new(TargetSpec {
                        addr: parse_addr("//app:tag").expect("addr"),
                        driver: "oci.tag".to_string(),
                        config: config
                            .iter()
                            .map(|(k, v)| ((*k).to_string(), v.clone()))
                            .collect::<HashMap<_, _>>(),
                        ..Default::default()
                    }),
                },
                &StdCancellationToken::new(),
            )
            .await
    }

    #[tokio::test]
    async fn parse_declares_only_the_ref_output_and_is_uncached() {
        let resp = parse(&[("ref", s("reg.io/app:1")), ("tags", list(&["latest"]))])
            .await
            .expect("parse");
        assert!(resp.target_def.inputs.is_empty());
        let [out] = resp.target_def.outputs.as_slice() else {
            panic!("one output, got {}", resp.target_def.outputs.len());
        };
        assert!(
            matches!(&out.paths[..], [p] if matches!(&p.content, Content::FilePath(f) if f == "app/tag.ref")),
            "{:?}",
            out.paths
        );
        assert!(!resp.target_def.cache.enabled);
        assert!(!resp.target_def.cache.remote_enabled);
        assert!(Driver::new().schema().accepts_deferred);
    }

    #[tokio::test]
    async fn credentials_are_unhashed_and_exclusive_with_ambient() {
        let base = [("ref", s("reg.io/app:1")), ("tags", list(&["latest"]))];
        let plain = parse(&base).await.expect("parse");
        let mut with = base.to_vec();
        with.push(("credentials", list(&["//auth:reg"])));
        let with = parse(&with).await.expect("parse");
        let [cred] = with.target_def.inputs.as_slice() else {
            panic!("one credential edge");
        };
        assert!(hdriver_support::credential::is_credential(
            &cred.annotations
        ));
        assert!(!cred.hashed && !cred.runtime);
        assert_eq!(with.target_def.hash, plain.target_def.hash);

        let mut both = base.to_vec();
        both.push(("credentials", list(&["//auth:reg"])));
        both.push(("ambient_credentials", Value::Bool(true)));
        let err = parse(&both).await.err().expect("both must be refused");
        assert!(format!("{err:#}").contains("exclusive"), "{err:#}");
    }

    #[test]
    fn tags_are_checked_against_the_spec_grammar() {
        let long = "a".repeat(MAX_TAG_LEN);
        for ok in ["latest", "1.2.3", "v1-rc_2", "_x", "A", long.as_str()] {
            check_tag(ok).unwrap_or_else(|e| panic!("{ok:?}: {e:#}"));
        }
        let too_long = "a".repeat(MAX_TAG_LEN + 1);
        for bad in [
            "",
            ".hidden",
            "-dash",
            "has space",
            "tab\t",
            "line\nbreak",
            "café",
            "a+b",
            too_long.as_str(),
        ] {
            let err = check_tag(bad).expect_err(bad);
            assert!(
                format!("{err:#}").contains("not a valid tag"),
                "{bad:?}: {err:#}"
            );
        }
        // A reference where a tag was wanted.
        for bad in ["reg.io/app:1", "app:1", "other/repo", "app@sha256:abc"] {
            let err = check_tag(bad).expect_err(bad);
            assert!(format!("{err:#}").contains("not a tag"), "{bad:?}: {err:#}");
        }
    }

    #[test]
    fn repeated_tags_are_dropped_and_none_is_refused() {
        assert_eq!(
            checked_tags(vec!["b".into(), "a".into(), "b".into()]).expect("tags"),
            ["b", "a"]
        );
        assert!(checked_tags(vec![]).is_err());
    }

    /// The order and set of tags are what the target does, so they move the
    /// def hash; a repeat does not.
    #[tokio::test]
    async fn the_tags_reach_the_def_hash() {
        let hash = |tags: &'static [&'static str]| async move {
            parse(&[("ref", s("reg.io/app:1")), ("tags", list(tags))])
                .await
                .expect("parse")
                .target_def
                .hash
        };
        assert_ne!(hash(&["a"]).await, hash(&["b"]).await);
        assert_eq!(hash(&["a", "a"]).await, hash(&["a"]).await);
    }

    #[tokio::test]
    async fn parse_takes_reads_and_refuses_anything_else() {
        parse(&[("ref", s("${read://app:push}")), ("tags", list(&["1"]))])
            .await
            .expect("a read is accepted");
        let err = parse(&[
            ("ref", s("reg.io/app:${image_hashout}")),
            ("tags", list(&["1"])),
        ])
        .await
        .err()
        .expect("refused");
        assert!(
            format!("{err:#}").contains("not something oci_tag substitutes"),
            "{err:#}"
        );
    }

    async fn run(
        reference: &str,
        tags: &[&str],
        credentials: Vec<hplugin::driver::CredentialMount>,
    ) -> (Sandbox, anyhow::Result<ManagedRunResponse>) {
        let creds: Vec<Value> = (0..credentials.len())
            .map(|i| s(&format!("//auth:c{i}")))
            .collect();
        let def = parse(&[
            ("ref", s(reference)),
            ("tags", list(tags)),
            ("insecure", Value::Bool(true)),
            ("credentials", Value::List(creds)),
        ])
        .await
        .expect("parse")
        .target_def;
        let sbx = Sandbox::new("app");
        let rid = "req".to_string();
        let mut req = run_request(&rid, "hashin", &def, &sbx, &[]);
        req.request.credentials = credentials;
        let res = Driver::new().run(req, &StdCancellationToken::new()).await;
        (sbx, res)
    }

    /// Each tag gets the manifest's own bytes, so it resolves to the same
    /// digest; the output pins it.
    #[tokio::test]
    async fn every_tag_points_at_the_same_digest() {
        let reg = MockRegistry::start(None);
        let digest = reg.put("acme/app", "build-1", &oci_manifest());
        let (sbx, res) = run(
            &format!("{}/acme/app:build-1", reg.host()),
            &["1.2", "latest"],
            vec![],
        )
        .await;
        res.expect("tag");
        for tag in ["1.2", "latest"] {
            let (_, bytes) = reg.get("acme/app", tag).expect(tag);
            assert_eq!(bytes, oci_manifest(), "{tag}");
        }
        let out = std::fs::read_to_string(sbx.pkg.join("tag.ref")).expect("output");
        assert_eq!(out, format!("{}/acme/app@{digest}", reg.host()));
    }

    /// The tag is checked before anything is written: a missing source pushes
    /// nothing.
    #[tokio::test]
    async fn a_missing_source_tags_nothing() {
        let reg = MockRegistry::start(None);
        reg.put("acme/app", "build-1", &oci_manifest());
        let (_sbx, res) = run(&format!("{}/acme/app:nope", reg.host()), &["1"], vec![]).await;
        let err = format!("{:#}", res.err().expect("missing"));
        assert!(err.contains("nothing to tag"), "{err}");
        assert!(
            !reg.requests().iter().any(|r| r.starts_with("PUT")),
            "{:?}",
            reg.requests()
        );
    }

    /// The declared credential reaches the registry on the push of the tag,
    /// and an anonymous attempt on the same registry writes nothing.
    #[tokio::test]
    async fn a_declared_credential_authenticates_the_tag() {
        let reg = MockRegistry::start(Some(BOT_BASIC));
        reg.put("acme/app", "build-1", &oci_manifest());
        let reference = format!("{}/acme/app:build-1", reg.host());

        let (_sbx, res) = run(&reference, &["1"], vec![]).await;
        let err = format!("{:#}", res.err().expect("anonymous is refused"));
        assert!(err.contains("anonymous"), "{err}");
        assert!(reg.get("acme/app", "1").is_none());

        let p =
            crate::pluginoci::auth::tests::Presented::new("//auth:c0", &[&reg.host()], BOT_ANSWER);
        let (_sbx, res) = run(&reference, &["1"], vec![p.mount.clone()]).await;
        res.expect("authenticated tag");
        assert!(reg.get("acme/app", "1").is_some());
        assert!(
            reg.authorizations().iter().all(|a| a == BOT_BASIC),
            "only the declared identity is sent: {:?}",
            reg.authorizations()
        );
    }
}
