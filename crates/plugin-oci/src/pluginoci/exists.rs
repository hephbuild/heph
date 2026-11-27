//! The `oci_exists` driver: checks that an image reference resolves in a
//! registry, and fails when it does not.
//!
//! A *check* against outside state, so it is **not cached**: the answer is the
//! registry's at the moment the target runs, and a cached "yes" would outlive a
//! deleted tag.
//!
//! Its one output, `<name>.ref`, is the reference pinned to what it resolved
//! to, `registry/repository@digest` — the form a deploy wants, and what
//! `${read://pkg:name}` hands a later target.
//!
//! "Missing" is only ever the registry saying so (a 404, or `MANIFEST_UNKNOWN` /
//! `NAME_UNKNOWN`): a refused credential, a 401 or a network failure is an
//! error of its own, so an expired token never reads as an unpublished image.

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

/// Config for an `oci_exists` target.
#[derive(Spec)]
struct OciExistsSpec {
    /// The image reference to look up, by tag or digest, e.g.
    /// `registry.io/me/app:1.2` or `registry.io/me/app@sha256:…`.
    ///
    /// Deferrable: `${read://pkg:name}` is the contents of that target's single
    /// output, so `ref = "${read://app:push}"` checks what an `oci.push` wrote.
    #[spec(required, rename = "ref")]
    reference: Deferred<String>,
    /// Look up in an insecure (HTTP / self-signed) registry: plain HTTP, and
    /// certificate validation off.
    insecure: bool,
    /// Credentials to look up with, as `credential` target addresses presented
    /// with `heph.auth.docker([...])`. When set, the registry is authenticated
    /// with these only — never with the host's own docker login — and a
    /// registry none of them covers is an error. When empty, the lookup is
    /// anonymous unless `ambient_credentials` is set.
    ///
    /// Not an input: nothing about a credential reaches the cache key.
    credentials: Vec<String>,
    /// Look up with the host's own docker login (`~/.docker/config.json`, its
    /// `credsStore`/`credHelpers`, podman's `auth.json`). Off by default: a
    /// target that sets neither this nor `credentials` looks up anonymously.
    /// Not combinable with `credentials`.
    ambient_credentials: bool,
    /// What the target reserves from the resource pools while it runs, e.g.
    /// `{"cpu": 2}`. IO-bound: an undeclared `cpu` costs 0.1. Never hashed.
    resources: hplugin::driver::Resources,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct OciExistsDef {
    /// Unresolved: `Deferred` hashes and serializes as the text the BUILD file
    /// wrote.
    reference: Deferred<String>,
    insecure: bool,
    /// Who looks, not what is looked up: deliberately not hashed.
    ambient_credentials: bool,
}

const OCI_EXISTS_FORMAT_VERSION: u32 = 1;

impl Hash for OciExistsDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        OCI_EXISTS_FORMAT_VERSION.hash(state);
        self.reference.hash(state);
        self.insecure.hash(state);
    }
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
            name: "exists".to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        OciExistsSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let addr = &req.target_spec.addr;
        let spec =
            OciExistsSpec::from(&req.target_spec.config).context("parse oci_exists config")?;
        super::check_ref_reads("oci_exists", spec.reference.raw())?;
        super::auth::check_exclusive(&spec.credentials, spec.ambient_credentials)?;
        let inputs = hdriver_support::credential::inputs(&spec.credentials, &addr.package)?;

        let def = OciExistsDef {
            reference: spec.reference,
            insecure: spec.insecure,
            ambient_credentials: spec.ambient_credentials,
        };
        let hash = {
            let mut h = DebugHasher::new(Xxh3Default::new(), || {
                format!("oci_exists_{}", addr.format())
            });
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
                // The registry's answer now, not a function of any input:
                // never cached (the default `CacheConfig::off()`).
                hash,
                resources: spec
                    .resources
                    .advertise(
                        hplugin::provider::PoolName::cpu(),
                        hplugin::driver::Resources::IO_CPU,
                    )
                    .context("oci.exists resources")?,
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
        let def = req.request.target.def_de::<OciExistsDef>().clone();
        let reference = req
            .request
            .resolve(&def.reference)
            .context("resolve oci_exists `ref`")?;
        let creds = RegistryCredentials::for_run(
            req.request.target,
            def.ambient_credentials,
            &req.request.credentials,
            &req.sandbox_dir,
            ctoken,
        )?;
        let pinned = registry::manifest_ref(&reference, def.insecure, &creds)
            .await
            .with_context(|| format!("look up {reference} ({})", creds.failure_hint()))?;
        let Some(pinned) = pinned else {
            anyhow::bail!(
                "{reference} does not exist: the registry has no such manifest or repository ({})",
                creds.failure_hint()
            );
        };
        tracing::debug!(target_addr = %req.request.target.addr.format(), %reference, %pinned, "exists");

        let out = req
            .sandbox_pkg_dir
            .join(ref_out_name(&req.request.target.addr.name));
        tokio::fs::write(&out, &pinned)
            .await
            .with_context(|| format!("write the resolved reference to {out:?}"))?;
        Ok(ManagedRunResponse::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pluginoci::registry::testreg::{MockRegistry, oci_manifest};
    use crate::pluginoci::testfake::{Sandbox, run_request};
    use hcore::hasync::StdCancellationToken;
    use hcore::htvalue::Value;
    use hmodel::htaddr::parse_addr;
    use hplugin::provider::TargetSpec;
    use std::collections::HashMap;

    fn parse_req(config: &[(&str, Value)]) -> ParseRequest {
        ParseRequest {
            request_id: "test".to_string(),
            target_spec: Arc::new(TargetSpec {
                addr: parse_addr("//app:exists").expect("addr"),
                driver: "oci.exists".to_string(),
                config: config
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), v.clone()))
                    .collect::<HashMap<_, _>>(),
                ..Default::default()
            }),
        }
    }

    async fn parse(config: &[(&str, Value)]) -> anyhow::Result<ParseResponse> {
        Driver::new()
            .parse(parse_req(config), &StdCancellationToken::new())
            .await
    }

    fn s(v: &str) -> Value {
        Value::String(v.to_string())
    }

    #[tokio::test]
    async fn parse_declares_only_the_ref_output_and_is_uncached() {
        let resp = parse(&[("ref", s("reg.io/app:1"))]).await.expect("parse");
        assert!(resp.target_def.inputs.is_empty());
        let [out] = resp.target_def.outputs.as_slice() else {
            panic!("one output, got {}", resp.target_def.outputs.len());
        };
        assert!(
            matches!(&out.paths[..], [p] if matches!(&p.content, Content::FilePath(f) if f == "app/exists.ref")),
            "{:?}",
            out.paths
        );
        assert!(!resp.target_def.cache.enabled);
        assert!(!resp.target_def.cache.remote_enabled);
        assert!(Driver::new().schema().accepts_deferred);
    }

    /// Credentials are unhashed edges; `ambient_credentials` moves no hash and
    /// is exclusive with them.
    #[tokio::test]
    async fn credentials_are_unhashed_and_exclusive_with_ambient() {
        let base = parse(&[("ref", s("reg.io/app:1"))]).await.expect("parse");
        let creds = Value::List(vec![s("//auth:reg")]);
        let with = parse(&[("ref", s("reg.io/app:1")), ("credentials", creds.clone())])
            .await
            .expect("parse");
        let [cred] = with.target_def.inputs.as_slice() else {
            panic!("one credential edge");
        };
        assert!(hdriver_support::credential::is_credential(
            &cred.annotations
        ));
        assert!(!cred.hashed && !cred.runtime);
        assert_eq!(with.target_def.hash, base.target_def.hash);

        let ambient = parse(&[
            ("ref", s("reg.io/app:1")),
            ("ambient_credentials", Value::Bool(true)),
        ])
        .await
        .expect("parse");
        assert_eq!(ambient.target_def.hash, base.target_def.hash);

        let err = parse(&[
            ("ref", s("reg.io/app:1")),
            ("credentials", creds),
            ("ambient_credentials", Value::Bool(true)),
        ])
        .await
        .err()
        .expect("both must be refused");
        assert!(format!("{err:#}").contains("exclusive"), "{err:#}");
    }

    #[tokio::test]
    async fn parse_takes_reads_and_refuses_anything_else() {
        for ok in [
            "reg.io/app:1",
            "${read://app:push}",
            "${read://infra:repo}:1",
        ] {
            parse(&[("ref", s(ok))])
                .await
                .unwrap_or_else(|e| panic!("{ok:?}: {e:#}"));
        }
        for bad in [
            "reg.io/app:${image_hashout}",
            "reg.io/app:${env:TAG}",
            "${src://infra:x}/app:1",
            "reg.io/a$$b:1",
        ] {
            let err = parse(&[("ref", s(bad))]).await.err().expect(bad);
            assert!(
                format!("{err:#}").contains("not something oci_exists substitutes"),
                "{bad:?}: {err:#}"
            );
        }
    }

    async fn run(
        reference: &str,
        credentials: Vec<hplugin::driver::CredentialMount>,
        n_credentials: usize,
    ) -> (Sandbox, anyhow::Result<ManagedRunResponse>) {
        let creds: Vec<Value> = (0..n_credentials)
            .map(|i| s(&format!("//auth:c{i}")))
            .collect();
        let def = parse(&[
            ("ref", s(reference)),
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

    /// Found: the output is the reference pinned to the digest the registry
    /// served.
    #[tokio::test]
    async fn an_existing_image_writes_its_pinned_ref() {
        let reg = MockRegistry::start(None);
        let digest = reg.put("acme/app", "1", &oci_manifest());
        let (sbx, res) = run(&format!("{}/acme/app:1", reg.host()), vec![], 0).await;
        res.expect("exists");
        let out = std::fs::read_to_string(sbx.pkg.join("exists.ref")).expect("output");
        assert_eq!(out, format!("{}/acme/app@{digest}", reg.host()));
    }

    /// Missing fails, and says it was the registry's answer.
    #[tokio::test]
    async fn a_missing_image_fails_naming_it() {
        let reg = MockRegistry::start(None);
        reg.put("acme/app", "1", &oci_manifest());
        for missing in ["acme/app:2", "acme/other:1"] {
            let reference = format!("{}/{missing}", reg.host());
            let (sbx, res) = run(&reference, vec![], 0).await;
            let err = format!("{:#}", res.err().expect("missing"));
            assert!(
                err.contains(&format!("{reference} does not exist")),
                "{err}"
            );
            assert!(err.contains("anonymous"), "{err}");
            assert!(!sbx.pkg.join("exists.ref").exists());
        }
    }

    /// A declared credential reaches the registry, and the same registry
    /// refusing the anonymous request is an error, not "missing".
    #[tokio::test]
    async fn a_declared_credential_authenticates_and_a_refusal_is_not_missing() {
        let reg = MockRegistry::start(Some(crate::pluginoci::registry::testreg::BOT_BASIC));
        reg.put("acme/app", "1", &oci_manifest());
        let reference = format!("{}/acme/app:1", reg.host());

        let (_sbx, res) = run(&reference, vec![], 0).await;
        let err = format!("{:#}", res.err().expect("anonymous is refused"));
        assert!(
            !err.contains("does not exist"),
            "a 401 is not missing: {err}"
        );
        assert!(err.contains("anonymous"), "{err}");

        let p = crate::pluginoci::auth::tests::Presented::new(
            "//auth:c0",
            &[&reg.host()],
            crate::pluginoci::registry::testreg::BOT_ANSWER,
        );
        let (sbx, res) = run(&reference, vec![p.mount.clone()], 1).await;
        res.expect("authenticated lookup");
        assert!(sbx.pkg.join("exists.ref").exists());
        assert!(
            reg.authorizations()
                .iter()
                .any(|a| a == crate::pluginoci::registry::testreg::BOT_BASIC),
            "{:?}",
            reg.authorizations()
        );
    }

    /// A target that declared credentials and received none is refused rather
    /// than looking up anonymously.
    #[tokio::test]
    async fn declared_credentials_that_did_not_arrive_are_refused() {
        let reg = MockRegistry::start(None);
        reg.put("acme/app", "1", &oci_manifest());
        let (_sbx, res) = run(&format!("{}/acme/app:1", reg.host()), vec![], 1).await;
        res.err().expect("a lost mount must fail");
        assert!(reg.requests().is_empty(), "{:?}", reg.requests());
    }
}
